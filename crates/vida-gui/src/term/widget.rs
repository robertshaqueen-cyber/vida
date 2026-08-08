//! 可交互终端 widget（M2b-2）：渲染 grid，并把人的输入原样送往 PTY。
//!
//! 设计约束：
//! - widget 只负责把平台键盘事件转换为终端字节，不解释命令；
//! - 输入按 iced 事件顺序发布，WebSocket 客户端同步入队，避免逐键异步任务乱序；
//! - resize 使用渲染管线实测的物理像素 cell 尺寸，逻辑像素只用于 iced 布局；
//! - widget 自身不设置定时器；光标闪烁由终端屏存活期间的 app subscription 驱动。

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use iced::advanced::Renderer as _;
use iced::advanced::graphics::geometry::Renderer as GeometryRenderer;
use iced::advanced::layout::{self, Layout};
use iced::advanced::mouse;
use iced::advanced::renderer;
use iced::advanced::text::Paragraph as _;
use iced::advanced::widget::operation::Focusable;
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, clipboard, input_method};
use iced::keyboard::{self, Key, Modifiers, key};
use iced::widget::canvas::{self, Frame, Text as CanvasText};
use iced::{
    Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Size, Vector, alignment, font,
    window,
};

use super::client_grid::ClientGrid;
use super::primitive::{
    DEFAULT_BG, TerminalAppearance, ViewportMetrics, resolve_bg, resolve_text_fg,
};

const TERMINAL_WIDGET_ID: &str = "vida-terminal-canvas";

/// 供进入终端屏时自动聚焦。
pub fn id() -> iced::widget::Id {
    iced::widget::Id::new(TERMINAL_WIDGET_ID)
}

struct State {
    focused: bool,
    window_focused: bool,
    scale_factor: f32,
    preedit: Option<input_method::Preedit>,
    last_grid_size: Option<(u16, u16)>,
    geometry_cache: canvas::Cache,
    last_render_key: Cell<Option<RenderKey>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderKey {
    version: u64,
    font: Font,
    font_size_bits: u32,
    cursor_on: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            focused: false,
            window_focused: true,
            scale_factor: 1.0,
            preedit: None,
            last_grid_size: None,
            geometry_cache: canvas::Cache::new(),
            last_render_key: Cell::new(None),
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
    appearance: TerminalAppearance,
    cursor_on: bool,
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
        appearance: TerminalAppearance,
        cursor_on: bool,
    ) -> Self {
        Self {
            snapshot,
            viewport_metrics,
            on_input,
            on_paste,
            on_resize,
            appearance,
            cursor_on,
            width: Length::Fill,
            height: Length::Fill,
        }
    }
}

impl<Message, Theme> Widget<Message, Theme, iced::Renderer> for TermCanvas<Message> {
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
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &iced::Renderer,
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
        _renderer: &iced::Renderer,
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
        tree: &Tree,
        renderer: &mut iced::Renderer,
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
        let state = tree.state.downcast_ref::<State>();
        let appearance = self.appearance.normalized();
        let base_font = terminal_font(&appearance.font_family, false, false);
        let cell_width = cell_advance(base_font, appearance.font_size);
        let cell_height = appearance.font_size * 1.15;
        self.viewport_metrics.store(
            cell_width * state.scale_factor,
            cell_height * state.scale_factor,
            state.scale_factor,
        );

        let key = RenderKey {
            version: self.snapshot.version,
            font: base_font,
            font_size_bits: appearance.font_size.to_bits(),
            cursor_on: self.cursor_on,
        };
        if state.last_render_key.get() != Some(key) {
            state.last_render_key.set(Some(key));
            state.geometry_cache.clear();
        }
        let geometry = state.geometry_cache.draw(renderer, bounds.size(), |frame| {
            draw_grid(
                frame,
                &self.snapshot,
                base_font,
                appearance.font_size,
                cell_width,
                cell_height,
                self.cursor_on,
            );
        });
        renderer.with_translation(Vector::new(bounds.x, bounds.y), |renderer| {
            renderer.draw_geometry(geometry);
        });
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }
}

/// Oryxis 的终端没有自建字形位图和纹理采样器，而是把文字交回 iced/cosmic-text。
/// 这里沿用同一做法，并通过 iced 自己的 Paragraph 测量真实等宽 advance，确保
/// 绘制、光标、PTY resize 三者使用同一套度量。
fn cell_advance(font: Font, font_size: f32) -> f32 {
    static ADVANCES: OnceLock<Mutex<HashMap<(Font, u32), f32>>> = OnceLock::new();
    let key = (font, font_size.to_bits());
    let mut advances = ADVANCES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(advance) = advances.get(&key) {
        return *advance;
    }
    const SAMPLES: usize = 40;
    let sample = "0".repeat(SAMPLES);
    let text = iced::advanced::text::Text {
        content: sample.as_str(),
        bounds: Size::INFINITE,
        size: Pixels(font_size),
        line_height: iced::advanced::text::LineHeight::default(),
        font,
        align_x: iced::advanced::text::Alignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: iced::advanced::text::Shaping::Basic,
        wrapping: iced::advanced::text::Wrapping::None,
    };
    let width = iced::advanced::graphics::text::Paragraph::with_text(text)
        .min_bounds()
        .width;
    let advance = if width > 0.0 {
        width / SAMPLES as f32
    } else {
        font_size * 0.6
    };
    advances.insert(key, advance);
    advance
}

fn intern_font_family(name: &str) -> &'static str {
    static FAMILIES: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let mut families = FAMILIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(name) = families.get(name) {
        return name;
    }
    let owned = name.to_string();
    let interned = Box::leak(owned.clone().into_boxed_str());
    families.insert(owned, interned);
    interned
}

