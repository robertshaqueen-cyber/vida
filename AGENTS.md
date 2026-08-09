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
- CLI 的人类可读输出必须跟随 GUI 保存的语言选择；`--json` 的字段名、枚举值和
  envelope 必须保持稳定，不得随界面语言变化；终端原始内容永不翻译

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
- **`app.hosts` 只能通过 `set_hosts()` 修改**（app.rs 中 VidaApp 的方法）。
  `set_hosts` 会同时重建主机标签页标题；绕过它直接写 `app.hosts`
  会导致 tab 标题与内容漂移（已发生：同步下载后 tab 名不更新）

## GUI 交互路径的验证

daemon 层测试无法覆盖「按钮是否真的发出了请求」——测试直接调用
`state.sync()`，绕过了 GUI 消息层。已发生的真实案例：76 个测试全部
通过，而「点击同步按钮」这个最基本的路径是坏的。

- 任何新增的用户可触发操作（按钮、菜单项、快捷键），
  必须在 README 的手动验收清单中增加一条对应的检查项，
  描述为「点击 X → 预期看到 Y」，由项目所有者实际操作确认
- 状态类 UI（如同步状态图标）只能由服务端响应更新，
  不得依据 GUI 本地推测决定是否发送请求或显示成功

## GUI 视觉一致性（强制）

- 新增或修改按钮、输入框、下拉框、弹出菜单、复选框、卡片等可见控件时，
  **禁止直接使用 iced 默认样式**；必须复用 `crates/vida-gui/src/ui.rs` 中的共享样式。
- 同类控件的高度、padding、圆角、边框、颜色、字体，以及 hover、focus、opened、
  selected、disabled 等状态必须与现有界面一致。实现前先对照项目中最近的同类控件。
- iced 将下拉框本体和弹出菜单分开着色；使用 `ui::picker` 时必须同时使用
  `ui::picker_menu`，不得只处理关闭状态。
- 如果共享视觉系统缺少所需状态，应先在 `ui.rs` 补齐共享实现，再由各 Screen 复用；
  禁止在单个 Screen 内复制一套近似颜色或尺寸。
- 可见界面变更除自动测试外，还必须在 README 手动验收清单中加入对应操作和预期视觉状态。

## 终端输入与 resize 约定

- 人的普通键盘输入走 `SessionInput`，daemon 必须保持原始字节透传；
  不做命令解释、换行转换，也不让 AI 进入人的输入路径
- 剪贴板必须走独立的 `PasteSession`，由 daemon 查询真实
  `TermMode::BRACKETED_PASTE`；禁止把多行粘贴直接当普通输入，否则会立即执行
- 输入与 resize 在 GUI update 内按事件顺序进入同一发送队列；禁止“一键一个异步
  Task”，异步调度可能打乱字符顺序
- resize 行列数必须使用渲染器实测的物理像素 cell 尺寸计算：
  `floor(logical bounds × scale / physical cell)`；不得混用逻辑像素与物理像素
- 键盘、IME、剪贴板内容可能包含口令或私钥，永不写日志

## CLI / MCP 客户端约定

- GUI、正式 CLI 与 MCP 桥必须复用 `vida-client`；禁止各自复制 daemon 端口/token
  发现、WebSocket 认证、请求关联或错误解码。
- `vidactl --json` 的成功/错误 envelope 与退出码属于稳定脚本接口；字段变更必须同步
  测试和 README。机器输出不得混入日志或人类提示。
- 主机与会话只读命令禁止调用 `RevealCredential`，任何输出都不得包含密码、私钥内容或
  私钥口令。
- Agent 写入策略、危险命令审批与审计必须在 daemon 侧统一执行；不得只在 CLI 或 MCP
  表面隐藏命令，因为持有 daemon token 的客户端可以直接请求底层协议。

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
