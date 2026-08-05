use iced::widget::{container, text};
use iced::{Element, Length, Theme};

/// Toast notification style variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ToastKind {
    /// Red background for errors.
    Error,
    /// Green background for success.
    Success,
    /// Blue background for info.
    Info,
    /// Yellow/orange background for warnings.
    Warning,
}

#[allow(dead_code)]
impl ToastKind {
    /// Background color for the toast.
    fn background_color(&self) -> iced::Color {
        match self {
            ToastKind::Error => iced::Color::from_rgb(0.8, 0.2, 0.2),
            ToastKind::Success => iced::Color::from_rgb(0.2, 0.7, 0.3),
            ToastKind::Info => iced::Color::from_rgb(0.2, 0.5, 0.8),
            ToastKind::Warning => iced::Color::from_rgb(0.9, 0.6, 0.1),
        }
    }

    /// Text color for the toast.
    fn text_color(&self) -> iced::Color {
        iced::Color::WHITE
    }
}

/// A reusable toast notification widget.
#[allow(dead_code)]
pub struct Toast<'a, Message> {
    content: Element<'a, Message>,
    kind: ToastKind,
}

#[allow(dead_code)]
impl<'a, Message: 'a> Toast<'a, Message> {
    /// Create a new toast with text content.
    pub fn new(text_content: &'a str, kind: ToastKind) -> Self {
        Self {
            content: text(text_content).color(kind.text_color()).size(14).into(),
            kind,
        }
    }

    /// Convert the toast into an Element.
    pub fn into_element(self) -> Element<'a, Message> {
        let background = self.kind.background_color();

        container(self.content)
            .padding(iced::Padding::from([10, 20]))
            .style(move |_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(background)),
                border: iced::Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .center_y(20)
            .width(Length::Fill)
            .into()
    }
}
