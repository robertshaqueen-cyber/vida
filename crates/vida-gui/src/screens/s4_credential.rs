use iced::widget::{button, column, container, row, text, text_input};
use iced::{Alignment, Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::secure_text_input::SecureTextInput;
use crate::ui::{self, icons};

#[derive(Debug, Clone, PartialEq)]
pub enum EditorMode {
    Add,
    Edit { host_id: String },
}

#[derive(Debug, Clone)]
pub struct State {
    pub mode: EditorMode,
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: String,
    pub tags: String,
    pub group: String,
    #[allow(dead_code)] // reserved for future use
    pub color: String,
    pub password: String,
    pub notes: String,
    pub saving: bool,
    pub error: Option<String>,
    /// True when user cleared the password field and we showed the intercept dialog.
    pub password_cleared: bool,
}

impl State {
    /// Create editor for adding a new host.
    pub fn new_add() -> Self {
        Self {
            mode: EditorMode::Add,
            name: String::new(),
            host: String::new(),
            user: "root".to_string(),
            port: "22".to_string(),
            tags: String::new(),
            group: String::new(),
            color: String::new(),
            password: String::new(),
            notes: String::new(),
            saving: false,
            error: None,
            password_cleared: false,
        }
    }

    /// Create editor pre-filled with existing host data for editing.
    #[allow(clippy::too_many_arguments)] // host fields map 1:1 to HostEntry
    pub fn new_edit(
        host_id: String,
        name: String,
        host: String,
        user: String,
        port: u16,
        tags: Vec<String>,
        group: Option<String>,
        color: Option<String>,
        notes: Option<String>,
    ) -> Self {
        Self {
            mode: EditorMode::Edit { host_id },
            name,
            host,
            user,
            port: port.to_string(),
            tags: tags.join(", "),
            group: group.unwrap_or_default(),
            color: color.unwrap_or_default(),
            password: String::new(), // empty = keep existing
            notes: notes.unwrap_or_default(),
            saving: false,
            error: None,
            password_cleared: false,
        }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let is_edit = matches!(self.mode, EditorMode::Edit { .. });
        let title_copy = if is_edit {
            i18n.tr("editor_title_edit")
        } else {
            i18n.tr("editor_title_add")
        };
        let title = text(title_copy).size(24);
        let header_icon = if is_edit {
            icons::PENCIL
        } else {
            icons::SERVER
        };
        let header = row![
            container(icons::icon(header_icon, 20).color(ui::ACCENT))
                .center_x(46)
                .center_y(46)
                .style(ui::accent_badge),
            column![
                title,
                ui::muted(if is_edit {
                    i18n.tr("editor_hint_keep_password")
                } else {
                    i18n.tr("editor_hint_need_password")
                })
                .size(12),
            ]
            .spacing(4),
        ]
        .spacing(13)
        .align_y(Alignment::Center);

        let name_input = text_input(i18n.tr("editor_name"), &self.name)
            .on_input(AppMessage::EditorNameChanged)
            .style(ui::input)
            .padding(10)
            .width(Length::Fill);

        let host_input = text_input(i18n.tr("editor_host"), &self.host)
            .on_input(AppMessage::EditorHostChanged)
            .style(ui::input)
            .padding(10)
            .width(Length::Fill);

        let user_input = text_input(i18n.tr("editor_user"), &self.user)
            .on_input(AppMessage::EditorUserChanged)
            .style(ui::input)
            .padding(10)
            .width(Length::Fill);

        let port_input = text_input(i18n.tr("editor_port"), &self.port)
            .on_input(AppMessage::EditorPortChanged)
            .style(ui::input)
            .padding(10)
            .width(Length::Fill);

        let password_label = if is_edit {
            text(i18n.tr("editor_password_edit_hint")).size(12)
        } else {
            text(i18n.tr("editor_password")).size(12)
        };
        let password_input = SecureTextInput::new(i18n.tr("editor_password"), &self.password)
            .on_input(AppMessage::EditorPasswordChanged)
            .secure(true)
            .style(ui::input)
            .padding(10)
            .width(Length::Fill);

        let tags_input = text_input(i18n.tr("editor_tags"), &self.tags)
            .on_input(AppMessage::EditorTagsChanged)
            .style(ui::input)
            .padding(10)
            .width(Length::Fill);

        let group_input = text_input(i18n.tr("editor_group"), &self.group)
            .on_input(AppMessage::EditorGroupChanged)
            .style(ui::input)
            .padding(10)
            .width(Length::Fill);

        let notes_input = text_input(i18n.tr("editor_notes"), &self.notes)
            .on_input(AppMessage::EditorNotesChanged)
            .style(ui::input)
            .padding(10)
            .width(Length::Fill);

        let can_save = !self.saving
            && !self.name.is_empty()
            && !self.host.is_empty()
            && !self.user.is_empty()
            && self.port.parse::<u16>().is_ok()
            && (is_edit || !self.password.is_empty()); // new host requires password

        let save_btn = if self.saving {
            button(i18n.tr("common_saving"))
                .width(Length::Shrink)
                .style(ui::primary_button)
                .padding([9, 14])
        } else {
            button(i18n.tr("common_save"))
                .width(Length::Shrink)
                .style(ui::primary_button)
                .padding([9, 14])
        };

        let save_btn = if can_save {
            save_btn.on_press(AppMessage::EditorSave)
        } else {
            save_btn
        };

        let cancel_btn = button(i18n.tr("common_cancel"))
            .on_press(AppMessage::EditorCancel)
            .style(ui::secondary_button)
            .padding([9, 14]);

        let hint = if is_edit {
            ui::muted(i18n.tr("editor_hint_keep_password")).size(11)
        } else {
            ui::muted(i18n.tr("editor_hint_need_password")).size(11)
        };

        let fields = column![
            name_input,
            host_input,
            row![user_input, port_input].spacing(12),
            password_label.color(ui::TEXT_SECONDARY),
            password_input,
            hint,
            tags_input,
            group_input,
            notes_input,
        ]
        .spacing(11)
        .width(Length::Fill);

        let mut card = column![header, fields, row![save_btn, cancel_btn].spacing(10)]
            .spacing(18)
            .width(Length::Fill);

        if let Some(error) = &self.error {
            card = card.push(
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

        let content = container(card)
            .padding(22)
            .width(Length::Fixed(620.0))
            .style(ui::surface);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .padding(iced::padding::Padding::new(0.0).top(62))
            .into()
    }
}
