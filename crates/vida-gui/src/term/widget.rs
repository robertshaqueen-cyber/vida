//! 可交互终端 widget（M2b-2）：渲染 grid，并把人的输入原样送往 PTY。
//!
//! 设计约束：
//! - widget 只负责把平台键盘事件转换为终端字节，不解释命令；
//! - 输入按 iced 事件顺序发布，WebSocket 客户端同步入队，避免逐键异步任务乱序；
//! - resize 使用渲染管线实测的物理像素 cell 尺寸，逻辑像素只用于 iced 布局；
//! - widget 自身不设置定时器；光标闪烁由终端屏存活期间的 app subscription 驱动。

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
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
const CONTEXT_MENU_WIDTH: f32 = 148.0;
const CONTEXT_ITEM_HEIGHT: f32 = 34.0;
const SCROLL_TO_BOTTOM: i32 = -1_000_000;

/// 供进入终端屏时自动聚焦。
pub fn id() -> iced::widget::Id {
    iced::widget::Id::new(TERMINAL_WIDGET_ID)
}

struct State {
    session_key: String,
    focused: bool,
    window_focused: bool,
    scale_factor: f32,
    preedit: Option<input_method::Preedit>,
    last_grid_size: Option<(u16, u16)>,
    geometry_cache: canvas::Cache,
    last_render_key: Cell<Option<RenderKey>>,
    selecting: bool,
    selection: Option<Selection>,
    selection_rows: BTreeMap<u64, Vec<super::client_grid::ClientCell>>,
    drag_viewport_cell: Option<(u16, u16)>,
    last_viewport_start: u64,
    context_menu: Option<Point>,
    context_hover: Option<ContextItem>,
    wheel_remainder: f32,
    scrolled: bool,
    interaction_version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CellPosition {
    row: u64,
    col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: CellPosition,
    head: CellPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextItem {
    Copy,
    Paste,
}

impl Selection {
    fn ordered(self) -> (CellPosition, CellPosition) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn is_non_empty(self) -> bool {
        self.anchor != self.head
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderKey {
    version: u64,
    font: Font,
    font_size_bits: u32,
    cursor_on: bool,
    interaction_version: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            session_key: String::new(),
            focused: false,
            window_focused: true,
            scale_factor: 1.0,
            preedit: None,
            last_grid_size: None,
            geometry_cache: canvas::Cache::new(),
            last_render_key: Cell::new(None),
            selecting: false,
            selection: None,
            selection_rows: BTreeMap::new(),
            drag_viewport_cell: None,
            last_viewport_start: 0,
            context_menu: None,
            context_hover: None,
            wheel_remainder: 0.0,
            scrolled: false,
            interaction_version: 0,
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

impl State {
    fn reset_for_session(&mut self, session_key: &str) {
        self.session_key.clear();
        self.session_key.push_str(session_key);
        self.selecting = false;
        self.selection = None;
        self.selection_rows.clear();
        self.drag_viewport_cell = None;
        self.last_viewport_start = 0;
        self.context_menu = None;
        self.context_hover = None;
        self.wheel_remainder = 0.0;
        self.scrolled = false;
        self.interaction_version = self.interaction_version.wrapping_add(1);
    }

    fn clear_transient_interaction(&mut self) {
        self.selecting = false;
        self.drag_viewport_cell = None;
        self.context_menu = None;
        self.context_hover = None;
        self.interaction_version = self.interaction_version.wrapping_add(1);
    }

    fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_rows.clear();
        self.drag_viewport_cell = None;
    }

    fn cache_snapshot(&mut self, grid: &ClientGrid) {
        if self.selection.is_none() {
            self.last_viewport_start = grid.viewport_start;
            return;
        }
        for viewport_row in 0..grid.rows {
            if let Some(cells) = grid.line_cells(viewport_row) {
                self.selection_rows.insert(
                    grid.viewport_start + u64::from(viewport_row),
                    cells.to_vec(),
                );
            }
        }
        if self.selecting
            && grid.viewport_start != self.last_viewport_start
            && let Some((viewport_row, col)) = self.drag_viewport_cell
            && let Some(selection) = self.selection.as_mut()
        {
            selection.head = CellPosition {
                row: grid.viewport_start + u64::from(viewport_row),
                col,
            };
            self.interaction_version = self.interaction_version.wrapping_add(1);
        }
        self.last_viewport_start = grid.viewport_start;
    }

    fn selected_text(&mut self, grid: &ClientGrid) -> Option<String> {
        self.cache_snapshot(grid);
        selected_text(&self.selection_rows, grid.cols, self.selection)
    }
}

/// 终端画面 widget：持有 grid 快照、渲染器实测度量和消息构造器。
pub struct Callbacks<Message> {
    pub input: fn(Vec<u8>) -> Message,
    pub paste: fn(Vec<u8>) -> Message,
    pub resize: fn(u16, u16) -> Message,
    pub scroll: fn(i32) -> Message,
}

pub struct ContextLabels {
    pub copy: String,
    pub paste: String,
}

pub struct TermCanvas<Message> {
    session_key: String,
    snapshot: Arc<ClientGrid>,
    viewport_metrics: Arc<ViewportMetrics>,
    on_input: fn(Vec<u8>) -> Message,
    on_paste: fn(Vec<u8>) -> Message,
    on_resize: fn(u16, u16) -> Message,
    on_scroll: fn(i32) -> Message,
    appearance: TerminalAppearance,
    cursor_on: bool,
    width: Length,
    height: Length,
    copy_label: String,
    paste_label: String,
}

impl<Message> TermCanvas<Message> {
    pub fn new(
        session_key: String,
        snapshot: Arc<ClientGrid>,
        viewport_metrics: Arc<ViewportMetrics>,
        callbacks: Callbacks<Message>,
        appearance: TerminalAppearance,
        cursor_on: bool,
        labels: ContextLabels,
    ) -> Self {
        Self {
            session_key,
            snapshot,
            viewport_metrics,
            on_input: callbacks.input,
            on_paste: callbacks.paste,
            on_resize: callbacks.resize,
            on_scroll: callbacks.scroll,
            appearance,
            cursor_on,
            width: Length::Fill,
            height: Length::Fill,
            copy_label: labels.copy,
            paste_label: labels.paste,
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

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state.session_key == self.session_key {
            state.cache_snapshot(&self.snapshot);
        }
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

        if state.session_key != self.session_key {
            state.reset_for_session(&self.session_key);
        }
        state.cache_snapshot(&self.snapshot);

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if !cursor.is_over(bounds) {
                    state.focused = false;
                    state.preedit = None;
                    state.clear_transient_interaction();
                    return;
                }

                state.focused = true;
                let Some(position) = cursor.position_in(bounds) else {
                    return;
                };
                if let Some(menu_at) = state.context_menu {
                    let menu = context_menu_rect(menu_at, bounds.size());
                    if menu.contains(position) {
                        let copy = position.y < menu.y + CONTEXT_ITEM_HEIGHT;
                        if copy {
                            if let Some(content) = state.selected_text(&self.snapshot) {
                                clipboard.write(clipboard::Kind::Standard, content);
                            }
                        } else if let Some(content) = clipboard
                            .read(clipboard::Kind::Standard)
                            .filter(|value| !value.is_empty())
                        {
                            if state.scrolled {
                                shell.publish((self.on_scroll)(SCROLL_TO_BOTTOM));
                                state.scrolled = false;
                            }
                            shell.publish((self.on_paste)(content.into_bytes()));
                        }
                        state.clear_transient_interaction();
                        shell.request_redraw();
                        shell.capture_event();
                        return;
                    }
                }

                let viewport_cell =
                    point_to_viewport_cell(position, &self.appearance, &self.snapshot);
                let cell = viewport_cell.map(|(row, col)| CellPosition {
                    row: self.snapshot.viewport_start + u64::from(row),
                    col,
                });
                state.selection = cell.map(|anchor| Selection {
                    anchor,
                    head: anchor,
                });
                state.selecting = cell.is_some();
                state.drag_viewport_cell = viewport_cell;
                state.selection_rows.clear();
                state.cache_snapshot(&self.snapshot);
                state.context_menu = None;
                state.context_hover = None;
                state.interaction_version = state.interaction_version.wrapping_add(1);
                shell.request_redraw();
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.selecting => {
                if let Some(position) = cursor.position_in(bounds)
                    && let Some((viewport_row, col)) =
                        point_to_viewport_cell(position, &self.appearance, &self.snapshot)
                    && let Some(selection) = state.selection.as_mut()
                {
                    let head = CellPosition {
                        row: self.snapshot.viewport_start + u64::from(viewport_row),
                        col,
                    };
                    state.drag_viewport_cell = Some((viewport_row, col));
                    if selection.head != head {
                        selection.head = head;
                        state.interaction_version = state.interaction_version.wrapping_add(1);
                        shell.request_redraw();
                    }
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.context_menu.is_some() => {
                let hovered = state.context_menu.and_then(|menu_at| {
                    let item = cursor
                        .position_in(bounds)
                        .and_then(|position| context_item_at(menu_at, bounds.size(), position));
                    match item {
                        Some(ContextItem::Copy)
                            if state.selected_text(&self.snapshot).is_none() =>
                        {
                            None
                        }
                        item => item,
                    }
                });
                if state.context_hover != hovered {
                    state.context_hover = hovered;
                    state.interaction_version = state.interaction_version.wrapping_add(1);
                    shell.request_redraw();
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.selecting => {
                state.selecting = false;
                state.drag_viewport_cell = None;
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right))
                if cursor.is_over(bounds) =>
            {
                state.focused = true;
                state.selecting = false;
                state.context_menu = cursor.position_in(bounds);
                state.context_hover = None;
                state.interaction_version = state.interaction_version.wrapping_add(1);
                shell.request_redraw();
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                let cell_height = self.appearance.normalized().font_size * 1.15;
                let movement = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y * 3.0,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / cell_height.max(1.0),
                };
                state.wheel_remainder += movement;
                let lines = state.wheel_remainder.trunc() as i32;
                if lines != 0 {
                    state.wheel_remainder -= lines as f32;
                    state.scrolled = true;
                    state.context_menu = None;
                    state.context_hover = None;
                    state.interaction_version = state.interaction_version.wrapping_add(1);
                    shell.publish((self.on_scroll)(lines));
                    shell.request_redraw();
                }
                shell.capture_event();
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
                        if state.scrolled {
                            shell.publish((self.on_scroll)(SCROLL_TO_BOTTOM));
                            state.scrolled = false;
                        }
                        state.clear_selection();
                        state.context_menu = None;
                        state.context_hover = None;
                        state.interaction_version = state.interaction_version.wrapping_add(1);
                        shell.publish((self.on_input)(bytes));
                        shell.request_redraw();
                        shell.capture_event();
                    }
                    KeyAction::Paste => {
                        if let Some(content) = clipboard.read(clipboard::Kind::Standard)
                            && !content.is_empty()
                        {
                            if state.scrolled {
                                shell.publish((self.on_scroll)(SCROLL_TO_BOTTOM));
                                state.scrolled = false;
                            }
                            shell.publish((self.on_paste)(content.into_bytes()));
                        }
                        state.clear_selection();
                        state.context_menu = None;
                        state.context_hover = None;
                        state.interaction_version = state.interaction_version.wrapping_add(1);
                        shell.request_redraw();
                        shell.capture_event();
                    }
                    KeyAction::Copy => {
                        if let Some(content) = state.selected_text(&self.snapshot) {
                            clipboard.write(clipboard::Kind::Standard, content);
                        }
                        state.context_menu = None;
                        state.context_hover = None;
                        state.interaction_version = state.interaction_version.wrapping_add(1);
                        shell.request_redraw();
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
            interaction_version: if state.session_key == self.session_key {
                state.interaction_version
            } else {
                0
            },
        };
        if state.last_render_key.get() != Some(key) {
            state.last_render_key.set(Some(key));
            state.geometry_cache.clear();
        }
        let geometry = state.geometry_cache.draw(renderer, bounds.size(), |frame| {
            draw_grid(
                frame,
                &self.snapshot,
                DrawOptions {
                    base_font,
                    font_size: appearance.font_size,
                    cell_width,
                    cell_height,
                    cursor_on: self.cursor_on,
                    selection: (state.session_key == self.session_key)
                        .then_some(state.selection)
                        .flatten(),
                    context_menu: (state.session_key == self.session_key)
                        .then_some(state.context_menu)
                        .flatten(),
                    context_hover: (state.session_key == self.session_key)
                        .then_some(state.context_hover)
                        .flatten(),
                    copy_label: &self.copy_label,
                    paste_label: &self.paste_label,
                },
            );
        });
        renderer.with_translation(Vector::new(bounds.x, bounds.y), |renderer| {
            renderer.draw_geometry(geometry);
        });
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let bounds = layout.bounds();
        if !cursor.is_over(bounds) {
            return mouse::Interaction::None;
        }
        let state = tree.state.downcast_ref::<State>();
        let over_context_menu = state.session_key == self.session_key
            && state.context_menu.is_some_and(|menu_at| {
                cursor.position_in(bounds).is_some_and(|position| {
                    context_menu_rect(menu_at, bounds.size()).contains(position)
                })
            });
        if over_context_menu {
            mouse::Interaction::None
        } else {
            mouse::Interaction::Text
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

struct DrawOptions<'a> {
    base_font: Font,
    font_size: f32,
    cell_width: f32,
    cell_height: f32,
    cursor_on: bool,
    selection: Option<Selection>,
    context_menu: Option<Point>,
    context_hover: Option<ContextItem>,
    copy_label: &'a str,
    paste_label: &'a str,
}

fn draw_grid(canvas: &mut Frame<iced::Renderer>, grid: &ClientGrid, options: DrawOptions<'_>) {
    let DrawOptions {
        base_font,
        font_size,
        cell_width,
        cell_height,
        cursor_on,
        selection,
        context_menu,
        context_hover,
        copy_label,
        paste_label,
    } = options;
    canvas.fill_rectangle(Point::ORIGIN, canvas.size(), color(DEFAULT_BG));
    let menu_rect = context_menu.map(|position| context_menu_rect(position, canvas.size()));

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

    if let Some(selection) = selection.filter(|selection| selection.is_non_empty()) {
        let (start, end) = selection.ordered();
        for viewport_row in 0..grid.rows {
            let history_row = grid.viewport_start + u64::from(viewport_row);
            if history_row < start.row || history_row > end.row {
                continue;
            }
            let start_col = if history_row == start.row {
                start.col
            } else {
                0
            };
            let end_col = if history_row == end.row {
                end.col
            } else {
                grid.cols.saturating_sub(1)
            };
            if start_col <= end_col {
                canvas.fill_rectangle(
                    Point::new(
                        start_col as f32 * cell_width,
                        viewport_row as f32 * cell_height,
                    ),
                    Size::new((end_col - start_col + 1) as f32 * cell_width, cell_height),
                    Color::from_rgba8(0x20, 0x9c, 0x91, 0.58),
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
            let cell_x = col as f32 * cell_width;
            let cell_y = row as f32 * cell_height;
            let covered_by_menu = menu_rect.is_some_and(|menu| {
                cell_x < menu.x + menu.width
                    && cell_x + cell_width > menu.x
                    && cell_y < menu.y + menu.height
                    && cell_y + cell_height > menu.y
            });
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
                && !covered_by_menu
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
            } else if !is_spacer && !hidden && !covered_by_menu && cell.ch != ' ' && cell.ch != '\0'
            {
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

    if let Some(menu) = menu_rect {
        canvas.fill_rectangle(
            Point::new(menu.x + 4.0, menu.y + 5.0),
            Size::new(menu.width, menu.height),
            Color::from_rgba8(0, 0, 0, 0.28),
        );
        canvas.fill_rectangle(
            Point::new(menu.x, menu.y),
            Size::new(menu.width, menu.height),
            Color::from_rgb8(0x19, 0x24, 0x22),
        );
        if let Some(item) = context_hover {
            let y = menu.y
                + match item {
                    ContextItem::Copy => 0.0,
                    ContextItem::Paste => CONTEXT_ITEM_HEIGHT,
                };
            canvas.fill_rectangle(
                Point::new(menu.x + 3.0, y + 2.0),
                Size::new(menu.width - 6.0, CONTEXT_ITEM_HEIGHT - 4.0),
                Color::from_rgb8(0x20, 0x4a, 0x45),
            );
        }
        canvas.fill_rectangle(
            Point::new(menu.x, menu.y + CONTEXT_ITEM_HEIGHT),
            Size::new(menu.width, 1.0),
            Color::from_rgb8(0x31, 0x45, 0x42),
        );
        let copy_color = if selection.is_some_and(Selection::is_non_empty) {
            Color::from_rgb8(0xe4, 0xeb, 0xe9)
        } else {
            Color::from_rgb8(0x65, 0x76, 0x73)
        };
        draw_text(
            canvas,
            copy_label.to_string(),
            Point::new(menu.x + 14.0, menu.y + 8.0),
            copy_color,
            13.0,
            Font::DEFAULT,
        );
        draw_text(
            canvas,
            paste_label.to_string(),
            Point::new(menu.x + 14.0, menu.y + CONTEXT_ITEM_HEIGHT + 8.0),
            Color::from_rgb8(0xe4, 0xeb, 0xe9),
            13.0,
            Font::DEFAULT,
        );
    }
}

fn context_menu_rect(position: Point, bounds: Size) -> Rectangle {
    let height = CONTEXT_ITEM_HEIGHT * 2.0;
    Rectangle {
        x: position.x.min((bounds.width - CONTEXT_MENU_WIDTH).max(0.0)),
        y: position.y.min((bounds.height - height).max(0.0)),
        width: CONTEXT_MENU_WIDTH,
        height,
    }
}

fn context_item_at(menu_at: Point, bounds: Size, position: Point) -> Option<ContextItem> {
    let menu = context_menu_rect(menu_at, bounds);
    if !menu.contains(position) {
        return None;
    }
    if position.y < menu.y + CONTEXT_ITEM_HEIGHT {
        Some(ContextItem::Copy)
    } else {
        Some(ContextItem::Paste)
    }
}

fn point_to_viewport_cell(
    position: Point,
    appearance: &TerminalAppearance,
    grid: &ClientGrid,
) -> Option<(u16, u16)> {
    if grid.rows == 0 || grid.cols == 0 || position.x < 0.0 || position.y < 0.0 {
        return None;
    }
    let appearance = appearance.normalized();
    let font = terminal_font(&appearance.font_family, false, false);
    let cell_width = cell_advance(font, appearance.font_size).max(1.0);
    let cell_height = (appearance.font_size * 1.15).max(1.0);
    Some((
        ((position.y / cell_height).floor() as u16).min(grid.rows - 1),
        ((position.x / cell_width).floor() as u16).min(grid.cols - 1),
    ))
}

fn selected_text(
    rows: &BTreeMap<u64, Vec<super::client_grid::ClientCell>>,
    cols: u16,
    selection: Option<Selection>,
) -> Option<String> {
    let selection = selection.filter(|selection| selection.is_non_empty())?;
    let (start, end) = selection.ordered();
    let mut output = String::new();
    for row in start.row..=end.row {
        let start_col = if row == start.row { start.col } else { 0 };
        let end_col = if row == end.row {
            end.col.min(cols.saturating_sub(1))
        } else {
            cols.saturating_sub(1)
        };
        let mut line = String::new();
        for col in start_col..=end_col {
            let Some(cell) = rows.get(&row).and_then(|cells| cells.get(col as usize)) else {
                continue;
            };
            if cell.flags & super::client_grid::cell_flags::WIDE_SPACER == 0 {
                line.push(cell.ch);
            }
        }
        output.push_str(line.trim_end_matches([' ', '\0']));
        if row != end.row {
            output.push('\n');
        }
    }
    (!output.is_empty()).then_some(output)
}

/// 把 TermCanvas 变成 Element。
pub fn canvas<'a, Message>(
    session_key: String,
    snapshot: Arc<ClientGrid>,
    viewport_metrics: Arc<ViewportMetrics>,
    callbacks: Callbacks<Message>,
    appearance: TerminalAppearance,
    cursor_on: bool,
    labels: ContextLabels,
) -> Element<'a, Message>
where
    Message: 'a,
{
    Element::new(TermCanvas::new(
        session_key,
        snapshot,
        viewport_metrics,
        callbacks,
        appearance,
        cursor_on,
        labels,
    ))
}

#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    Bytes(Vec<u8>),
    Paste,
    Copy,
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
    if is_copy_shortcut(key, physical_key, modifiers) {
        return KeyAction::Copy;
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

fn is_copy_shortcut(key: &Key, physical_key: key::Physical, modifiers: Modifiers) -> bool {
    let is_c = key
        .to_latin(physical_key)
        .is_some_and(|ch| ch.eq_ignore_ascii_case(&'c'));
    #[cfg(target_os = "macos")]
    {
        is_c && modifiers.logo() && !modifiers.control() && !modifiers.alt()
    }
    #[cfg(not(target_os = "macos"))]
    {
        is_c && modifiers.control() && modifiers.shift() && !modifiers.alt()
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

    fn grid_with_text(lines: &[&str], cols: u16) -> ClientGrid {
        let mut grid = ClientGrid::new(lines.len() as u16, cols);
        let updates = lines
            .iter()
            .enumerate()
            .map(|(row, line)| crate::term::frame::LineUpdate {
                row: row as u16,
                start_col: 0,
                end_col: cols - 1,
                runs: line
                    .chars()
                    .map(|ch| crate::term::frame::Run {
                        len: 1,
                        flags: 0,
                        fg: super::super::client_grid::ColorSpec::Default,
                        bg: super::super::client_grid::ColorSpec::Default,
                        ch,
                    })
                    .collect(),
            })
            .collect();
        grid.apply_frame(&crate::term::frame::TerminalFrame {
            seq: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            viewport_start: 0,
            lines: updates,
        });
        grid
    }

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
    fn selection_extracts_rows_and_trims_unselected_padding() {
        let grid = grid_with_text(&["hello ", "world "], 6);
        let rows = (0..grid.rows)
            .filter_map(|row| {
                grid.line_cells(row)
                    .map(|cells| (u64::from(row), cells.to_vec()))
            })
            .collect();
        let selection = Selection {
            anchor: CellPosition { row: 0, col: 1 },
            head: CellPosition { row: 1, col: 4 },
        };
        assert_eq!(
            selected_text(&rows, grid.cols, Some(selection)).as_deref(),
            Some("ello\nworld")
        );
        assert_eq!(
            selected_text(
                &rows,
                grid.cols,
                Some(Selection {
                    anchor: selection.head,
                    head: selection.anchor,
                })
            ),
            Some("ello\nworld".to_string())
        );
    }

    #[test]
    fn selection_keeps_cached_rows_across_scrollback_viewports() {
        let mut bottom = grid_with_text(&["two  ", "three"], 5);
        bottom.viewport_start = 2;
        let mut older = grid_with_text(&["zero ", "one  "], 5);
        older.viewport_start = 0;

        let mut state = State {
            selection: Some(Selection {
                anchor: CellPosition { row: 3, col: 4 },
                head: CellPosition { row: 3, col: 4 },
            }),
            ..State::default()
        };
        state.cache_snapshot(&bottom);
        state.cache_snapshot(&older);
        state.selection.as_mut().unwrap().head = CellPosition { row: 0, col: 0 };

        assert_eq!(
            state.selected_text(&older).as_deref(),
            Some("zero\none\ntwo\nthree")
        );
    }

    #[test]
    fn context_menu_is_clamped_inside_terminal() {
        let rect = context_menu_rect(Point::new(990.0, 740.0), Size::new(1000.0, 750.0));
        assert_eq!(rect.x, 852.0);
        assert_eq!(rect.y, 682.0);
        assert_eq!(rect.width, CONTEXT_MENU_WIDTH);
        assert_eq!(
            context_item_at(
                Point::new(990.0, 740.0),
                Size::new(1000.0, 750.0),
                Point::new(860.0, 690.0)
            ),
            Some(ContextItem::Copy)
        );
        assert_eq!(
            context_item_at(
                Point::new(990.0, 740.0),
                Size::new(1000.0, 750.0),
                Point::new(860.0, 725.0)
            ),
            Some(ContextItem::Paste)
        );
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
