# 设计决策记录

## IME 安全缺陷分析 (2026-08-03)

**Issue**: https://github.com/robertshaqueen-cyber/vida/issues/2

### 问题描述

在 macOS 上使用中文输入法时，密码输入框（`.secure(true)`）仍然显示输入法候选框。
这意味着输入法正在拦截按键，字符进入输入法缓冲区而非密码框。
云输入法（搜狗、百度等）会将输入内容上传至厂商服务器，导致主口令存在离开本机的风险。

### 技术调查结果

#### 1. InputMethod::Disabled 的定义与用法

```rust
// iced_core-0.14.0/src/input_method.rs
pub enum InputMethod<T = String> {
    /// Input method is disabled.
    Disabled,
    /// Input method is enabled.
    Enabled {
        cursor: Rectangle,
        purpose: Purpose,
        preedit: Option<Preedit<T>>,
    },
}
```

- `Disabled`: 完全禁用 IME，窗口调用 `set_ime_allowed(false)`
- `Enabled`: 启用 IME，可设置 purpose (Normal/Secure/Terminal)

#### 2. text_input .secure(true) 的行为

```rust
// iced_widget-0.14.2/src/text_input.rs
fn input_method(...) -> InputMethod<&'b str> {
    // ...
    let secure_value = self.is_secure.then(|| value.secure());
    let value = secure_value.as_ref().unwrap_or(value);
    // ...
    InputMethod::Enabled {
        cursor: ...,
        purpose: if self.is_secure {
            input_method::Purpose::Secure
        } else {
            input_method::Purpose::Normal
        },
        preedit: ...,
    }
}
```

**关键发现**: 即使 `secure(true)`，text_input 仍然返回 `InputMethod::Enabled`，
只是 purpose 设为 `Secure`。这导致 IME 仍然被启用。

#### 3. iced 是否提供应用层关闭 IME 的途径

- `Shell::request_input_method(&mut self, ime: &InputMethod)` - widget 可以调用
- 但 `merge()` 方法只在当前状态为 `Disabled` 时才会更新
- 一旦 IME 被启用，widget 无法主动将其关闭

#### 4. winit 的 set_ime_allowed(false) 能否被间接触发

可以。当 widget 返回 `InputMethod::Disabled` 时：
```rust
// iced_winit-0.14.0/src/window.rs
pub fn request_input_method(&mut self, input_method: InputMethod) {
    match input_method {
        InputMethod::Disabled => {
            self.disable_ime();  // 调用 set_ime_allowed(false)
        }
        InputMethod::Enabled { ... } => {
            self.enable_ime(...);  // 调用 set_ime_allowed(true)
        }
    }
}
```

### 影响范围

所有 `.secure(true)` 调用点：

| 文件 | 行号 | 用途 |
|------|------|------|
| s1_setup.rs | 33 | 创建金库 - 口令输入 |
| s1_setup.rs | 40 | 创建金库 - 确认口令 |
| s2_unlock.rs | 49 | 解锁金库 - 口令输入 (error 分支) |
| s2_unlock.rs | 59 | 解锁金库 - 口令输入 (normal 分支) |
| s4_credential.rs | 113 | 主机编辑 - 口令输入 |
| s9_backup.rs | 37 | 导出备份 - 口令输入 |

### 可选方案

#### 方案 A: Patch iced 的 text_input (推荐)

修改 text_input 的 `input_method()` 方法，当 `is_secure` 为 true 时返回 `InputMethod::Disabled`：

```rust
// 修改前
InputMethod::Enabled {
    purpose: if self.is_secure {
        input_method::Purpose::Secure
    } else {
        input_method::Purpose::Normal
    },
    ...
}

// 修改后
if self.is_secure {
    InputMethod::Disabled
} else {
    InputMethod::Enabled { purpose: Purpose::Normal, ... }
}
```

**优点**: 最简单，一行代码修复
**缺点**: 需要维护 iced fork

#### 方案 B: 自定义 widget 包装

创建 `SecureTextInput` widget，包装 `text_input` 并在 `update()` 方法中
处理 IME 事件，拒绝所有 IME commit。

**优点**: 不需要 patch iced
**缺点**: 实现复杂，需要处理所有键盘事件，可能遗漏

#### 方案 C: NSTextField FFI (不采用)

用户明确不采用此方案，因为：
- 原生 NSView 叠在 wgpu 窗口上需要处理布局、缩放
- 主题跟随困难
- 成本与脆弱度过高

### 临时规避

在修复完成前，用户应：
1. 输入主口令前切换到英文输入法
2. SECUIRTY.md 已添加此警告

### 下一步

1. 向 iced 上游提交 PR，让 secure field 返回 `InputMethod::Disabled`
2. 同时在本地使用 cargo patch 指向修复后的版本
3. 修复完成后更新 SECURITY.md

---

## IME 修复实施 (2026-08-03)

### 实际方案: 自定义 widget 包装 (方案 B)

选择了 wrapper widget 而非 fork iced，理由：
- 避免维护 fork 的长期成本
- ~170 行代码，无需修改 iced 源码
- 通过 `cargo advanced` feature flag 启用 `iced::advanced` 模块

### Shell::input_method_mut() 的仲裁问题

`Shell::merge()` 的逻辑：`Enabled` 优先于 `Disabled`。一旦任何 widget
请求 `Enabled`，其他 widget 通过 `merge()` 无法将其关闭。

因此 `input_method_mut()`（直接赋值）是唯一能为 secure 字段强制关闭
IME 的方式。`request_input_method()` 无法覆盖已启用的状态。

**为什么不能无条件写入**：

RedrawRequested 事件到达所有可见 text_input。若非 secure 字段调用
`request_input_method(Enabled)`，merge 使 `input_method = Enabled`。
secure 字段的 `Disabled` 写入被 merge 忽略（Enabled 优先）。无条件
写入会覆盖非 secure 字段的请求，导致同屏所有非 secure 字段无法输入
中文（S4 主机名、备注等）。

**最终方案：拦截 IME + 检测焦点 + Keyboard 禁用**

三层防御：

1. **Ime::Preedit / Ime::Commit**：直接 return，不传递给 inner text_input。
   阻止中文字符被提交到 secure 字段。同时写入 Disabled。

2. **RedrawRequested**：对比 inner update 前后的 shell.input_method 状态。
   如果 inner text_input 调用了 request_input_method（仅 focused 时发生），
   shell 状态会改变。检测到变化 → 写入 Disabled。未变化 → 不写入（不覆盖
   兄弟 widget 的 IME 请求）。

3. **Keyboard 事件**：无条件写入 Disabled。Keyboard 事件只到达 focused widget，
   不影响兄弟。防止 IME 在下次 RedrawRequested 时被重新激活。

**焦点检测原理**：text_input 仅在 focused + window focused 时调用
request_input_method（line 1353）。对比 update 前后的 shell 状态，
变化 = focused，未变化 = not focused。无需访问 inner widget 的私有状态。

