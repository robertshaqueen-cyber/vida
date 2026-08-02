use iced::widget::{button, column, container, text};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;

#[derive(Debug, Clone)]
pub struct ConflictFileInfo {
    pub path: String,
    pub pattern: String,
}

#[derive(Debug, Clone)]
pub struct State {
    pub files: Vec<ConflictFileInfo>,
    pub remote_hosts: Vec<String>,
}

impl State {
    #[allow(dead_code)] // revived with sync trigger entry
    pub fn new(files: Vec<ConflictFileInfo>, remote_hosts: Vec<String>) -> Self {
        Self { files, remote_hosts }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("conflict_file_title")).size(24);
        let subtitle = text(i18n.trf("conflict_file_count", &[&self.files.len().to_string()])).size(14);

        let file_items: Vec<Element<_>> = self.files
            .iter()
            .map(|f| {
                let name = text(&f.path).size(13);
                let pattern = text(i18n.trf("conflict_file_source", &[&f.pattern])).size(11);
                column![name, pattern].spacing(2).into()
            })
            .collect();

        let file_list = column(file_items).spacing(8);

        let content = if !self.remote_hosts.is_empty() {
            let remote_header = text(i18n.tr("conflict_file_hosts")).size(14);
            let remote_items: Vec<Element<_>> = self.remote_hosts
                .iter()
                .map(|h| text(h.as_str()).size(13).into())
                .collect();
            let remote_list = column(remote_items).spacing(4);

            let adopt_btn = button(i18n.tr("conflict_file_adopt"))
                .on_press(AppMessage::ConflictFileAdopt);
            let ignore_btn = button(i18n.tr("conflict_file_ignore"))
                .on_press(AppMessage::ConflictFileIgnore);

            column![title, subtitle, file_list, remote_header, remote_list, adopt_btn, ignore_btn]
                .spacing(12)
                .padding(40)
        } else {
            let adopt_btn = button(i18n.tr("conflict_file_adopt_short"))
                .on_press(AppMessage::ConflictFileAdopt);
            let ignore_btn = button(i18n.tr("conflict_file_ignore_short"))
                .on_press(AppMessage::ConflictFileIgnore);

            column![title, subtitle, file_list, adopt_btn, ignore_btn]
                .spacing(12)
                .padding(40)
        };

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }
}
