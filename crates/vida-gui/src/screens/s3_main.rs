use iced::widget::{button, column, container, row, rule, text, tooltip};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::screens::Tab;

#[derive(Debug, Clone)]
pub struct HostItem {
    pub id: String,
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub tags: Vec<String>,
    pub group: Option<String>,
    pub color: Option<String>,
    pub auth_kind: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct State {
    pub hosts: Vec<HostItem>,
    pub search_query: String,
}

impl State {
    /// Top tab bar: tabs on the left, spacer, then settings/lock on the right
    pub fn view_tab_bar<'a>(
        tabs: &'a [Tab],
        active_tab_id: &'a str,
        i18n: &'a I18n,
        show_connect_panel: bool,
    ) -> Element<'a, AppMessage> {
        // Tab buttons: text-only, active tab has bottom indicator, with X close button
        let tab_buttons: Vec<Element<'a, AppMessage>> = tabs
            .iter()
            .map(|tab| {
                let label = text(&tab.name).size(13);
                let close_btn = tooltip(
                    button(text("×").size(14))
                        .on_press(AppMessage::CloseTab(tab.id.clone()))
                        .style(button::text),
                    i18n.tr("main_tab_close"),
                    tooltip::Position::Bottom,
                );
                let tab_content = row![label, close_btn]
                    .spacing(4)
                    .align_y(iced::Alignment::Center);
                let is_active = tab.id == active_tab_id;
                let btn = button(tab_content)
                    .on_press(AppMessage::SwitchTab(tab.id.clone()))
                    .style(button::text)
                    .width(Length::Shrink);
                if is_active {
                    container(column![btn, rule::horizontal(2),])
                        .width(Length::Shrink)
                        .into()
                } else {
                    btn.into()
                }
            })
            .collect();

        let tabs_row = row(tab_buttons).spacing(0).align_y(iced::Alignment::Center);

        let add_btn = tooltip(
            button(text("+").size(16))
                .on_press(AppMessage::OpenAddHostTab)
                .style(button::text),
            i18n.tr("main_tab_add"),
            tooltip::Position::Bottom,
        );

        // Stacked/expand button for quick connect panel
        let connect_panel_btn = tooltip(
            button(text("⊞").size(14))
                .on_press(AppMessage::ToggleConnectPanel)
                .style(if show_connect_panel {
                    button::secondary
                } else {
                    button::text
                }),
            i18n.tr("main_tab_connect"),
            tooltip::Position::Bottom,
        );

        // Left side: tabs + add button + connect panel button
        let left_side = row![tabs_row, add_btn, connect_panel_btn]
            .spacing(4)
            .align_y(iced::Alignment::Center);

        // Right side: settings gear + lock
        let settings_btn = tooltip(
            button(text("⚙").size(16))
                .on_press(AppMessage::OpenSettingsTab)
                .style(button::text),
            i18n.tr("main_tab_settings"),
            tooltip::Position::Bottom,
        );
        let lock_btn = tooltip(
            button(text("🔒").size(14))
                .on_press(AppMessage::LockVault)
                .style(button::text),
            i18n.tr("main_tab_lock"),
            tooltip::Position::Bottom,
        );

        let right_buttons = row![settings_btn, lock_btn]
            .spacing(4)
            .align_y(iced::Alignment::Center);

        // Full tab bar: left | spacer | right
        let tab_bar = row![
            left_side,
            container(text("")).width(Length::Fill),
            right_buttons,
        ]
        .spacing(0)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill);

        container(tab_bar)
            .padding(iced::padding::Padding::new(8.0).horizontal(12.0))
            .width(Length::Fill)
            .into()
    }

    pub fn view_connect_panel<'a>(
        hosts: &'a [HostItem],
        recent_ids: &'a [String],
        search: &str,
        i18n: &'a I18n,
    ) -> Element<'a, AppMessage> {
        use iced::widget::text_input;

        let search_input = text_input(i18n.tr("main_connect_search"), search)
            .on_input(AppMessage::ConnectPanelSearch)
            .id("connect_panel_search")
            .width(Length::Fill);

        let mut items: Vec<Element<'a, AppMessage>> = Vec::new();
        items.push(search_input.into());
        items.push(container(text("")).height(8).into()); // spacer

        // Recent hosts section
        let mut recent_items: Vec<Element<'a, AppMessage>> = Vec::new();
        for host_id in recent_ids.iter() {
            if let Some(host) = hosts.iter().find(|h| &h.id == host_id) {
                let label = text(format!("  {}  {}@{}", host.name, host.user, host.host)).size(13);
                let item = button(label)
                    .on_press(AppMessage::QuickConnectHost(host.id.clone()))
                    .style(button::text)
                    .width(Length::Fill);
                recent_items.push(item.into());
            }
        }
        if !recent_items.is_empty() {
            items.push(text(i18n.tr("main_connect_recent")).size(12).into());
            items.push(column(recent_items).spacing(2).into());
            items.push(rule::horizontal(1).into());
        }

        // All hosts section (filtered by search)
        let search_lower = search.to_lowercase();
        let filtered: Vec<&HostItem> = hosts
            .iter()
            .filter(|h| {
                search.is_empty()
                    || h.name.to_lowercase().contains(&search_lower)
                    || h.host.to_lowercase().contains(&search_lower)
                    || h.user.to_lowercase().contains(&search_lower)
            })
            .collect();

        if !filtered.is_empty() {
            items.push(text(i18n.tr("main_connect_all_hosts")).size(12).into());
            let host_items: Vec<Element<'a, AppMessage>> = filtered
                .iter()
                .map(|host| {
                    let label =
                        text(format!("  {}  {}@{}", host.name, host.user, host.host)).size(13);
                    button(label)
                        .on_press(AppMessage::QuickConnectHost(host.id.clone()))
                        .style(button::text)
                        .width(Length::Fill)
                        .into()
                })
                .collect();
            items.push(column(host_items).spacing(2).into());
        }

        // Add host button
        items.push(rule::horizontal(1).into());
        items.push(
            button(text(i18n.tr("main_connect_add")).size(13))
                .on_press(AppMessage::QuickAddHost)
                .style(button::text)
                .width(Length::Fill)
                .into(),
        );

        let panel = column(items).spacing(4).padding(8).width(Length::Fill);

        container(panel).padding(4).into()
    }

    pub fn view_host_detail(&self, host_id: &str, i18n: &I18n) -> Element<'_, AppMessage> {
        if let Some(host) = self.hosts.iter().find(|h| h.id == host_id) {
            let name = text(&host.name).size(24);
            let conn = text(format!("{}@{}:{}", host.user, host.host, host.port)).size(14);
            let auth = text(i18n.trf("main_auth_kind", &[&host.auth_kind])).size(14);

            let edit_btn =
                button(i18n.tr("main_edit")).on_press(AppMessage::EditHost(host.id.clone()));
            let reveal_btn = button(i18n.tr("main_reveal_credential"))
                .on_press(AppMessage::RevealCredential(host.id.clone()));
            let delete_btn = button(i18n.tr("main_delete"))
                .on_press(AppMessage::DeleteHostConfirm(host.id.clone()));

            let notes = match &host.notes {
                Some(n) if !n.is_empty() => text(n).size(13),
                _ => text("").size(13),
            };

            let buttons = row![edit_btn, reveal_btn, delete_btn].spacing(12);
            let detail = column![name, conn, auth, buttons, notes]
                .spacing(12)
                .padding(20);

            container(detail)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            text(i18n.tr("main_host_not_found")).into()
        }
    }
}