### classify_error 过渡方案

当前通过字符串匹配错误消息来分类（wrong_passphrase / vault_corrupted）。
这是过渡方案，存在误报风险（如 "invalid" 过宽）。

**已收窄**: 仅匹配 core 层实际产出的稳定文案：
- wrong_passphrase: "wrong passphrase", "auth failed", "hmac mismatch"
- vault_corrupted: "corrupted data", "failed to parse vault json"

**目标**: 在错误产生位置直接携带分类（VidaError { category, detail }），
下游不再做文本匹配。

---

## 抖动动画 (改版 3) — 放弃

iced 0.14 没有 CSS-like 的 keyframe 动画机制。实现抖动需要：
1. 创建 `Subscription` 驱动定时器
2. 在 N 帧内交替偏移 x 坐标
3. 动画结束后取消订阅

**放弃原因**: 复杂度与收益不成比例。错误提示已通过红色边框 + toast
充分传达，抖动是锦上添花。若未来 iced 支持声明式动画，可重新考虑。

**硬性要求**: 若实现，驱动订阅只能在动画期间存在，做完报告空闲 CPU ≈ 0%。

---

## 同步假成功案 — GUI 本地推测状态构成死锁 (2026-08-05)

**症状**: 76 个测试全部通过，但「点击同步按钮」这个最基本的路径是坏的。
A 设备显示 ✓（假成功），B 设备点击跳转设置（假跳转），daemon 日志中
Sync 请求从未到达。

### 三个叠加的 bug

1. **按钮依据本地状态决定是否发请求**（死锁）：
   `view_tab_bar` 检测 `sync_state == NotConfigured`（本地推断）后，
   把按钮的 `on_press` 替换成 `OpenSettingsTab`。sync_state 初始为
   Unknown/NotConfigured，GUI 不请求 → 状态永远不更新 → 按钮永远
   不发请求。A 与 B 显示不一致是因为 sync_state 到达该状态的本地
   路径不同，而非配置差异。

2. **本地写入假成功**：`DeleteHost` / `EditorSaved` 直接把 sync_state
   设为 `LocalChanges`；而 `SyncCompleted` 的 fallback 分支（`_`）把
   任何未识别状态设为 `Synced`。任何一条路径都能让界面显示 ✓ 而
   daemon 一无所知。

3. **测试绕过 GUI 消息层**：所有测试直接调用 `state.sync()`，
   「按钮是否真的发出了请求」从未被验证。

### 修正原则（已写入 AGENTS.md）

- 同步按钮永远发送 Sync 请求，由 daemon 返回真实状态
- `sync_state` 只能由服务端响应（SyncCompleted / WsError）更新，
  删除本地推测写入（`LocalChanges` 变体整个移除）
- 新增端到端测试 `sync_end_to_end_config_to_upload`：
  update_settings 配置路径 → 添加主机 → state.sync() → 断言远端
  文件存在且解密后包含该主机

### 可复用的教训

- **GUI 依据本地推测状态决定是否发送请求 = 死锁**。
  状态类 UI 只能由服务端响应更新，不得本地推测。
- **报告成功但什么都没做**（假 ✓）与「报告已实测但无人验证」同源：
  都需要一个从用户操作到真实结果的完整路径测试。

---

## 同步远端文件时间线疑案 (2026-08-05)

**症状**: `/tmp/vida-sync/vault.age` 的创建时间（18:58）晚于两台
daemon 的最后一条日志（18:57），而 `init_sync()` 与
`update_settings()` 均被确认只读、不会写入远端文件。文件来源无法
从日志中定位——因为当时的同步路径完全没有日志。

**处置**: 同步功能现已正常，不再追查。补上日志使同类情况可定位：
- `LocalPathBackend::upload` 成功后 `info!`，记录目标路径与字节数
- `LocalPathBackend::download` 成功后 `info!`，记录字节数
- `SyncCoordinator::sync` 进入判定分支前 `debug!`，记录
  `remote_exists / remote_changed / local_changed` 三个布尔值

**教训**: 任何会改动远端文件的操作都必须有日志。没有日志的
文件变动 = 无法归因的谜团，调试成本远高于补日志的成本。

---

## M1 跨层契约闭环 (2026-08-05)

M1 跨层契约于 2026-08-05 全部闭环，实测通过场景 5/6：
- daemon 层 4 条：Downloaded 写入+替换内存+更新 SyncState、
  Conflict + resolve 替换内存、ConflictFilesDetected 返回列表、
  RemoteMissing + 两个处置方向 —— 均已有实现+测试
- GUI 层 3 条：Downloaded/Conflict resolve 后刷新列表、S7 冲突文件
  采纳/忽略、S8 远端缺失两个处置按钮 —— 均已在界面实现
- 场景 5 实测：双设备模拟，Downloaded 分支刷新 + 持久化 + 无重复下载
- 场景 6 实测：冲突界面两侧列表正确；resolve_conflict_remote 后编辑
  保存，本地旧数据未被写回（内存替换真实生效）

AGENTS.md 中「跨层未完成契约」一节已删除（不再有未完成项）。
M1 关闭。

---

## M2a-0 alacritty_terminal 0.26 API 实测 (2026-08-05)

> 通过 `crates/vida-daemon/examples/term_probe.rs` 编译运行实测，
> 不是读文档的推断。运行：`cargo run -p vida-daemon --example term_probe`。

### 依赖

```toml
alacritty_terminal = "0.26.0"
```
`alacritty_terminal::vte` 是 re-export（lib.rs: `pub use vte;`），
无需单独引入 vte crate。vte 0.15.0 随 0.26.0 一起锁定。

### Term 构造

```rust
// 实际签名（term/mod.rs:410）
pub fn new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T>

// 用法
use alacritty_terminal::event::VoidListener;
let mut term: Term<VoidListener> = Term::new(config, &size, VoidListener);
```

- `Config`：`term::Config`，公开字段 `scrolling_history: usize`
  （**默认 10000**，注意与金库默认 5000 不同，接入时须显式设置）。
  其他字段：default_cursor_style / vi_mode_cursor_style /
  semantic_escape_chars / kitty_keyboard / osc52，均取默认即可。
- `Dimensions` trait（grid/mod.rs:486）：需实现 `total_lines()` /
  `screen_lines()` / `columns()`，其余有默认实现。探针自定义了
  `ProbeSize { cols, rows }` 结构体实现。
- listener：`event::VoidListener`（event.rs:108），空实现。

### Processor

```rust
// vte 0.15.0 / ansi.rs:271
pub struct Processor<T: Timeout = StdSyncHandler> { ... }

// 用法（注意：有默认类型参数但仍需显式标注，否则 E0283）
use alacritty_terminal::vte::ansi::Processor;
let mut processor: Processor<alacritty_terminal::vte::ansi::StdSyncHandler> =
    Processor::new();
processor.advance(&mut term, bytes);  // vte 0.15 lib.rs:109
```

