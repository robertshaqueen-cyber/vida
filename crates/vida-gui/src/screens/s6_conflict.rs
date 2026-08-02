use iced::widget::{button, column, container, row, text};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;

#[derive(Debug, Clone)]
pub struct State {
    pub local_hosts: Vec<String>,
    pub remote_hosts: Vec<String>,
}

impl State {
    pub fn new(local_hosts: Vec<String>, remote_hosts: Vec<String>) -> Self {
        Self { local_hosts, remote_hosts }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("conflict_title")).size(24);
        let subtitle = text(i18n.tr("conflict_subtitle")).size(14);

        // Local side
        let local_header = text(i18n.tr("conflict_local")).size(16);
        let local_items: Vec<Element<_>> = self.local_hosts
            .iter()
            .map(|h| text(h.as_str()).size(13).into())
            .collect();
        let local_list = column(local_items).spacing(4);
        let local_card = column![local_header, local_list]
            .spacing(8)
            .padding(16);

        // Remote side
        let remote_header = text(i18n.tr("conflict_remote")).size(16);
        let remote_items: Vec<Element<_>> = self.remote_hosts
            .iter()
            .map(|h| text(h.as_str()).size(13).into())
            .collect();
        let remote_list = column(remote_items).spacing(4);
        let remote_card = column![remote_header, remote_list]
            .spacing(8)
            .padding(16);

        let sides = row![local_card, remote_card].spacing(20);

        let keep_local = button(i18n.tr("conflict_keep_local"))
            .on_press(AppMessage::ConflictResolveLocal);
        let keep_remote = button(i18n.tr("conflict_keep_remote"))
            .on_press(AppMessage::ConflictResolveRemote);

        let buttons = row![keep_local, keep_remote].spacing(20);

        let content = column![title, subtitle, sides, buttons]
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