fn terminal_font(family: &str, bold: bool, italic: bool) -> Font {
    Font {
        family: font::Family::Name(intern_font_family(family)),
        weight: if bold {
            font::Weight::Bold
        } else {
            font::Weight::Normal
        },
        style: if italic {
            font::Style::Italic
        } else {
            font::Style::Normal
        },
        ..Font::DEFAULT
    }
}

fn color(rgb: (u8, u8, u8)) -> Color {
    Color::from_rgb8(rgb.0, rgb.1, rgb.2)
}

fn draw_text(
    frame: &mut Frame<iced::Renderer>,
    content: String,
    position: Point,
    foreground: Color,
    size: f32,
    font: Font,
) {
    frame.fill_text(CanvasText {
        content,
        position,
        color: foreground,
        size: Pixels(size),
        font,
        align_x: alignment::Horizontal::Left.into(),
        align_y: alignment::Vertical::Top,
        ..CanvasText::default()
    });
}

fn draw_grid(
    canvas: &mut Frame<iced::Renderer>,
    grid: &ClientGrid,
    base_font: Font,
    font_size: f32,
    cell_width: f32,
    cell_height: f32,
    cursor_on: bool,
) {
    canvas.fill_rectangle(Point::ORIGIN, canvas.size(), color(DEFAULT_BG));

    // Canvas 的文字层始终在形状层之上，所以先画背景、光标和装饰线。
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let Some(cell) = grid.cell(row, col) else {
                continue;
            };
            if cell.flags & super::client_grid::cell_flags::WIDE_SPACER != 0 {
                continue;
            }
            let x = col as f32 * cell_width;
            let y = row as f32 * cell_height;
            let width = if cell.flags & super::frame::flag::WIDE != 0 {
                cell_width * 2.0
            } else {
                cell_width
            };
            let is_cursor = cursor_on
                && grid.cursor_visible
                && row == grid.cursor_row
                && col == grid.cursor_col;
            if is_cursor {
                canvas.fill_rectangle(
                    Point::new(x, y),
                    Size::new(width, cell_height),
                    color(resolve_text_fg(cell)),
                );
            } else if cell.bg != super::client_grid::ColorSpec::Default
                || cell.flags & super::frame::flag::REVERSE != 0
            {
                canvas.fill_rectangle(
                    Point::new(x, y),
                    Size::new(width, cell_height),
                    color(resolve_bg(cell)),
                );
            }
            let decoration = if is_cursor {
                color(resolve_bg(cell))
            } else {
                color(resolve_text_fg(cell))
            };
            if cell.flags & super::frame::flag::UNDERLINE != 0 {
                canvas.fill_rectangle(
                    Point::new(x, y + cell_height - 2.0),
                    Size::new(width, 1.0),
                    decoration,
                );
            }
            if cell.flags & super::frame::flag::STRIKEOUT != 0 {
                canvas.fill_rectangle(
                    Point::new(x, y + (cell_height * 0.52).round()),
                    Size::new(width, 1.0),
                    decoration,
                );
            }
        }
    }

    struct Run {
        row: u16,
        start_col: u16,
        next_col: u16,
        foreground: Color,
        font: Font,
        content: String,
    }
    let flush = |canvas: &mut Frame<iced::Renderer>, run: Run| {
        draw_text(
            canvas,
            run.content,
            Point::new(
                run.start_col as f32 * cell_width,
                run.row as f32 * cell_height,
            ),
            run.foreground,
            font_size,
            run.font,
        );
    };
    let mut run: Option<Run> = None;
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let Some(cell) = grid.cell(row, col) else {
                continue;
            };
            let is_spacer = cell.flags & super::client_grid::cell_flags::WIDE_SPACER != 0;
            let hidden = cell.flags & super::frame::flag::HIDDEN != 0;
            let is_cursor = cursor_on
                && grid.cursor_visible
                && row == grid.cursor_row
                && col == grid.cursor_col;
            let foreground = if is_cursor {
                color(resolve_bg(cell))
            } else {
                color(resolve_text_fg(cell))
            };
            let cell_font = terminal_font(
                match base_font.family {
                    font::Family::Name(name) => name,
                    _ => super::primitive::DEFAULT_TERMINAL_FONT_FAMILY,
                },
                cell.flags & super::frame::flag::BOLD != 0,
                cell.flags & super::frame::flag::ITALIC != 0,
            );
            let batchable = !is_spacer
                && !hidden
                && !is_cursor
                && cell.ch.is_ascii_graphic()
                && cell.flags & super::frame::flag::WIDE == 0;
            let fits = batchable
                && run.as_ref().is_some_and(|run| {
                    run.row == row
                        && run.next_col == col
                        && run.foreground == foreground
                        && run.font == cell_font
                        && run.content.len() < 32
                });
            if fits {
                if let Some(run) = run.as_mut() {
                    run.content.push(cell.ch);
                    run.next_col += 1;
                }
                continue;
            }
            if let Some(run) = run.take() {
                flush(canvas, run);
            }
            if batchable {
                run = Some(Run {
                    row,
                    start_col: col,
                    next_col: col + 1,
                    foreground,
                    font: cell_font,
                    content: cell.ch.to_string(),
                });
            } else if !is_spacer && !hidden && cell.ch != ' ' && cell.ch != '\0' {
                draw_text(
                    canvas,
                    cell.ch.to_string(),
                    Point::new(col as f32 * cell_width, row as f32 * cell_height),
                    foreground,
                    font_size,
                    cell_font,
                );
            }
        }
        if let Some(run) = run.take() {
            flush(canvas, run);
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
    appearance: TerminalAppearance,
    cursor_on: bool,
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
        appearance,
        cursor_on,
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