`advance<P: Perform>(&mut self, performer: &mut P, bytes: &[u8])`。
`Term<T>` 实现了 `Handler`（term/mod.rs:1059），`Handler: Perform`，
所以 `processor.advance(&mut term, bytes)` 直接可用。

### grid 遍历与 Cell

```rust
use alacritty_terminal::grid::Dimensions;  // 提供 last_column() 等

// 视口遍历（grid/mod.rs:422）—— 从历史顶部到屏幕底部，跳过滚动回看
for indexed in term.grid().display_iter() {
    let point = indexed.point;   // Point<Line, Column>
    let cell: &Cell = indexed.cell;
    print!("{}", cell.c);
}

// Cell（term/cell.rs:134）
pub struct Cell {
    pub c: char,          // 主字符（默认 ' '）
    pub fg: Color,        // 默认 Named(Foreground)
    pub bg: Color,        // 默认 Named(Background)
    pub flags: Flags,     // bitflags，见下
    pub extra: Option<Arc<CellExtra>>,  // zerowidth 字符、hyperlink
}
```

- `Flags`（term/cell.rs:15）：BOLD / ITALIC / UNDERLINE / WRAPLINE /
  WIDE_CHAR / WIDE_CHAR_SPACER / DIM / HIDDEN / STRIKEOUT /
  LEADING_WIDE_CHAR_SPACER / DOUBLE_UNDERLINE / UNDERCURL / DOTTED /
  DASHED。**宽字符用 `Flags::WIDE_CHAR` 标记，第二个 cell 是
  `WIDE_CHAR_SPACER`** —— 推送协议须带上 wide 标志（M2a 陷阱 #8）。
- `Color`（vte 0.15 ansi.rs:1128）：

```rust
pub enum Color {
    Named(NamedColor),   // 16 色语义名（Foreground/Background/Red/...）
    Spec(Rgb),           // 24 位真彩色
    Indexed(u8),         // 256 色索引
}
```

- **实测 size**：`size_of::<Cell>() = 24`，`size_of::<Color>() = 4`，
  `size_of::<Colors>() = 1076`（Colors 是 16 色表，推送无需带）。

### 光标

```rust
let cursor = term.grid().cursor.point;   // grid/mod.rs:113 pub cursor
// Point { line: Line(i32), column: Column(usize) }
// 行号从 0 开始，line.0 可能为负（显示偏移时）
```

### 增量脏行接口：存在（damage）

**结论：alacritty_terminal 自带 damage 接口，M2a-2 优先使用，不做全量 diff。**

```rust
// term/mod.rs:458
#[must_use]
pub fn damage(&mut self) -> TermDamage<'_>;
pub fn reset_damage(&mut self);   // 读完后必须调用

pub enum TermDamage<'a> {
    Full,                                  // 整个终端损坏（初始状态/resize/插入模式）
    Partial(TermDamageIterator<'a>),       // 脏行迭代器
}
// TermDamageIterator 的 Item = LineDamageBounds { line: usize, left: usize, right: usize }
```

**实测行为**：
- 构造后第一次 `damage()` 返回 `Full`（初始即全损坏）
- `reset_damage()` 后再 `damage()` 且无写入 → `Partial([])`（实际输出 `Partial([(3,19,19)])`，是旧光标点）
- 写入 "write on row 0\r\n" 后 → `Partial([(3, 0, 33), (4, 0, 0)])`：
  行 3 的 0..=33 列 + 新光标所在行 4 被精确标记。**行列范围就是
  增量推送的输入，无需自己做 diff。**

注意：`damage()` 是 `&mut self`，且 `TermDamage` 借用 term，
必须在一个作用域内迭代完（或 collect 后释放）再调用 `reset_damage()`。

### resize

```rust
// term/mod.rs:655
pub fn resize<S: Dimensions>(&mut self, size: S);
```
实测：80×24 → 40×12 正常收缩，无 panic。resize 同时作用于
grid、inactive_grid、damage 状态。dimensions 不变时提前返回。
M2a-2 中 PTY 侧 ioctl 与 Term::resize 必须成对调用（陷阱 #5）。

### 换行行为（探针意外发现）

`Term` 的 `linefeed`（term/mod.rs:1423）**只下移不回车**——
探针输入 `line three\nline four` 后 "line four" 落在第 3 行第 10 列
（继承 `line three` 的光标列）。这是 VT 标准语义（LF 与 CR 分离），
真实 shell 输出都是 `\r\n`，不会触发此问题。daemon 侧无需处理，
但推送协议按 cell 位置发送，天然不受影响。

### 滚动回看容量

`Config.scrolling_history`（默认 10000）。M2a-2 接入时须从金库
`Settings.scrollback_lines`（默认 3000，见结论 2）读取并显式设置。
`Term::resize` 不改变历史容量；`Term::new` 时由
`config.scrolling_history` 决定（term/mod.rs:414）。

### 对 M2a-2 推送协议的影响

1. **damage 直接提供脏行区间**：每帧
   `term.damage()` → 遍历 `LineDamageBounds` → 对每个脏行的
   left..=right 列做 RLE → 推送。不需要行哈希 diff。
2. `TermDamage::Full` 出现时（resize/初始/插入模式）推送整屏。
3. 光标从 `term.grid().cursor.point` 读取，每帧必推。
4. wide 标志直接来自 `Cell.flags`（WIDE_CHAR），协议 flags 字段
   已预留 wide 位（规格 5.4 的 flags: u8）。

### 结论 1：damage 提供「行 + 列范围」，推送协议按列区间发送

检查点 A 实测：`Partial([(5, 0, 25), (6, 0, 0)])` = (行号, 起始列, 结束列)。

推送协议据此调整——**不必发送整行，只发送 damage 报告的列区间**。
M2a-2 的帧格式改为：

```
[seq: u64 BE]                        // 单调递增，检测丢帧
[cursor_row: u16][cursor_col: u16][cursor_visible: u8]
[line_count: u16]
  每行：
    [row: u16][start_col: u16][end_col: u16][run_count: u16]
      每个 run：
        [run_len: u16]
        [flags: u8]                  // bold/italic/underline/reverse/... + wide
        [fg_tag: u8][fg payload]     // 0x00 默认(0字节) / 0x01 索引(1字节) / 0x02 RGB(3字节)
        [bg_tag: u8][bg payload]
        [char_len: u8][char bytes]   // UTF-8，run 内所有 cell 同字符时才合并
```

RLE 只覆盖 `start_col..=end_col` 区间。相比整行发送，yes/htop 这类
高频局部刷新场景的带宽显著降低。

