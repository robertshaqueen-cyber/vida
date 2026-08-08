use iced::widget::{button, column, container, row, text};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::ui::{self, icons};

#[derive(Debug, Clone)]
pub struct State {
    pub error_message: String,
}

impl State {
    pub fn new(error: String) -> Self {
        Self {
            error_message: error,
        }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let mark = container(icons::icon(icons::TERMINAL, 24).color(ui::ACCENT))
            .center_x(54)
            .center_y(54)
            .style(ui::accent_badge);
        let title = text("vida").size(28);
        let subtitle = ui::muted(i18n.tr("connection_title")).size(14);
        let error_text = ui::muted(&self.error_message).size(12);

        let retry_btn = button(i18n.tr("connection_retry"))
            .on_press(AppMessage::RetryConnection)
            .style(ui::primary_button)
            .padding([10, 18]);

        let details = column![title, subtitle, error_text, retry_btn]
            .spacing(10)
            .width(Length::Fill);
        let content = container(
            row![mark, details]
                .spacing(20)
                .align_y(iced::Alignment::Center),
        )
        .padding(24)
        .width(600)
        .style(ui::elevated);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(ui::app_background)
            .into()
    }
}
