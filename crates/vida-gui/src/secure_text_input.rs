use iced::advanced::widget::Tree;
use iced::advanced::{layout, mouse, overlay, renderer, Clipboard, Shell, Widget};
use iced::widget::text_input::{self, Status, Style};
use iced::{Element, Event, Length, Rectangle, Size, Vector};
use iced::advanced::InputMethod;
use iced::advanced::layout::Layout;
use iced::window;

/// A wrapper around `text_input` that disables IME for secure fields.
///
/// This solves the security issue where Chinese IME would intercept
/// keystrokes in password fields, potentially leaking to cloud IME services.
///
/// # iced 依赖声明 (升级前必读)
///
/// 本实现依赖 iced 0.14 的三条**未在文档中承诺**的内部行为：
///
/// 1. **IME 事件类型**：当 IME 启用时，winit 发送 `Event::InputMethod`
///    而非 `Event::Keyboard`。如果 iced 升级后改变了事件映射逻辑，
///    IME 会静默重新启用。
///
/// 2. **键盘事件仅到达 focused widget**：`Event::Keyboard` 事件只被
///    focused 的 text_input 处理。如果 iced 改变了事件分发逻辑（如
///    广播到所有 widget），非 focused 的 SecureTextInput 可能干扰兄弟。
///
/// 3. **Shell::input_method 差分检测焦点**：`text_input` 仅在
///    focused + window focused 时调用 `request_input_method`（line 1353）。
///    对比 update 前后 `shell.input_method()` 状态可推断内部 widget
///    是否 focused。如果 iced 改变了 `request_input_method` 的调用时机，
///    焦点检测会失效。
///
/// **升级检查清单**：若 iced 升级后中文输入法在口令框出现候选窗：
/// - 先检查 `Event::InputMethod` 是否仍被 winit 正确映射
/// - 再检查 `text_input` 的 `request_input_method` 调用条件
/// - 最后验证 `shell.input_method()` 差分逻辑
/// - 详见 `docs/decisions.md` IME 安全缺陷分析
pub struct SecureTextInput<'a, Message: Clone> {
    inner: text_input::TextInput<'a, Message>,
    width: Length,
}

impl<'a, Message: Clone + 'a> SecureTextInput<'a, Message> {
    /// Create a new secure text input that disables IME.
    pub fn new(placeholder: &str, value: &str) -> Self {
        Self {
            inner: text_input::TextInput::new(placeholder, value),
            width: Length::Fill,
        }
    }

    /// Convert into an Element.
    pub fn into_element(self) -> Element<'a, Message> {
        Element::new(self)
    }

    pub fn on_input(mut self, message: impl Fn(String) -> Message + 'a) -> Self {
        self.inner = self.inner.on_input(message);
        self
    }

    pub fn on_submit(mut self, message: Message) -> Self {
        self.inner = self.inner.on_submit(message);
        self
    }

    pub fn secure(mut self, is_secure: bool) -> Self {
        self.inner = self.inner.secure(is_secure);
        self
    }

    pub fn style(mut self, style: impl Fn(&iced::Theme, Status) -> Style + 'a) -> Self {
        self.inner = self.inner.style(style);
        self
    }

    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    pub fn padding(mut self, padding: impl Into<iced::Padding>) -> Self {
        self.inner = self.inner.padding(padding);
        self
    }

    pub fn size(mut self, size: impl Into<iced::Pixels>) -> Self {
        self.inner = self.inner.size(size);
        self
    }

    pub fn id(mut self, id: impl Into<iced::widget::Id>) -> Self {
        self.inner = self.inner.id(id);
        self
    }
}

impl<'a, Message> Widget<Message, iced::Theme, iced::Renderer> for SecureTextInput<'a, Message>
where
    Message: Clone + 'a,
{
    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: Length::Shrink,
        }
    }

    fn size_hint(&self) -> Size<Length> {
        self.size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::layout(
            &mut self.inner,
            tree,
            renderer,
            limits,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::draw(
            &self.inner,
            tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        )
    }

    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::tag(&self.inner)
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::state(&self.inner)
    }

    fn children(&self) -> Vec<Tree> {
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::children(&self.inner)
    }

    fn diff(&self, tree: &mut Tree) {
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::diff(&self.inner, tree)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::operation::Operation,
    ) {
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::operate(
            &mut self.inner,
            tree,
            layout,
            renderer,
            operation,
        )
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Block IME events entirely. When IME is enabled, winit sends
        // Ime::Preedit/Ime::Commit instead of Keyboard::KeyPressed.
        // The inner text_input commits Chinese characters via Ime::Commit
        // BEFORE any post-update guard can run. Blocking prevents this.
        if let Event::InputMethod(_) = event {
            *shell.input_method_mut() = InputMethod::Disabled;
            return;
        }

        // Snapshot shell state before inner update. If the inner text_input
        // calls request_input_method (which only happens when it's focused),
        // the shell state will change. This is how we detect focus without
        // accessing the inner widget's private state.
        let before = shell.input_method().clone();

        // Let inner widget handle non-IME events
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::update(
            &mut self.inner,
            tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        let after = shell.input_method().clone();

        // Determine if we should write Disabled:
        // 1. Keyboard events: always write (only focused widget gets these;
        //    prevents IME from being re-enabled on next RedrawRequested)
        // 2. RedrawRequested: write only if the inner widget changed the
        //    shell state (meaning it's focused and called request_input_method).
        //    This avoids overriding sibling widgets' IME requests.
        let should_disable = match event {
            Event::Keyboard(_) => true,
            Event::Window(window::Event::RedrawRequested(_)) => before != after,
            _ => false,
        };

        if should_disable {
            *shell.input_method_mut() = InputMethod::Disabled;
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::mouse_interaction(
            &self.inner,
            tree,
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        <text_input::TextInput<'a, Message> as Widget<Message, iced::Theme, iced::Renderer>>::overlay(
            &mut self.inner,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message> From<SecureTextInput<'a, Message>> for Element<'a, Message>
where
    Message: Clone + 'a,
{
    fn from(widget: SecureTextInput<'a, Message>) -> Self {
        Element::new(widget)
    }
}
