//! 可交互终端 widget（M2b-2）：渲染 grid，并把人的输入原样送往 PTY。
//!
//! 设计约束：
//! - widget 只负责把平台键盘事件转换为终端字节，不解释命令；
//! - 输入按 iced 事件顺序发布，WebSocket 客户端同步入队，避免逐键异步任务乱序；
//! - resize 使用渲染管线实测的物理像素 cell 尺寸，逻辑像素只用于 iced 布局；
//! - 没有输入或尺寸变化时不产生消息、不设置常驻定时器。

use std::sync::Arc;

use iced::advanced::layout::{self, Layout};
use iced::advanced::mouse;
use iced::advanced::renderer;
use iced::advanced::widget::operation::Focusable;
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, clipboard, input_method};
use iced::keyboard::{self, Key, Modifiers, key};
use iced::{Element, Event, Length, Rectangle, Size, window};

use super::client_grid::ClientGrid;
use super::primitive::{TermPrimitive, ViewportMetrics};

const TERMINAL_WIDGET_ID: &str = "vida-terminal-canvas";

/// 供进入终端屏时自动聚焦。
pub fn id() -> iced::widget::Id {
    iced::widget::Id::new(TERMINAL_WIDGET_ID)
}

#[derive(Debug)]
struct State {
    focused: bool,
    window_focused: bool,
    scale_factor: f32,
    preedit: Option<input_method::Preedit>,
    last_grid_size: Option<(u16, u16)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            focused: false,
            window_focused: true,
            scale_factor: 1.0,
            preedit: None,
            last_grid_size: None,
        }
    }
}

impl Focusable for State {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn focus(&mut self) {
        self.focused = true;
    }

    fn unfocus(&mut self) {
        self.focused = false;
        self.preedit = None;
    }
}

/// 终端画面 widget：持有 grid 快照、渲染器实测度量和消息构造器。
pub struct TermCanvas<Message> {
    snapshot: Arc<ClientGrid>,
    viewport_metrics: Arc<ViewportMetrics>,
    on_input: fn(Vec<u8>) -> Message,
    on_paste: fn(Vec<u8>) -> Message,
    on_resize: fn(u16, u16) -> Message,
    width: Length,
    height: Length,
}