**reset 时机**：每次推送完成后必须调用 `term.reset_damage()`，否则脏区
累积，下次推送会重复发送旧行。`damage()` 与 `reset_damage()` 是成对操作。

**要求测试**（M2a-2 实现时）：连续两次写入之间调用 `reset_damage()`，
断言第二次 `damage()` 不包含第一次写入的行。

### 结论 2：scrollback 默认值 5000 → 3000

`size_of::<Cell>() = 24` 实测。5000 行 × 200 列 × 24 字节 = 24 MB，
超出「每标签增量 < 20 MB」目标。Cell 内部布局已紧凑
（char 4 + fg 4 + bg 4 + flags 4 + Option<Arc> 8），无压缩空间。

**决定**：`Settings.scrollback_lines` 默认值从 5000 改为 **3000**。
3000 × 200 × 24 = 14.4 MB，留出余量给 Grid 的行索引开销。

- 属于 Settings 默认值变更，不改变结构，**不需要升 vault version**
- 设置页「回滚行数」默认显示同步更新，字段下方加说明：
  「每 1000 行约占用 5 MB 内存」
- docs/development.md 内存指标一节写明换算关系

### 结论 3：Config::scrolling_history 必须显式设置

`Config::scrolling_history` 默认 10000（alacritty 的默认值），
与金库默认 3000 不同。M2a-2 接入时**不得使用 `Config::default()`**，
必须显式设置为 `Settings.scrollback_lines`。

**要求测试**（M2a-2 实现时）：断言 Term 构造时使用的
`scrolling_history` 等于金库设置值。

### M2a-1 结论 4：UTF-8 跨读边界已由 vte 内部处理

**实测验证**（term_probe 第 8 节）：构造 `"你好世界"`（12 字节），
从字节 5 切开（"世" 的中间），分两次 `processor.advance()`，最终
grid 中四字完整（CJK 宽字符每字占 2 列，第二个 cell 是
WIDE_CHAR_SPACER 空格，过滤空格后还原原文）。断言通过。

**原理**：vte 0.15 `Parser` 内部维护 `partial_utf8: [u8; 4]` +
`partial_utf8_len`（lib.rs:68-69）。`advance` 遇到 buffer 尾部不完整
UTF-8（`error_len() == None` 且无 ESC 截断）时存入 partial 缓冲
（lib.rs:656），下次 `advance` 先补全（lib.rs:113/148）。

**结论**：**daemon 读线程无需缓存不完整字节**，直接整块喂
`Processor` 即可。跨读边界由 vte 状态机处理。

### M2a-1 结论 5：SessionInput 必须是原始字节透传

**约定**（M2a-2 及以后必须遵循）：

> SessionInput 不做行缓冲、不做换行转换、不解释任何按键。
> 客户端发什么就写什么到 PTY。

理由：daemon 侧若做行处理，M5 的 agent 将无法发送 Ctrl-C、
方向键、以及任何 TUI 交互所需的控制序列。

term_shell 已验证：raw mode 下 `\x03`（Ctrl+C）透传到 shell，
中断 `sleep 30` 后 `pwd` 立即可用（expect 实测）。

### M2a-1 结论 6：OpenLocalSession 固定 cwd = HOME

portable-pty 的 `spawn_command` 默认 cwd 是 HOME（探针实测）。
规格确认：**OpenLocalSession 不接受客户端指定 cwd，固定使用
HOME**（与固定 `$SHELL` 同理）。理由：M5 的 MCP 客户端会调用
此接口，cwd 不应成为可被 agent 操纵的输入。

term_shell 探针为了便于演示显式 `cmd.cwd(current_dir)`，daemon
接入时**不**继承，使用 HOME。

### M2a-1 结论 7：推送限频「合并后延迟发送」，禁止丢弃

term_shell 三轮评审发现：限频若施加在**处理**（advance）而不是
**绘制**上，窗口内的变化会被跳过——启动 5ms 时的提示符因 <16ms
被跳过，之后无新事件唤醒，屏幕永远空白。expect 测试恰好总在
发送字节（唤醒循环），因此两次都没发现。

**原则（M2a-2 推送限频同样适用）**：

> 帧率限制窗口内的多次变化必须「合并后延迟发送」，不能丢弃。
> 处理（Term 状态更新）不限频，只有发送/绘制限频。

实现模式：
```
收到 Ev::Output → 立即 processor.advance(&mut term, &bytes)  // 处理不限频
                   dirty = true
绘制/发送： if dirty && last >= 16ms { 发送; dirty = false }
             else 保留 dirty（合并，下次超时返回时补发）
主循环 recv 带超时（≤16ms），超时返回时若 dirty 则补一次发送
```

若窗口内变化被丢弃，一次突发输出的**最终状态**永远不会到达
客户端——与本次屏幕空白的现象完全相同，只是发生在网络一侧。
`yes` 场景下最终状态（堆积的 y）必须到达，只是可以晚到。

### M2a-1 结论 8：stdin 读取用 libc::read(fd 0)，不用 std::io::Stdin

**症状**：程序启动后立即退出（真实终端/重定向输入下）。expect 伪
TTY 下测试通过，因为 expect 的 stdin 是打开的、启动后立即有数据。

**根因**（eprintln 诊断确认）：stdin 即时 EOF（如 `/dev/null`）时
`std::io::Stdin::read` 立即返回 `Ok(0)` → 触发退出链：InputClosed →
关闭 PTY writer → shell EOF 退出 → OutputClosed → 程序退出。全程
几十毫秒，表现为「什么都没执行」。

**修复**：stdin 读线程改用 `libc::read(fd 0)`——纯 syscall，绕开
`std::io::Stdin` 内部的 BufReader 行缓冲（raw mode 下与行缓冲可能
冲突）。EINTR 重试。

### M2a-1 结论 9：Ev::InputClosed 不关闭 PTY writer

真实终端中 Ctrl+D 是把 `0x04` 字节传给 shell，由 shell 自行决定
是否退出；客户端不应替它关闭管道。

**规则**：InputClosed 仅记录日志，不动 writer。**程序唯一退出出口
是 Ctrl+] (0x1d)**。这同时消除了「stdin 线程异常 → 程序退出」的
整条副作用链——即使 stdin 通道异常结束，终端会话仍继续。

### M2a-1 结论 10：Ctrl+] 退出流程（优雅终止 + SIGKILL 兜底）

portable-pty 的 `Child::kill()` 发送的是 **SIGKILL**（不是 SIGHUP，
早期注释写错）。Ctrl+] 流程：

1. `writer.take()` —— 关闭写端，shell 收到 EOF **自行退出**（干净）
2. 等待 OutputClosed，**超时 2 秒**
3. 超时未退出（shell 卡在子进程）才 `child.kill()`（SIGKILL），
   打印 warn 说明是强制终止

实测：正常路径 shell EOF 退出 exit_code=0，kill 兜底未触发。

