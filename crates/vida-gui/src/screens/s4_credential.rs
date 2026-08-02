use iced::widget::{button, column, container, row, text, text_input};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;

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
        let title = if is_edit {
            text(i18n.tr("editor_title_edit")).size(24)
        } else {
            text(i18n.tr("editor_title_add")).size(24)
        };

        let name_input = text_input(i18n.tr("editor_name"), &self.name)
            .on_input(AppMessage::EditorNameChanged)
            .width(Length::Fill);

        let host_input = text_input(i18n.tr("editor_host"), &self.host)
            .on_input(AppMessage::EditorHostChanged)
            .width(Length::Fill);

        let user_input = text_input(i18n.tr("editor_user"), &self.user)
            .on_input(AppMessage::EditorUserChanged)
            .width(Length::Fill);

        let port_input = text_input(i18n.tr("editor_port"), &self.port)
            .on_input(AppMessage::EditorPortChanged)
            .width(Length::Fill);

        let password_label = if is_edit {
            text(i18n.tr("editor_password_edit_hint")).size(12)
        } else {
            text(i18n.tr("editor_password")).size(12)
        };
        let password_input = text_input(i18n.tr("editor_password"), &self.password)
            .on_input(AppMessage::EditorPasswordChanged)
            .secure(true)
            .width(Length::Fill);

        let tags_input = text_input(i18n.tr("editor_tags"), &self.tags)
            .on_input(AppMessage::EditorTagsChanged)
            .width(Length::Fill);

        let group_input = text_input(i18n.tr("editor_group"), &self.group)
            .on_input(AppMessage::EditorGroupChanged)
            .width(Length::Fill);

        let notes_input = text_input(i18n.tr("editor_notes"), &self.notes)
            .on_input(AppMessage::EditorNotesChanged)
            .width(Length::Fill);

        let can_save = !self.saving
            && !self.name.is_empty()
            && !self.host.is_empty()
            && !self.user.is_empty()
            && self.port.parse::<u16>().is_ok()
            && (is_edit || !self.password.is_empty()); // new host requires password

        let save_btn = if self.saving {
            button(i18n.tr("common_saving")).width(Length::Fill)
        } else {
            button(i18n.tr("common_save")).width(Length::Fill)
        };

        let save_btn = if can_save {
            save_btn.on_press(AppMessage::EditorSave)
        } else {
            save_btn
        };

        let cancel_btn = button(i18n.tr("common_cancel")).on_press(AppMessage::EditorCancel);

        let error_text = match &self.error {
            Some(e) => text(e).size(12),
            None => text(""),
        };

        let hint = if is_edit {
            text(i18n.tr("editor_hint_keep_password")).size(11)
        } else {
            text(i18n.tr("editor_hint_need_password")).size(11)
        };

        let content = column![
            title,
            name_input,
            host_input,
            row![user_input, port_input].spacing(12),
            password_label,
            password_input,
            hint,
            tags_input,
            group_input,
            notes_input,
            row![save_btn, cancel_btn].spacing(12),
            error_text,
        ]
        .spacing(10)
        .padding(40)
        .max_width(500);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }
}