impl<Message> TermCanvas<Message> {
    pub fn new(
        snapshot: Arc<ClientGrid>,
        viewport_metrics: Arc<ViewportMetrics>,
        on_input: fn(Vec<u8>) -> Message,
        on_paste: fn(Vec<u8>) -> Message,
        on_resize: fn(u16, u16) -> Message,
    ) -> Self {
        Self {
            snapshot,
            viewport_metrics,
            on_input,
            on_paste,
            on_resize,
            width: Length::Fill,
            height: Length::Fill,
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for TermCanvas<Message>
where
    Renderer: iced::advanced::Renderer + iced_wgpu::primitive::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let widget_id = id();
        operation.focusable(Some(&widget_id), layout.bounds(), state);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if cursor.is_over(bounds) {
                    state.focused = true;
                    shell.request_redraw();
                    shell.capture_event();
                } else {
                    state.focused = false;
                    state.preedit = None;
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                text,
                ..
            }) if state.focused && state.window_focused => {
                match translate_key(key, *physical_key, *modifiers, text.as_deref()) {
                    KeyAction::Bytes(bytes) => {
                        shell.publish((self.on_input)(bytes));
                        shell.capture_event();
                    }
                    KeyAction::Paste => {
                        if let Some(content) = clipboard.read(clipboard::Kind::Standard)
                            && !content.is_empty()
                        {
                            shell.publish((self.on_paste)(content.into_bytes()));
                        }
                        shell.capture_event();
                    }
                    KeyAction::Ignore => {}
                }
            }
            Event::InputMethod(input_method::Event::Opened) if state.focused => {
                state.preedit = Some(input_method::Preedit::new());
                shell.request_redraw();
                shell.capture_event();
            }
            Event::InputMethod(input_method::Event::Preedit(content, selection))
                if state.focused =>
            {
                state.preedit = Some(input_method::Preedit {
                    content: content.clone(),
                    selection: selection.clone(),
                    text_size: None,
                });
                shell.request_redraw();
                shell.capture_event();
            }
            Event::InputMethod(input_method::Event::Commit(content)) if state.focused => {
                state.preedit = Some(input_method::Preedit::new());
                if !content.is_empty() {
                    shell.publish((self.on_input)(content.as_bytes().to_vec()));
                }
                shell.capture_event();
            }
            Event::InputMethod(input_method::Event::Closed) => {
                state.preedit = None;
            }
            Event::Window(window::Event::Focused) => {
                state.window_focused = true;
                if state.focused {
                    shell.request_redraw();
                }
            }
            Event::Window(window::Event::Unfocused) => {
                state.window_focused = false;
                state.preedit = None;
            }
            Event::Window(window::Event::Rescaled(scale)) if scale.is_finite() && *scale > 0.0 => {
                state.scale_factor = *scale;
                state.last_grid_size = None;
                shell.request_redraw();
            }
            Event::Window(window::Event::RedrawRequested(_)) => {
                if let Some(size) = grid_size(bounds, state.scale_factor, &self.viewport_metrics)
                    && state.last_grid_size != Some(size)
                {
                    state.last_grid_size = Some(size);
                    shell.publish((self.on_resize)(size.0, size.1));
                }

                if state.focused && state.window_focused {
                    shell.request_input_method(&terminal_input_method(
                        &self.snapshot,
                        bounds,
                        state,
                        &self.viewport_metrics,
                    ));
                }
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if self.snapshot.rows == 0 || self.snapshot.cols == 0 {
            return;
        }
        let primitive =
            TermPrimitive::new(self.snapshot.clone(), bounds, self.viewport_metrics.clone());
        renderer.draw_primitive(bounds, primitive);
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }
}

/// 把 TermCanvas 变成 Element。
pub fn canvas<'a, Message>(
    snapshot: Arc<ClientGrid>,
    viewport_metrics: Arc<ViewportMetrics>,
    on_input: fn(Vec<u8>) -> Message,
    on_paste: fn(Vec<u8>) -> Message,
    on_resize: fn(u16, u16) -> Message,
) -> Element<'a, Message>
where
    Message: 'a,
{
    Element::new(TermCanvas::new(
        snapshot,
        viewport_metrics,
        on_input,
        on_paste,
        on_resize,
    ))
}

#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    Bytes(Vec<u8>),
    Paste,
    Ignore,
}