### M2a-1 结论 11：expect 无法覆盖真实 TTY 启动路径

term_shell 三轮评审中三类 bug（限频跳过、启动即退出）均因
**expect 伪 TTY 与真实终端差异**而漏测：expect 的 stdin 打开且
启动后立即有数据可读，永远碰不到「真实终端下启动瞬间 read 返回
0」或「无事件时积压不处理」的情况。

**规则**：交互式工具（term_shell 等）PR 描述必须明确写
「本工具的行为依赖真实 TTY，expect 测试无法覆盖启动路径，
需所有者在真实终端中确认」，并列出 expect 覆盖了什么、
没覆盖什么。

### M2a-1 结论 12：vim 默认无状态栏，TUI 验收判据选择

检查点 B 中 vim 启动画面「无状态栏」曾被误判为渲染缺陷，实为
vim 默认 `laststatus=1`（单窗口不显示状态栏）的正确行为，与
Term 渲染无关。

**验收 TUI 渲染时**：应挑选默认就有明显边框/状态行的程序
（htop、less、tmux），vim 的启动画面不适合作为唯一判据。

**另记录**：`crossterm::terminal::size()` 在伪 TTY（expect）下
可能返回 0×0，直接用于 `PtySize`/`Term` 会触发
alacritty grid 的 `columns() - 1` 下溢 panic（grid/mod.rs:499）。
非 tty / 0 尺寸均须降级 80×24。

### M2a-3 结论 13：read_screen_styled 的定位

**新增的 IPC 方法**：`ReadScreenStyled`，返回 `ScreenStyled`：
```json
{
  "rows": [[{"c":"h","fg":{"Rgb":[255,0,0]},"bg":"Default","flags":0}, ...], ...],
  "cols": 200,
  "cursor": {"row": 23, "col": 31}
}
```

**与 ScreenData 的关系**：并列，各有定位。

| 接口 | 返回 | 定位 |
|---|---|---|
| `ReadScreen` | `ScreenData { lines, wide_cols, cursor }` | 干净文本，M5 `screen_read` 用 |
| `ReadScreenStyled` | `ScreenStyled { rows(cells), cols, cursor }` | 带样式渲染，M2b GUI 用 |

**颜色表示**：`AnsiColor` 枚举（`Default` / `Indexed(u8)` / `Rgb(u8,u8,u8)`），
序列化为 serde 枚举格式。

**wide_cols**：`ReadScreenStyled` 不含 wide_cols —— 宽字符的 spacer cell
被跳过（不输出），但 `flags` 含 `WIDE` 位（0x40），客户端据此判断
该字符占两列。M5 `screen_read` 用 `ReadScreen`（含 wide_cols）。

**M5 用哪个**：`screen_read` 用 `ReadScreen`（干净文本 + wide_cols，
agent 可直接阅读文本并获知列对齐）。`ReadScreenStyled` 是 M2b
渲染层的接口，两者职责不重叠，不构成「两处保持一致」问题。

### M2a 教训 14：限频必须用绝对时隙，相对 sleep 会累积抖动

初版推送循环用 `thread::sleep(16ms)` 每轮无条件处理。macOS 的
sleep 返回抖动（5-26ms）会累积，实测出现 6ms、12ms 帧间隔——
每九帧就有一帧违反限频。

**正确做法**：绝对时隙。`next_slot` 单调 +16ms 推进，
`now < next_slot` 时 sleep 到点；处理耗时长于 interval 时防
burst 追赶（把时隙拉回当前时间）。

```rust
let mut next_slot = Instant::now();
loop {
    if Instant::now() < next_slot {
        thread::sleep(next_slot - Instant::now());
    }
    next_slot += interval;
    if next_slot + interval < Instant::now() {
        next_slot = Instant::now(); // 防 burst
    }
    // 读 damage → 编码 → 入队
}
```

实测：push_loop 内部间隔 15.16-18.59ms（多数恰 16.00ms）。

### M2a 教训 15：客户端观测的帧间隔不能作为限频判据

限频是否生效，必须在**生成侧**（push_loop）测量。客户端观测受
以下因素干扰，会产生「限频失效」的假象：

- **订阅首帧**：订阅时立即发全量快照，客户端循环第一条 interval
  记为 0ms（无前驱）
- **通道缓冲**：有界通道积压时客户端一次性读到多帧
- **传输层**：WebSocket/TCP 缓冲导致到达时间聚集

正确判据：
- **生成侧**：push_loop 内部相邻帧间隔（本次用诊断日志实测）
- **seq 跨度**：1 秒内 seq 增量 ≤ 65（有界通道丢帧不影响此判据）

watch 工具已改进：跳过首帧间隔、微秒精度、`--duration` 自动退出
并打印统计（帧数/字节/平均间隔/低于 16ms 占比）。

### M2b-0 结论 16：字体度量与逐 cell 定位（2026-08-07）

**实测字体度量**（cosmic-text 0.15，Monospace 族，SF Mono/Menlo 回退，
字号 16px，行高 20px）：

| 指标 | 实测值 | 测量方式 |
|---|---|---|
| `cell_width` | **9.60 px** | 排单个 'M'，取 layout `line_w`（移除位图字体后主字体为 Menlo） |
| `cell_height` | **20.0 px** | Metrics 的 line_height（ascent+descent+line_gap 由字体计算） |
| `ascent` | **14.26 px** | `line_y - line_top`（主字体 Menlo 的基线到行顶距离），全局统一基线 |

**统一基线**（M2b-0 必改 3）：所有字符（含 fallback 字体）用主字体 ascent 对齐：
`glyph_y = row_y + ascent - placement.top`。不用各字体自己的度量——
中英文基线不一致是「中文偏下/abc 上标」的成因。

**反色**（M2b-0 必改 2）：前景/背景对调，背景 quad 条件为
`bg.is_some() || reverse`，reverse 时背景取 run 前景色。

**宽字符判定**（M2b-0 必改 1）：用 unicode-width crate 按 East Asian Width
判定（`U+00FF` 以上不算宽，é ü α β → ✓ ● 都是单列）。
这是 example 临时方案——M2b-1 后宽字符由 daemon 推送协议 flags 的
WIDE 位提供，客户端不再自行判定，两处判定必须一致。

**防复发约定：字符数 ≠ 列数**（M2b-0 必改 1 后半 + 评审）：
终端渲染中「字符数」与「列数」是两个量。凡涉及位置或宽度的计算
一律用列数（背景 quad、下划线、run 跨度 = `run_cols` = 各字符列宽
之和），只有遍历字符时才用字符数（`run_len`）。
本轮已因混用出现两次错误：①字形列推进用列、背景宽度用字符数；
②初始实现里宽字符判定用 `ch > 0xFF` 把 é ü α β 误判为双列。
M2b-1 接入协议后，daemon 推送的 start_col / end_col 都是列号，
客户端不得再用字符数参与任何几何计算。

