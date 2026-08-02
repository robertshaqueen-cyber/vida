use iced::widget::{button, column, container, text, text_input};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;

#[derive(Debug, Clone)]
pub struct State {
    pub passphrase: String,
    pub confirm_passphrase: String,
    pub risk_confirmed: bool,
    pub error: Option<String>,
    pub creating: bool,
}

impl State {
    pub fn new() -> Self {
        Self {
            passphrase: String::new(),
            confirm_passphrase: String::new(),
            risk_confirmed: false,
            error: None,
            creating: false,
        }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text("vida").size(32);
        let subtitle = text(i18n.tr("setup_title")).size(18);

        let pass_input = text_input(i18n.tr("setup_passphrase_placeholder"), &self.passphrase)
            .on_input(AppMessage::SetupPassphraseChanged)
            .secure(true);

        let confirm_input = text_input(i18n.tr("setup_confirm_placeholder"), &self.confirm_passphrase)
            .on_input(AppMessage::SetupConfirmPassphraseChanged)
            .secure(true);

        let mismatch = if !self.confirm_passphrase.is_empty()
            && self.passphrase != self.confirm_passphrase
        {
            text(i18n.tr("setup_passphrase_mismatch")).size(12)
        } else {
            text("")
        };

        // Risk warning text
        let risk_warning = text(i18n.tr("setup_risk_warning"))
            .size(13);

        let risk_check = iced::widget::checkbox(self.risk_confirmed)
            .label(i18n.tr("setup_risk_checkbox"))
            .on_toggle(AppMessage::SetupRiskConfirmed);

        let can_create = !self.passphrase.is_empty()
            && self.passphrase == self.confirm_passphrase
            && self.risk_confirmed
            && !self.creating;

        let create_btn = if self.creating {
            button(i18n.tr("setup_creating"))
        } else {
            button(i18n.tr("setup_create"))
        };

        let create_btn = if can_create {
            create_btn.on_press(AppMessage::SetupCreateVault)
        } else {
            create_btn.style(button::secondary)
        };

        let create_btn = container(create_btn)
            .width(Length::Shrink)
            .center_x(Length::Fill);

        let error_text = match &self.error {
            Some(e) => text(e).size(14),
            None => text(""),
        };

        let content = column![
            title,
            subtitle,
            pass_input,
            confirm_input,
            mismatch,
            risk_warning,
            risk_check,
            create_btn,
            error_text,
        ]
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
