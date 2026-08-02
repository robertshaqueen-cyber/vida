use iced::widget::{button, column, container, text};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;

#[derive(Debug, Clone)]
pub struct State;

impl State {
    pub fn new() -> Self {
        Self
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("remote_missing_title")).size(24);
        let subtitle = text(i18n.tr("remote_missing_subtitle")).size(14);

        let reupload_btn = button(i18n.tr("remote_missing_reupload"))
            .on_press(AppMessage::RemoteMissingAction("reupload".to_string()));
        let clear_btn = button(i18n.tr("remote_missing_clear"))
            .on_press(AppMessage::RemoteMissingAction("clear_state".to_string()));

        let content = column![title, subtitle, reupload_btn, clear_btn]
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
