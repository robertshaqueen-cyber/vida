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

**风险**: 直接赋值绕过了 merge 仲裁，谁最后写谁赢。

**缓解措施**: 仅在 `Event::Keyboard` 和 `Event::InputMethod` 事件时写入
`Disabled`。非键盘事件（鼠标、窗口、触摸）不涉及 IME，写入可能影响
兄弟 widget。限制为键盘事件后，只有 focused widget 处理这些事件。

**待验证**: S4 页面同屏有 secure 和非 secure 输入框时，中文输入法在
「备注」等普通字段正常工作，「口令」字段无候选框。

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
