use iced::widget::{button, column, container, text, text_input};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;

#[derive(Debug, Clone)]
pub struct State {
    pub passphrase: String,
    pub remember: bool,
    pub error: Option<String>,
    pub unlocking: bool,
}

impl State {
    pub fn new() -> Self {
        Self {
            passphrase: String::new(),
            remember: false,
            error: None,
            unlocking: false,
        }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text("vida").size(32);
        let subtitle = text(i18n.tr("unlock_title")).size(18);

        let pass_input = text_input(i18n.tr("unlock_passphrase_placeholder"), &self.passphrase)
            .on_input(AppMessage::UnlockPassphraseChanged)
            .secure(true);

        let remember_check = iced::widget::checkbox(self.remember)
            .label(i18n.tr("unlock_remember"))
            .on_toggle(AppMessage::UnlockRememberToggled);

        let can_unlock = !self.passphrase.is_empty() && !self.unlocking;

        let unlock_btn = if self.unlocking {
            button(i18n.tr("unlock_unlocking"))
        } else {
            button(i18n.tr("unlock_unlock"))
        };

        let unlock_btn = if can_unlock {
            unlock_btn.on_press(AppMessage::UnlockVault)
        } else {
            unlock_btn.style(button::secondary)
        };

        let unlock_btn = container(unlock_btn)
            .width(Length::Shrink)
            .center_x(Length::Fill);

        let error_text = match &self.error {
            Some(e) => text(e).size(14),
            None => text(""),
        };

        let content = column![title, subtitle, pass_input, error_text, remember_check, unlock_btn]
            .spacing(10)
            .padding(40)
            .max_width(400);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }
}