**字形位图格式处理**（M2b-1 评审）：`img.content` 三种格式分支：
- `Mask`（1 字节/像素）：主路径——cosmic_text 0.15 在 swash.rs 硬编码
  `.format(Format::Alpha)`，实测 'A'/'你'/'a'/'粗' 均为 Mask，无暴露的
  子像素设置（无需从源头改）
- `SubpixelMask`（4 字节/像素 RGBA）：取 RGB 均值作 alpha（终端灰度渲染，
  不做子像素抗锯齿）。防御分支——cosmic_text 当前不会产出，版本升级后可能
- `Color`（彩色字形，如 emoji）：**已知限制——本轮不支持**，warn + 跳过该
  字形（背景照画）。M2b-2/3 或后续里程碑按需实现
- 显式长度校验 + warn（禁止 `.min(len-1)` 钳位——钳位把越界变成
  「重复读最后一字节」，让错误信号消失只表现为画错）

**物理像素约定**（M2b-1 评审，Retina 渲染）：内部一律用物理像素，
只有与 iced 布局交互（widget bounds、鼠标坐标）时才换算。
- `measure_font(scale)`：Metrics 字号 × scale（方式 B）——所有测量值
  （advance/ascent/位图尺寸）同一来源同一单位，无手工换算；
  physical() 的 scale 参数与字号分离时容易漏乘
- **cell 尺寸一律取整为整数物理像素**（真实终端 Alacritty/WezTerm/
  iTerm 同）：advance 实测 19.2 若直接用，每列 x=0/19.2/38.4/57.6...
  落在非整数像素边界 → quad 覆盖半个物理像素 → 字形边缘重采样发虚，
  且每列小数部分不同整行参差不齐。round() 最接近真实 advance，
  列累计误差最小
- scale=2 取整后实测：cell_width=19px、cell_height=40px、
  ascent=29px（原始 19.2/40/28.5）。逻辑尺寸只用于与 iced 布局交互；
  取整后 cell 宽度变化会影响列数换算（M2b-2 resize 用物理像素换算）
- 字形 quad 整数对齐：glyph_x = cell_x + placement.left（left 是
  整数）、glyph_y = (row_y + ascent).round() - placement.top、
  下划线高度 1px（1.5px 曾落在非整数边界）。测试断言所有 quad
  顶点坐标为整数
- 字形缓存 key = (char, bold, scale.to_bits())：跨 DPI 拖动窗口时
  scale 变化 → 旧字形全部失效，reset_atlas（清缓存/图集归零/cursor
  重置/全量重传）后按新 scale 重建
- quad 坐标（origin×scale + col×cw_physical）与 uniform screen_size
  （physical_size）同一物理空间
- 采样器 Nearest（min/mag/mipmap）——字形按物理像素精确光栅化，
  无插值
- **sRGB 处理**：iced 0.14 默认 GAMMA_CORRECTION=true → surface 为
  sRGB 格式（iced 自身文字 CPU 端 into_linear 再输出）。终端管线
  颜色是 sRGB 值，直接输出会被 GPU 二次编码 → 发灰。fs_main 在
  surface 为 sRGB 时输出前 pow(2.2) 转 linear（uniform gamma flag
  自适应，alpha 不参与 gamma）
- 图集 1024×1024：2x 下 226 典型字符实测占用 15.0%（512² 时为
  58.6%，粗体字形是独立位图会再翻倍，故保守扩 1024）。纹理
  4MB，可接受。溢出有 warn（非静默丢弃）

**终端字体显式指定**（M2b-1 评审，字体观感根因）：ASCII 曾用
`CourierNewPSMT`（Courier New）——「发虚/和 GUI 原生字体不一样」
的根因，与渲染管线无关。
- **引入原因**：为绕开 GB18030 Bitmap（无矢量轮廓）删除所有含
  "Bitmap" 的字体，副作用是**改变了 monospace fallback 顺序**，
  Courier New（fontdb 加载顺序靠前）排到 Menlo 之前
- **修正**：主字体显式 `Family::Name("Menlo")`（产品决策，不依赖
  fallback 顺序）；`VIDA_FONT_FAMILY` 可覆盖；缺失时按候选回落
  `Menlo → SF Mono → Monaco → 默认 monospace` 并 warn；启动日志
  打印 'A'/'你' 实际字体名（log_face_names）
- 中文仍走 fallback（BIZ UDGothic 等有矢量轮廓的字体）；
  「删除含 Bitmap 字体」逻辑保留（防止位图字体混入），其副作用
  由显式主字体消除
- 配置接口：`VIDA_FONT_SIZE`（默认 16，范围 8-48）、
  `VIDA_FONT_FAMILY`（默认 Menlo）——M2b-3 设置页接入时迁移为
  Settings 字段
- Menlo 16px 实测（scale=1）：cell_width=10、cell_height=20、
  ascent=14（decisions.md 本轮记录）

**排查记录与教训（M2b-1 完整链路）**：
- **删除 Bitmap 字体的副作用**：为绕开 GB18030 Bitmap（无矢量轮廓）
  删除所有含 "Bitmap" 的字体，副作用是改变了 monospace fallback 顺序，
  ASCII 落到 Courier New（旧式打字机衬线体，笔画细/x-height 低）。
  **教训：终端字体必须显式指定 family，不能依赖 fallback 顺序；
  排查字体问题时第一步就该打印实际使用的字体名**（log_face_names）
- **图集 cursor 必须跨帧持久**：每帧重置 next_x/next_y 会让新字形
  覆盖已写入的字形（ASCII 先写入靠前最易被覆盖）。测试
  atlas_cursor_must_persist_across_frames 覆盖
- **write_texture 的 layout 必须匹配源缓冲布局**：bytes_per_row 需
  256 对齐且与源数据行距一致，因此不能逐字形上传（字形宽 < 256
  无法对齐）；方案为维护 CPU 完整图集 + 按行区间上传（行距恒
  ATLAS*4=4096）。测试 atlas_row_upload_passes_validation 覆盖
- **环境假设必须先测量再推理**：本机 1920×1080 非 HiDPI、scale=1。
  此前多轮基于「Retina 2x」的推断（glyph.physical scale、整数
  cell、gamma）均不成立或无关——scale=1 时字形 1:1 光栅化本无
  模糊。字体观感问题的真正根因是字体选择（Courier New）
- **GUI 日志排查**：tracing filter 必须用 bin 名（"vida"）而非
  package 名（"vida_gui"）——module_path! 以 bin 名为前缀；
  RUST_LOG 被 shell 设置时可能吞掉全部 GUI 日志（改用 VIDA_LOG）

**M2b-2 所有者视觉验收修正（2026-08-08）**：初次验收确认 Menlo 16、
灰色前景与 CJK 字距在 scale=1 下无法接受。读取本机 Ghostty 默认配置后，
默认值改为 Menlo 13、前景 `#ffffff`、背景 `#282c34`。
`VIDA_FONT_SIZE` / `VIDA_FONT_FAMILY` 覆盖仍保留。