fn translate_key(
    key: &Key,
    physical_key: key::Physical,
    modifiers: Modifiers,
    text: Option<&str>,
) -> KeyAction {
    if is_paste_shortcut(key, physical_key, modifiers) {
        return KeyAction::Paste;
    }

    // Command/Windows 键的其他组合交给系统；Ctrl 仍是终端控制键。
    if modifiers.logo() {
        return KeyAction::Ignore;
    }

    if modifiers.control() {
        if matches!(key, Key::Named(key::Named::Space)) {
            return KeyAction::Bytes(vec![0]);
        }
        let character = key.to_latin(physical_key).or_else(|| match key.as_ref() {
            Key::Character(value) if value.len() == 1 => value.chars().next(),
            _ => None,
        });
        if let Some(byte) = character.and_then(control_byte) {
            return KeyAction::Bytes(vec![byte]);
        }
    }

    let modifier = xterm_modifier(modifiers);
    let named = match key.as_ref() {
        Key::Named(key::Named::Enter) => Some(b"\r".as_slice()),
        Key::Named(key::Named::Tab) if modifiers.shift() => Some(b"\x1b[Z".as_slice()),
        Key::Named(key::Named::Tab) => Some(b"\t".as_slice()),
        Key::Named(key::Named::Backspace) => Some(b"\x7f".as_slice()),
        Key::Named(key::Named::Escape) => Some(b"\x1b".as_slice()),
        Key::Named(key::Named::ArrowUp) if modifier == 1 => Some(b"\x1b[A".as_slice()),
        Key::Named(key::Named::ArrowDown) if modifier == 1 => Some(b"\x1b[B".as_slice()),
        Key::Named(key::Named::ArrowRight) if modifier == 1 => Some(b"\x1b[C".as_slice()),
        Key::Named(key::Named::ArrowLeft) if modifier == 1 => Some(b"\x1b[D".as_slice()),
        Key::Named(key::Named::Home) if modifier == 1 => Some(b"\x1b[H".as_slice()),
        Key::Named(key::Named::End) if modifier == 1 => Some(b"\x1b[F".as_slice()),
        Key::Named(key::Named::Insert) => Some(b"\x1b[2~".as_slice()),
        Key::Named(key::Named::Delete) => Some(b"\x1b[3~".as_slice()),
        Key::Named(key::Named::PageUp) => Some(b"\x1b[5~".as_slice()),
        Key::Named(key::Named::PageDown) => Some(b"\x1b[6~".as_slice()),
        Key::Named(key::Named::F1) => Some(b"\x1bOP".as_slice()),
        Key::Named(key::Named::F2) => Some(b"\x1bOQ".as_slice()),
        Key::Named(key::Named::F3) => Some(b"\x1bOR".as_slice()),
        Key::Named(key::Named::F4) => Some(b"\x1bOS".as_slice()),
        Key::Named(key::Named::F5) => Some(b"\x1b[15~".as_slice()),
        Key::Named(key::Named::F6) => Some(b"\x1b[17~".as_slice()),
        Key::Named(key::Named::F7) => Some(b"\x1b[18~".as_slice()),
        Key::Named(key::Named::F8) => Some(b"\x1b[19~".as_slice()),
        Key::Named(key::Named::F9) => Some(b"\x1b[20~".as_slice()),
        Key::Named(key::Named::F10) => Some(b"\x1b[21~".as_slice()),
        Key::Named(key::Named::F11) => Some(b"\x1b[23~".as_slice()),
        Key::Named(key::Named::F12) => Some(b"\x1b[24~".as_slice()),
        _ => None,
    };
    if let Some(bytes) = named {
        return KeyAction::Bytes(bytes.to_vec());
    }

    let modified_cursor = match key.as_ref() {
        Key::Named(key::Named::ArrowUp) => Some('A'),
        Key::Named(key::Named::ArrowDown) => Some('B'),
        Key::Named(key::Named::ArrowRight) => Some('C'),
        Key::Named(key::Named::ArrowLeft) => Some('D'),
        Key::Named(key::Named::Home) => Some('H'),
        Key::Named(key::Named::End) => Some('F'),
        _ => None,
    };
    if let Some(final_byte) = modified_cursor {
        return KeyAction::Bytes(format!("\x1b[1;{modifier}{final_byte}").into_bytes());
    }

    let Some(text) = text.filter(|value| !value.is_empty()) else {
        return KeyAction::Ignore;
    };
    let mut bytes = Vec::with_capacity(text.len() + usize::from(modifiers.alt()));
    if modifiers.alt() {
        bytes.push(0x1b);
    }
    bytes.extend_from_slice(text.as_bytes());
    KeyAction::Bytes(bytes)
}

fn is_paste_shortcut(key: &Key, physical_key: key::Physical, modifiers: Modifiers) -> bool {
    let is_v = key
        .to_latin(physical_key)
        .is_some_and(|ch| ch.eq_ignore_ascii_case(&'v'));
    #[cfg(target_os = "macos")]
    {
        is_v && modifiers.logo() && !modifiers.control() && !modifiers.alt()
    }
    #[cfg(not(target_os = "macos"))]
    {
        is_v && modifiers.control() && modifiers.shift() && !modifiers.alt()
    }
}

