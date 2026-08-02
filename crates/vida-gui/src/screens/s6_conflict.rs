use iced::widget::{button, column, container, row, text};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;

#[derive(Debug, Clone)]
pub struct State {
    pub local_hosts: Vec<String>,
    pub remote_hosts: Vec<String>,
    /// Present only on the local side.
    pub only_local: Vec<String>,
    /// Present only on the remote side.
    pub only_remote: Vec<String>,
    /// Present on both sides.
    pub both: Vec<String>,
}

impl State {
    pub fn new(local_hosts: Vec<String>, remote_hosts: Vec<String>) -> Self {
        let only_local: Vec<String> = local_hosts
            .iter()
            .filter(|n| !remote_hosts.contains(n))
            .cloned()
            .collect();
        let only_remote: Vec<String> = remote_hosts
            .iter()
            .filter(|n| !local_hosts.contains(n))
            .cloned()
            .collect();
        let both: Vec<String> = local_hosts
            .iter()
            .filter(|n| remote_hosts.contains(n))
            .cloned()
            .collect();
        Self {
            local_hosts,
            remote_hosts,
            only_local,
            only_remote,
            both,
        }
    }

    fn list_block<'a>(title: &'a str, items: &'a [String]) -> Element<'a, AppMessage> {
        let header = text(title).size(13);
        let entries: Vec<Element<_>> = items
            .iter()
            .map(|h| text(h.as_str()).size(13).into())
            .collect();
        let list = if entries.is_empty() {
            column![text("—").size(13)].spacing(2)
        } else {
            column(entries).spacing(2)
        };
        column![header, list].spacing(6).into()
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("conflict_title")).size(24);
        let subtitle = text(i18n.tr("conflict_subtitle")).size(14);

        // Side summaries with counts
        let local_header = text(i18n.trf(
            "conflict_side_count",
            &[&self.local_hosts.len().to_string()],
        ))
        .size(16);
        let local_items: Vec<Element<_>> = self
            .local_hosts
            .iter()
            .map(|h| text(h.as_str()).size(13).into())
            .collect();
        let local_list = column(local_items).spacing(4);
        let local_card = column![local_header, local_list].spacing(8).padding(16);

        let remote_header = text(i18n.trf(
            "conflict_side_count",
            &[&self.remote_hosts.len().to_string()],
        ))
        .size(16);
        let remote_items: Vec<Element<_>> = self
            .remote_hosts
            .iter()
            .map(|h| text(h.as_str()).size(13).into())
            .collect();
        let remote_list = column(remote_items).spacing(4);
        let remote_card = column![remote_header, remote_list].spacing(8).padding(16);

        let sides = row![local_card, remote_card].spacing(20);

        // Difference groups: only local / only remote / on both sides
        let diff_title = text(i18n.tr("conflict_diff_title")).size(16);
        let diff_groups = row![
            Self::list_block(i18n.tr("conflict_diff_only_local"), &self.only_local),
            Self::list_block(i18n.tr("conflict_diff_only_remote"), &self.only_remote),
            Self::list_block(i18n.tr("conflict_diff_both"), &self.both),
        ]
        .spacing(24)
        .align_y(iced::Alignment::Start);
        let diff_section = column![diff_title, diff_groups].spacing(8);

        let keep_local =
            button(i18n.tr("conflict_keep_local")).on_press(AppMessage::ConflictResolveLocal);
        let keep_remote =
            button(i18n.tr("conflict_keep_remote")).on_press(AppMessage::ConflictResolveRemote);

        let buttons = row![keep_local, keep_remote].spacing(20);

        let content = column![title, subtitle, sides, diff_section, buttons]
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