scale=1 的真实 GUI 截图仍显示灰度抗锯齿边缘偏碎、偏细，因此字形首次写入 CPU
图集时对 alpha 使用 `pow(alpha, 0.72)` 提升中间覆盖率，模拟低 DPI stem
darkening；0/1 端点不变，不影响实心背景和光标。该计算不放在 GPU 片元 shader
热路径。终端 canvas 自身设为 `#282c34`，不再透出 iced 页面背景。

第二轮所有者截图确认：把 PingFang WIDE 字形强行横向铺满两个 cell 会破坏原始
宽高比，中文明显被压扁。该缩放已撤销；协议仍用两个 cell 推进列位置，但字形按
字体原始位图尺寸绘制。宁可保留少量右侧留白，也不能扭曲字形。

光标协议此前已完整传递 `row/col/visible`，但 primitive 没有消费这些字段，
因此画面完全没有光标。现在按 Ghostty 默认绘制实心方块并反转块内字形颜色；
终端活跃且设置启用时，以 500ms 周期切换光标相位。任何输入或新帧都会立即恢复
亮相位。订阅只在终端屏存活，离开终端后 timer 自动销毁。光标 overlay 几何只
建立一次；闪烁仅更新 16 字节 uniform，不得随相位重建整屏 GPU buffer。

终端字体族、字号和光标闪烁已进入 Settings。因为 Settings 是金库格式的一部分，
本次将金库 v4 升至 v5，并通过 JSON 层迁移补入 Menlo / 13 / true；GUI 保存时以
daemon 返回的完整 Settings 为底，只覆盖页面字段，避免清空未展示的 S3 配置。

所有者后续截图确认，zsh 输入行的 bold 中文观感正常，但普通输出的中文在小字号
Regular 下显得过细、像被横向压扁。宽字符普通输出改用字体的 Medium face；bold
仍使用 Bold，英文普通输出仍使用所选等宽字体的 Regular。该修正只改变字重，不对
字形做单轴缩放，因此不会再次破坏中文宽高比。

字体名称与字号禁止自由输入：字体下拉在打开设置时从 fontdb 的系统字体库生成，
只保留 `monospaced` family，避免比例字体破坏固定 cell；字号使用一组常用整数选项。
配置中已不存在的字体回落到已安装的 Menlo，再回落到列表第一项。

后续与同机 Ghostty、iced 原生界面逐像素对照，确认 macOS 的 pt 已是逻辑像素；
再乘 `96/72` 会把 13pt 中文放大为 17×17px，几乎占满 18px 行高。此前还对所有
宽字符强制使用 Medium，并以 `pow(0.72)` 加深灰度覆盖率，三者叠加造成中文像粗体、
行距拥挤且抗锯齿边缘接近位图字体。

终端布局和 wgpu 图集保持不变，字形光栅化改由 `font-kit 0.14.3` 调用平台原生后端：
macOS CoreText、Windows DirectWrite、Linux FreeType；只有原生字体缺字或加载失败时
才回落到 Swash。选择字体时先按 PostScript 名精确匹配 Regular/Bold，避免 CoreText
family 枚举把 Menlo Regular 误选成 Italic。`font-kit` 读取 PingFang 字体集合会让
Physical footprint 从约 248MB 激增到 552MB，因此原生后端只加载用户选择的等宽
字体；CJK 固定由已有 Swash 数据库回退，不复制大型字体集合。普通宽字符保持
Regular，alpha 原样上传；Menlo 13pt 在 scale=1 下为 cell 8×18、ascent 14，CoreText
`A` 位图 8×10、Swash 中文位图 13×13。最终隔离 release 实测 scale=1 Physical
footprint 257.3MB（峰值 257.8MB），相对旧版 247.9MB 增加约 9.4MB。

第四轮与同机 Ghostty 截图逐像素对照后确认：Ghostty 零配置并不使用
Menlo，而是内置 JetBrains Mono 13pt、纯白前景和原生 alpha 混合。Vida 因此
内置 JetBrains Mono 2.304 Regular/Bold（OFL-1.1），同时保留系统等宽字体选择。
拉丁字形由平台原生后端直接从内置字体光栅化，CJK 仍走 Swash 系统回退；
默认前景恢复 `#ffffff`。金库 v5 升至 v6，仅将与旧默认完全一致的
Menlo/13pt/闪烁设置迁移为 JetBrains Mono，保留其他用户选择。

第五轮所有者截图确认英文正常、中文仍偏小偏细。Ghostty 1.3.1 的
`+show-face --string='中文水测试'` 在本机明确返回 `PingFang SC`；源码进一步确认
macOS 通过 `CTFontCreateForString` 按系统语言发现 CJK 回退，并以 `ic_width`
调和回退字体尺寸。Vida 改为直接持有 CoreText 返回的 CTFont 句柄，不再用
Swash 光栅化中文，也不读取/复制整个 PingFang TTC。本机实际选择为
`PingFangSC-Regular` / `PingFangSC-Semibold`；JetBrains Mono → 苹方的 `ic_width`
系数约 1.05，13pt 中文实际光栅化约 13.65pt。scale=1 位图由 Swash 13×13
改为 CoreText 常规 13×14、中粗 14×14，均位于 8×18 cell 内。原生 CTFont
发布版窗口输出中英文和 `ls` 后 Physical footprint 为 252.0MB（峰值 252.8MB）；
测试同时检查中文轮廓覆盖至少四分之三的位图行，并以「上」的非对称轮廓校验
位图方向，防止坐标错误导致缺笔或上下颠倒。
测试同时断言原生遮罩包含非零实心像素和 0—255 之间的抗锯齿覆盖率，且普通、粗体、
CJK 位图上下界都位于 cell 内。
图集增量上传测试会把跨行探针写入 GPU 纹理，再复制回 MAP_READ buffer 与 CPU 图集
逐字节比较；因此本次缺笔已确认不是 atlas 行距、上传范围或 GPU 数据损坏。

第六轮所有者截图指出：输入态 `ls --color` 的 `--` 过暗，且中英文都像粗体、
缺少平滑抗锯齿。对照本机 Ghostty 1.3.1 的有效配置与 CoreText 源码后确认，
Ghostty 默认 `font-thicken=false`，使用 `linearGray`、亚像素定位且关闭亚像素量化；
Vida 此前经 font-kit/自有 CJK 路径把 font smoothing 打开，实际产生了额外加粗。
macOS 的拉丁与 CJK 现统一走 CTFont 句柄，使用 linearGray 灰度遮罩并关闭 smoothing；
ANSI 0—15 色同步为 Ghostty 默认调色板，daemon 也不再把 NamedColor 丢成默认白色。
CoreText 位图上传前裁掉全透明边界；低 DPI 的单像素横/竖笔画只归一化峰值覆盖，
不扩张轮廓，避免 `-` 首次出现时被窗口合成稀释到近乎不可见。scale=1 发布版
输出中英文与 `ls --color` 后 Physical footprint 为 251.5MB（峰值 252.2MB）。

