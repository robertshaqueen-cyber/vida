use iced::widget::{button, column, container, text};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;

#[derive(Debug, Clone)]
pub struct State {
    pub error_message: String,
}

impl State {
    pub fn new(error: String) -> Self {
        Self { error_message: error }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text("vida").size(32);
        let subtitle = text(i18n.tr("connection_title")).size(18);
        let error_text = text(&self.error_message).size(14);

        let retry_btn = button(i18n.tr("connection_retry"))
            .on_press(AppMessage::RetryConnection);

        let content = column![title, subtitle, error_text, retry_btn]
            .spacing(16)
            .padding(40);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }
}
