# AGENTS.md — vida 项目约定

## 项目结构

```
vida/
├── crates/
│   ├── vida-core/     # 核心逻辑：金库、配置、主机档案
│   ├── vida-daemon/   # 守护进程：PTY、SSH、MCP 服务端、WebSocket
│   ├── vida-gui/      # GUI：iced + wgpu 终端渲染、侧边栏、标签页
│   └── vida-mcp-cli/  # stdio→WebSocket 桥接（给只支持 stdio 的 MCP 客户端）
```

## 里程碑流程

1. 完成一个 M 后停下，输出验收清单，等所有者确认
2. 禁止一次性写完多个里程碑
3. 每个里程碑一个 git 分支，中文提交信息
4. 每个里程碑报告内存实测数字

## 代码规范

- Rust 2024 edition
- 引入新 crate 前先说明理由
- 错误信息说人话：发生了什么 + 可能原因 + 建议下一步
- 密码与私钥内容永不写入日志
- 每个工具的 description 必须写明「什么时候用、什么时候不要用」

## 设计铁律

1. AI 永不进入人的输入路径
2. 写入仲裁：人永远优先
3. GUI 是 daemon 的客户端（WebSocket 通信，不直接调用）
4. 金库格式必须可用标准 age CLI 解密

## GUI 数据分层约定

**业务数据存 VidaApp，Screen 只存该屏幕自己的 UI 状态。**

- 主机列表（hosts）、tabs、同步状态等业务数据在 `VidaApp` 字段上，
  任何屏幕（冲突、备份等）都可读取，不得从 `Screen::Main` 内部取数
- 例如主机详情视图要展示的凭据显示状态（revealed_credential）属于
  Main 屏幕的 UI 状态，留在 `s3_main::State`
- 违反此约定会导致：非 Main 屏幕触发同步时冲突界面拿不到本地列表

## GUI 交互路径的验证

daemon 层测试无法覆盖「按钮是否真的发出了请求」——测试直接调用
`state.sync()`，绕过了 GUI 消息层。已发生的真实案例：76 个测试全部
通过，而「点击同步按钮」这个最基本的路径是坏的。

- 任何新增的用户可触发操作（按钮、菜单项、快捷键），
  必须在 README 的手动验收清单中增加一条对应的检查项，
  描述为「点击 X → 预期看到 Y」，由项目所有者实际操作确认
- 状态类 UI（如同步状态图标）只能由服务端响应更新，
  不得依据 GUI 本地推测决定是否发送请求或显示成功

## 内存测量规范

- **唯一指标**：`vmmap --summary <pid>` 中的 `Physical footprint`
- **禁止**用 RSS（包含共享库，不代表实际占用）
- **禁止**用 `ps -o rss` 作为验收依据
- 每个里程碑报告 footprint，注明显示器 scale factor
- Retina 推算公式：`footprint ≈ 固定开销(~24M) + IOSurface(物理像素×4)`
- Cell 内存增量：用 `std::mem::size_of` 打印，报告 5000行×200列 满载实测增量

## 金库格式变更规则（强制）

任何对 Vault / HostEntry / AuthMethod / Settings 的字段增删改名或
enum variant 形态变更，**必须在同一个提交内完成以下三件事**：

1. **CURRENT_VAULT_VERSION +1**（vault.rs 常量）
2. **增加迁移函数**并接入 `migrate_json` 迁移链（JSON 层面操作）
3. **增加测试**：构造上一版本的真实 JSON，验证迁移后字段完整

三件事缺一不可。漏掉任何一步都会导致用户金库打不开。

**版本快照测试** `version_snapshot`：序列化完整填充的 Vault，
与当前版本号断言比对。结构一改此测试即失败，强制执行上述三步。

## 跨层未完成契约

### daemon 层（已完成）
| 契约 | 状态 |
|---|---|
| Downloaded 写入+替换内存+更新 SyncState | ✅ 已实现+测试 |
| Conflict + resolve 替换内存 | ✅ 已实现+测试 |
| ConflictFilesDetected 返回文件列表 | ✅ 已实现+测试 |
| RemoteMissing + 两个处置方向 | ✅ 已实现+测试 |

### GUI 层（已完成）
| 契约 | GUI 必须执行的动作 | 状态 |
|---|---|---|
| Downloaded / Conflict resolve | 收到 SyncResponse 后用其中的 hosts 刷新列表 | ✅ app.rs Downloaded handler 读取 val.get("hosts") 并更新列表 |
| ConflictFilesDetected | 提示发现冲突文件，允许查看内容并选择采纳/忽略 | ✅ S7ConflictFileScreen 展示文件列表+pattern |
| RemoteMissing | 提示远端文件缺失，提供「重新上传 / 清除同步状态」两个选项 | ✅ S8RemoteMissingScreen 两个按钮：handle_reupload / handle_clear_state |

**本节已全部清空。M1 不再被跨层契约阻塞。**

## iced 版本锁定（强制）

`vida-gui/Cargo.toml` 中 iced 版本锁定为精确版本（`= 0.14.0`）。

**原因**：`SecureTextInput` 依赖 iced 三条未在文档中承诺的内部行为：
1. IME 启用时事件为 `Event::InputMethod` 而非 `Event::Keyboard`
2. 键盘事件仅到达 focused widget
3. `shell.input_method()` 前后差分可推断内部 widget 是否 focused

**升级 iced 前必须**：
1. 阅读 iced changelog，检查上述三条是否有变动
2. 执行 IME 手动验证清单（见下）
3. 验证通过后才可提升版本

**IME 手动验证清单**（中文输入法下执行）：
- [ ] S2 密码框：无候选窗，直接输入 ASCII
- [ ] S1 口令框：同上
- [ ] S4 口令框：同上
- [ ] S4 主机名称框：中文输入正常，候选窗出现
- [ ] S4 备注框：同上
- [ ] S9 备份口令框：同上
- [ ] S4 同屏：口令框无候选窗 + 主机名称框中文正常（同时成立）
- [ ] Cmd+V 粘贴含中文字符串到六个 secure 框：成功
- [ ] 空闲 CPU ≈ 0%（无常驻订阅）
