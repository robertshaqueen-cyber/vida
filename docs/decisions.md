# 技术决策记录

## M0: 工作区结构与渲染方案

**决策**: 4-crate workspace (vida-core, vida-daemon, vida-gui, vida-mcp-cli)

**理由**: 
- 核心逻辑与渲染分离，daemon 可独立运行
- iced 仅用于 chrome（侧边栏/标签栏/对话框），终端 pane 用 wgpu 直接渲染
- mcp-cli 是 stdio→WebSocket 桥接，给不支持 HTTP 的 MCP 客户端用

**日期**: 2026-08-01

## M0: WebSocket 通信

**决策**: daemon 监听 127.0.0.1 随机端口，GUI 通过 WebSocket 连接

**理由**:
- 即使同机也走 WebSocket，未来支持远程 daemon 时只需改配置
- tungstenite/tokio-tungstenite 是 Rust 生态最成熟的 WebSocket 库

**日期**: 2026-08-01

## M0: GUI 框架选择 iced

**决策**: iced 0.14 作为 GUI 框架

**理由**:
- 能做出 Zed/Linear 那种利落的工程感
- 内置 wgpu 支持，可与终端渲染共享 GPU 上下文
- 跨平台，未来扩展方便
- 所有者接受风格差异

**日期**: 2026-08-01

## M0: 内存指标与测量方法

**决策**: 唯一指标为 `vmmap --summary` 的 `Physical footprint`，不用 RSS。

**理由**:
- RSS 包含共享库的只读映射（~60MB），不代表进程实际占用
- 活动监视器的「内存」列显示的是 footprint
- macOS 文档明确区分 footprint（独占）和 RSS（含共享）

**实测数据**:
- wgpu 裸空窗口 footprint: 24.9M (小) ~ 32.1M (全屏)
- iced chrome 增量: ~6M
- 字体加载: 0K dirty（macOS Core Text 内存映射）
- 固定开销: ~24M（GPU 设备 + 驱动 + 堆结构）
- 帧缓冲: 受显示器物理分辨率限制

**日期**: 2026-08-01

## M1: 加密算法 — scrypt（非 Argon2id）

**决策**: 口令加密使用 age 规范的 scrypt recipient，不用 Argon2id。

**理由**:
- age 规范中基于口令的加密只有 scrypt recipient stanza
- Argon2id 不在 age 规范内，用它会导致标准 `age` CLI 无法解密
- 设计铁律 2.4 要求金库可用标准 age CLI 解密（逃生舱）
- `age` crate 0.12 的 `Encryptor::with_user_passphrase` 自动使用 scrypt

**Work factor（实测）**:
- 设置：`recipient.set_work_factor(18)` → log_n = 18, N = 262144, r = 8, p = 1
- 与 `age` CLI 默认加密 work factor 一致
- age header: `-> scrypt <salt_b64> 18`
- **解密耗时实测**（`age` CLI 1.3.1, 10 次取中位）：**0.362 秒**
- 对比 log_n=14（auto-tuned）：0.043s → 0.362s，强度提升 16 倍
- 内存：scrypt 解密瞬时占用 ~256MB（预期行为，非泄漏）

**依赖变更**:
- 之前：无加密依赖（占位符）
- 现在：`age = "0.12"`, `secrecy = "0.10"`
- `rage` 是 CLI 工具，不是库；库用 `age` crate

**日期**: 2026-08-01

## M2: 终端解析 — alacritty_terminal（非 vte+vt100）

**决策**: 使用 `alacritty_terminal` 0.26.0 作为终端状态机和 grid 后端。

**理由**:
- 规格书指定 alacritty_terminal（Alacritty 的 grid + VTE 状态机）
- vte + vt100 同时引入是重复的（vt100 内部就是 vte）
- alacritty_terminal 被 Zed 使用，仿真完整度远高于 vte+vt100
- 自研 VT 解析是「出错了所有者无法描述、只能说看起来怪怪的」的模块

**API 验证**（2026-08-01）:

- **版本**: `alacritty_terminal = "0.26"`（0.24 有 rustix 兼容性问题）
- **Cell 类型**: `alacritty_terminal::term::cell::Cell`
  ```rust
  pub struct Cell {
      pub c: char,              // 4 bytes
      pub fg: vte::ansi::Color, // enum
      pub bg: vte::ansi::Color, // enum
      pub flags: Flags,         // bitflags
      pub extra: Option<Arc<CellExtra>>,  // 8 bytes (64-bit)
  }
  ```
  `size_of::<Cell>() = 24 bytes`
- **Term**: `Term<T: EventListener>`，泛型参数 T 是事件代理
  - `VoidListener` 提供空实现（unit struct）
  - `Term::new(config, dimensions, event_proxy)` 创建实例
  - `term.grid()` → `&Grid<Cell>`，`term.grid_mut()` → `&mut Grid<Cell>`
  - `term.resize(size)` 调整大小
- **Grid**: `Grid<Cell>` 支持 `grid[Point]` 索引，`grid.display_iter()` 遍历可见 cell
- **VTE 集成**: `vte::Parser` + 实现 `vte::Perform` trait，调用 `Handler` 方法写入 Term
- **内存**: 5000 行 × 200 列 = 1,000,000 cells × 24 bytes = **22.89 MB**

**日期**: 2026-08-01
