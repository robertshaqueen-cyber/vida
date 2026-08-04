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
