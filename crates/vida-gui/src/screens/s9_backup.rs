use iced::widget::{button, column, container, row, rule, text};
use iced::{Alignment, Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::secure_text_input::SecureTextInput;
use crate::ui::{self, icons};

#[derive(Debug, Clone)]
pub struct RestorePreview {
    pub host_count: usize,
    pub modified_at: String,
    pub host_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct State {
    pub export_passphrase: String,
    pub use_current: bool,
    pub exporting: bool,
    pub result: Option<String>,
    pub error: Option<String>,
    pub restore_path: String,
    pub restore_passphrase: String,
    pub restore_data: Vec<u8>,
    pub restore_preview: Option<RestorePreview>,
    pub restore_validating: bool,
    pub restoring: bool,
    pub restore_result: Option<String>,
    pub restore_error: Option<String>,
}

impl State {
    pub fn new() -> Self {
        Self {
            export_passphrase: String::new(),
            use_current: true,
            exporting: false,
            result: None,
            error: None,
            restore_path: String::new(),
            restore_passphrase: String::new(),
            restore_data: Vec::new(),
            restore_preview: None,
            restore_validating: false,
            restoring: false,
            restore_result: None,
            restore_error: None,
        }
    }

    pub fn view_settings_content(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let subtitle = ui::muted(i18n.tr("backup_subtitle")).size(12);

        let current_check = iced::widget::checkbox(self.use_current)
            .label(i18n.tr("backup_use_current"))
            .on_toggle(AppMessage::BackupUseCurrentToggled);

        let pass_input =
            SecureTextInput::new(i18n.tr("backup_new_passphrase"), &self.export_passphrase)
                .on_input(AppMessage::BackupPassphraseChanged)
                .secure(true)
                .style(ui::input)
                .padding(10)
                .width(Length::Fill);

        let can_export =
            !self.exporting && (self.use_current || !self.export_passphrase.is_empty());

        let export_btn = if self.exporting {
            button(i18n.tr("backup_exporting"))
                .width(Length::Shrink)
                .style(ui::primary_button)
                .padding([9, 14])
        } else {
            button(
                row![
                    icons::icon(icons::SHIELD, 14),
                    text(i18n.tr("backup_title")).size(13),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .width(Length::Shrink)
            .style(ui::primary_button)
            .padding([9, 14])
        };

        let export_btn = if can_export {
            export_btn.on_press(AppMessage::BackupExport)
        } else {
            export_btn
        };

        let mut content = column![subtitle, current_check, pass_input, export_btn,]
            .spacing(12)
            .width(Length::Fill);

        if let Some(result) = &self.result {
            content = content.push(
                row![
                    icons::icon(icons::CIRCLE_CHECK, 15).color(ui::SUCCESS),
                    text(result)
                        .size(12)
                        .color(ui::SUCCESS)
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .width(Length::Fill),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .width(Length::Fill),
            );
        }

        if let Some(error) = &self.error {
            content = content.push(
                container(
                    row![
                        icons::icon(icons::CIRCLE_ALERT, 15).color(ui::DANGER_TEXT),
                        text(error).size(12).color(ui::DANGER_TEXT),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([8, 10])
                .width(Length::Fill)
                .style(ui::error_notice),
            );
        }

        let restore_path = if self.restore_path.is_empty() {
            i18n.tr("backup_restore_no_file")
        } else {
            &self.restore_path
        };
        let path_row = row![
            container(
                text(restore_path)
                    .size(12)
                    .color(if self.restore_path.is_empty() {
                        ui::TEXT_MUTED
                    } else {
                        ui::TEXT_PRIMARY
                    })
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .width(Length::Fill),
            )
            .padding([10, 11])
            .width(Length::Fill)
            .style(ui::input_container),
            button(i18n.tr("backup_restore_choose_file"))
                .on_press(AppMessage::BackupRestoreChooseFile)
                .style(ui::secondary_button)
                .padding([9, 12]),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let restore_passphrase = SecureTextInput::new(
            i18n.tr("backup_restore_passphrase"),
            &self.restore_passphrase,
        )
        .on_input(AppMessage::BackupRestorePassphraseChanged)
        .secure(true)
        .style(ui::input)
        .padding(10)
        .width(Length::Fill);

        let can_validate = !self.restore_path.is_empty()
            && !self.restore_passphrase.is_empty()
            && !self.restore_validating
            && !self.restoring;
        let validate_button = button(if self.restore_validating {
            i18n.tr("backup_restore_validating")
        } else {
            i18n.tr("backup_restore_validate")
        })
        .style(ui::secondary_button)
        .padding([9, 14]);
        let validate_button = if can_validate {
            validate_button.on_press(AppMessage::BackupRestorePreview)
        } else {
            validate_button
        };

        let mut restore = column![
            text(i18n.tr("backup_restore_title")).size(16),
            ui::muted(i18n.tr("backup_restore_subtitle")).size(12),
            path_row,
            restore_passphrase,
            validate_button,
        ]
        .spacing(11)
        .width(Length::Fill);

        if let Some(preview) = &self.restore_preview {
            let names = if preview.host_names.is_empty() {
                i18n.tr("backup_restore_empty").to_string()
            } else {
                preview.host_names.join("、")
            };
            let summary = i18n.trf(
                "backup_restore_preview",
                &[&preview.host_count.to_string(), &preview.modified_at],
            );
            restore = restore.push(
                container(
                    column![
                        row![
                            icons::icon(icons::CIRCLE_CHECK, 15).color(ui::SUCCESS),
                            text(summary).size(12).color(ui::SUCCESS),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                        text(i18n.trf("backup_restore_hosts", &[&names]))
                            .size(12)
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                    ]
                    .spacing(7),
                )
                .padding([10, 12])
                .width(Length::Fill)
                .style(ui::success_notice),
            );
            restore = restore.push(
                container(
                    row![
                        icons::icon(icons::CIRCLE_ALERT, 15).color(ui::DANGER_TEXT),
                        text(i18n.tr("backup_restore_warning"))
                            .size(12)
                            .color(ui::DANGER_TEXT)
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .width(Length::Fill),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([9, 11])
                .width(Length::Fill)
                .style(ui::error_notice),
            );
            let confirm = button(if self.restoring {
                i18n.tr("backup_restoring")
            } else {
                i18n.tr("backup_restore_confirm")
            })
            .style(ui::danger_button)
            .padding([9, 14]);
            restore = restore.push(if self.restoring {
                confirm
            } else {
                confirm.on_press(AppMessage::BackupRestoreConfirm)
            });
        }

        if let Some(result) = &self.restore_result {
            restore = restore.push(
                row![
                    icons::icon(icons::CIRCLE_CHECK, 15).color(ui::SUCCESS),
                    text(result)
                        .size(12)
                        .color(ui::SUCCESS)
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .width(Length::Fill),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }

        if let Some(error) = &self.restore_error {
            restore = restore.push(
                container(
                    row![
                        icons::icon(icons::CIRCLE_ALERT, 15).color(ui::DANGER_TEXT),
                        text(error)
                            .size(12)
                            .color(ui::DANGER_TEXT)
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .width(Length::Fill),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .padding([8, 10])
                .width(Length::Fill)
                .style(ui::error_notice),
            );
        }

        content
            .push(rule::horizontal(1))
            .push(restore)
            .spacing(16)
            .into()
    }
}
