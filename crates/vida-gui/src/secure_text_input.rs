use iced::advanced::widget::Tree;
use iced::advanced::{layout, mouse, overlay, renderer, Clipboard, Shell, Widget};
use iced::widget::text_input::{self, Status, Style};
use iced::{Element, Event, Length, Rectangle, Size, Vector};
use iced::advanced::InputMethod;
use iced::advanced::layout::Layout;

/// A wrapper around `text_input` that disables IME for secure fields.
///
/// This solves the security issue where Chinese IME would intercept
/// keystrokes in password fields, potentially leaking to cloud IME services.
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
        // Let inner widget handle the event (it will call shell.request_input_method())
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

        // Override: force IME to be disabled for secure fields
        // This prevents Chinese IME from intercepting keystrokes
        *shell.input_method_mut() = InputMethod::Disabled;
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
