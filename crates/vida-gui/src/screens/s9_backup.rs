use iced::widget::{button, column, container, text, text_input};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;

#[derive(Debug, Clone)]
pub struct State {
    pub export_passphrase: String,
    pub use_current: bool,
    pub exporting: bool,
    pub result: Option<String>,
    pub error: Option<String>,
}

impl State {
    pub fn new() -> Self {
        Self {
            export_passphrase: String::new(),
            use_current: true,
            exporting: false,
            result: None,
            error: None,
        }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("backup_title")).size(24);
        let subtitle = text(i18n.tr("backup_subtitle")).size(14);

        let current_check = iced::widget::checkbox(self.use_current)
            .label(i18n.tr("backup_use_current"))
            .on_toggle(AppMessage::BackupUseCurrentToggled);

        let pass_input = text_input(i18n.tr("backup_new_passphrase"), &self.export_passphrase)
            .on_input(AppMessage::BackupPassphraseChanged)
            .secure(true)
            .width(Length::Fill);

        let can_export = !self.exporting
            && (self.use_current || !self.export_passphrase.is_empty());

        let export_btn = if self.exporting {
            button(i18n.tr("backup_exporting")).width(Length::Shrink)
        } else {
            button(i18n.tr("backup_title")).width(Length::Shrink)
        };

        let export_btn = if can_export {
            export_btn.on_press(AppMessage::BackupExport)
        } else {
            export_btn
        };

        let back_btn = button(i18n.tr("common_back")).on_press(AppMessage::BackupBack);

        let result_text = match &self.result {
            Some(r) => text(r).size(12),
            None => text(""),
        };

        let error_text = match &self.error {
            Some(e) => text(e).size(12),
            None => text(""),
        };

        let content = column![
            title,
            subtitle,
            current_check,
            pass_input,
            export_btn,
            result_text,
            error_text,
            back_btn,
        ]
        .spacing(12)
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
