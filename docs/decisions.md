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