fn control_byte(ch: char) -> Option<u8> {
    let upper = ch.to_ascii_uppercase();
    match upper {
        '@' | '2' => Some(0),
        'A'..='Z' => Some(upper as u8 & 0x1f),
        '[' | '3' => Some(27),
        '\\' | '4' => Some(28),
        ']' | '5' => Some(29),
        '^' | '6' => Some(30),
        '_' | '7' | '/' => Some(31),
        '?' | '8' => Some(127),
        _ => None,
    }
}

fn xterm_modifier(modifiers: Modifiers) -> u8 {
    1 + u8::from(modifiers.shift())
        + 2 * u8::from(modifiers.alt())
        + 4 * u8::from(modifiers.control())
}

fn grid_size(bounds: Rectangle, scale: f32, metrics: &ViewportMetrics) -> Option<(u16, u16)> {
    if !scale.is_finite() || scale <= 0.0 || bounds.width <= 0.0 || bounds.height <= 0.0 {
        return None;
    }
    let (cell_width, cell_height) = metrics.cell_size_for(scale);
    let cols = ((bounds.width * scale) / cell_width)
        .floor()
        .clamp(1.0, 1000.0) as u16;
    let rows = ((bounds.height * scale) / cell_height)
        .floor()
        .clamp(1.0, 1000.0) as u16;
    Some((cols, rows))
}

fn terminal_input_method<'a>(
    snapshot: &ClientGrid,
    bounds: Rectangle,
    state: &'a State,
    metrics: &ViewportMetrics,
) -> input_method::InputMethod<&'a str> {
    let scale = state.scale_factor.max(f32::EPSILON);
    let (cell_width, cell_height) = metrics.cell_size_for(scale);
    let cursor = Rectangle {
        x: bounds.x + snapshot.cursor_col as f32 * cell_width / scale,
        y: bounds.y + snapshot.cursor_row as f32 * cell_height / scale,
        width: cell_width / scale,
        height: cell_height / scale,
    };
    input_method::InputMethod::Enabled {
        cursor,
        purpose: input_method::Purpose::Terminal,
        preedit: state.preedit.as_ref().map(input_method::Preedit::as_ref),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::{Code, Physical};

    fn physical(code: Code) -> Physical {
        Physical::Code(code)
    }

    #[test]
    fn printable_and_alt_text_are_utf8_bytes() {
        let normal = translate_key(
            &Key::Character("你".into()),
            physical(Code::KeyA),
            Modifiers::empty(),
            Some("你"),
        );
        assert_eq!(normal, KeyAction::Bytes("你".as_bytes().to_vec()));

        let alt = translate_key(
            &Key::Character("x".into()),
            physical(Code::KeyX),
            Modifiers::ALT,
            Some("x"),
        );
        assert_eq!(alt, KeyAction::Bytes(b"\x1bx".to_vec()));
    }

    #[test]
    fn control_and_navigation_keys_use_terminal_sequences() {
        let ctrl_c = translate_key(
            &Key::Character("c".into()),
            physical(Code::KeyC),
            Modifiers::CTRL,
            None,
        );
        assert_eq!(ctrl_c, KeyAction::Bytes(vec![3]));

        let left = translate_key(
            &Key::Named(key::Named::ArrowLeft),
            physical(Code::ArrowLeft),
            Modifiers::CTRL,
            None,
        );
        assert_eq!(left, KeyAction::Bytes(b"\x1b[1;5D".to_vec()));

        let ctrl_bracket = translate_key(
            &Key::Character("[".into()),
            Physical::Code(Code::BracketLeft),
            Modifiers::CTRL,
            None,
        );
        assert_eq!(ctrl_bracket, KeyAction::Bytes(vec![27]));
    }

    #[test]
    fn grid_uses_physical_pixels_and_clamps() {
        let metrics = ViewportMetrics::default();
        metrics.store(19.0, 40.0, 2.0);
        let size = grid_size(
            Rectangle {
                x: 0.0,
                y: 0.0,
                width: 950.0,
                height: 600.0,
            },
            2.0,
            &metrics,
        );
        assert_eq!(size, Some((100, 30)));
    }
}