第七轮所有者截图证明上述自建位图修正仍不可靠：输入态 `ls --color` 的两枚
短横线仍会被采样到几乎不可见，CJK 视觉高度也再次偏小。对照 Oryxis 当前源码后，
关键差异不是某个 CoreText 开关，而是架构：Oryxis 不维护独立字形位图、基线换算
和纹理采样器，而是使用 iced canvas 的 `fill_text`，让 iced/cosmic-text 负责字体
回退与 GPU 文本缓存；ASCII 合并为短 run，宽字符保持逐 cell 定位。Vida 因此删除
自建 wgpu 字形图集，按同一原则独立实现文字层：cell advance 由 iced Paragraph 对
40 个 `0` 的真实宽度测量并缓存，ASCII 最多 32 字符一批，CJK 按协议 WIDE 起始列
单独绘制，行高为字号的 1.15 倍。背景、反色、光标和装饰线仍按 cell 绘制，PTY、
网格协议与人的输入路径均未改变。启用 iced `canvas` feature 会引入其官方 lyon
几何依赖，这是使用 iced 原生文字路径所必需，不是新增终端栈。scale=1 隔离发布版
实测：首次输入但未执行的 `ls --color` 两枚短横线清晰可见，`echo 中文测试` 未压扁；
Physical footprint 为 264.8MB（峰值 265.2MB）。

**对齐自查方法**（评审建议）：rows() 最上面加一行尺子——
每列一个 `|`，共 80 列。像素级验证（surface readback dump PNG）：
80 个 `|` 全部落在 `col * cell_width` 列边界（偏差 <1px）；
反色块白底边缘 [col24, col28) 与列边界精确重合。

**教训：macOS 'GB18030 Bitmap' 纯位图中文字体**（无 glyf/cff 矢量轮廓表）：
cosmic-text 的 swash 光栅化对它会静默失败（get_image 返回 None），
中文 fallback 选中它导致字形缺失。构造 FontSystem 时必须用自定义 fontdb
移除 post_script_name 含 'Bitmap' 的字体，fallback 才会选中有轮廓的字体。

**逐 cell 定位的实现**（term_grid example）：
- 每个 cell 的 x = `col * cell_width`，显式计算
- 每个 RLE run 一个 cosmic-text Buffer（run 内同字符），
  run 的 glyph x = `start_col * cell_width + glyph 内偏移`
- 宽字符（中文）占 `2 * cell_width`，由 run 的 start_col 推进，
  **不由字体决定**
- 不把整行拼成字符串排版——避免字体回退/emoji 时字形宽度
  与列宽模型不一致

**渲染路径**：cosmic-text 排版 → SwashCache 光栅化字形到 512×512
图集（灰度 alpha）→ wgpu 纹理 quad 逐 glyph 绘制。属性（粗体用
Weight::BOLD，下划线画 1.5px 线，反色用黑字+白底）。

**验证要点**：row 2 的 `你好世界abc你好`，4 个中文 = 8 列，
`abc` 起始应在第 16 列——所有者截图确认对齐。

## UI 结论 1：原生视觉令牌 + 字体图标，不引入 WebView（2026-08-08）

UI 改造继续使用锁定的 iced 0.14 和现有 wgpu 渲染器。参考 Oryxis 的紧凑标签栏、
深色表面层级和设置侧栏，但不复制其 AGPL 实现；Vida 在 `vida-gui/src/ui.rs` 中独立
定义颜色、间距、圆角和控件状态。这样所有页面共享同一视觉语言，又不会把 GUI 业务
状态移出既有的 `VidaApp` / `Screen` 分层。

图标采用 Lucide 字体的单个 TTF 资源并由 iced 文字管线绘制。与为每个按钮启用 SVG
解析相比，它不增加新的渲染后端，也不为每个图标建立独立纹理。标签区域单独横向滚动，
操作按钮固定在右侧；这是布局约束，不依赖窗口宽度的手工估算。

## M2b-2 终端输入、粘贴与 resize（2026-08-08）

### 人的输入路径

`TermCanvas` 是 iced focusable widget。进入终端屏时自动聚焦，点击画布也会聚焦；
只有 focused 且窗口 focused 时才消费键盘和 IME 事件。普通字符使用 iced
`KeyPressed.text` 的 UTF-8，中文等组合输入使用 `InputMethod::Commit`，并持续请求
`Purpose::Terminal`，候选窗锚点取终端光标所在 cell。

普通字符、Ctrl+A-Z/Ctrl+[ 等控制字节、方向键/Home/End/Delete/Page/F1-F12
在 GUI 内转换为标准终端字节。`SessionInput` 的 daemon 语义不变：收到什么就原样
写入 PTY，不做命令解释或换行转换，AI 也不进入人的输入路径。

逐键输入禁止使用“一键一个异步 Task”：多个 Task 的调度顺序不等于键盘事件顺序。
`WsClient::send_queued` 在 iced update 内同步进入单一 mpsc，后台 WebSocket writer
按队列顺序发送；它只用于不读取响应的输入和 resize，不用于业务请求。

### 粘贴必须与普通输入分流

GUI 实测发现：把多行剪贴板直接走 `SessionInput` 会让 shell 逐行立即执行，这是
终端安全问题。新增 `PasteSession`，daemon 查询 alacritty `TermMode::BRACKETED_PASTE`：

- 启用时写入 `ESC[200~ + 内容 + ESC[201~`，多行先进入 shell 编辑缓冲区，用户按
  Enter 后才执行；同时删除内容中的 ESC，防止伪造结束边界后注入控制序列。
- 未启用时保持原内容，兼容不支持 bracketed paste 的程序。

不得为了省接口而把所有输入都包成 paste；这会破坏控制键和 TUI。密码、私钥和
剪贴板内容均不得写日志。

### resize 使用同一份真实字体度量

渲染管线把当前 scale 下实测的物理像素 `cell_width/cell_height` 原子回传给 widget。
行列计算为 `floor(logical_bounds × scale / physical_cell)`，范围限制 1–1000。
窗口变化只有跨过一个完整 cell、行列数真的改变时才发送 `ResizeSession`，因此无需
常驻 debounce timer，空闲 CPU 仍为零。

GUI 收到新尺寸时先重建 `ClientGrid`，daemon 随后在同一个请求中同时执行
`Term::resize` 与 PTY ioctl。scale 变化时先用同公式的保守值，渲染器给出真实度量后
下一帧自动校正，避免 Retina/跨显示器时逻辑像素与物理像素混用。
