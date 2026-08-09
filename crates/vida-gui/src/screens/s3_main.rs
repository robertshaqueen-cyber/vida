use iced::widget::{Space, button, column, container, row, rule, scrollable, text, tooltip};
use iced::{Alignment, Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::screens::{Tab, TabKind};
use crate::ui::{self, icons};

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
    pub agent_trust: String,
}

#[derive(Debug, Clone)]
pub struct State {
    /// Currently revealed credential `(host_id, plaintext)` for a host,
    /// auto-hides after 15s. Only shown when the host_id matches the
    /// currently viewed host.
    pub revealed_credential: Option<(String, String)>,
    /// True right after copy, shows "copied, auto-clears in 45s".
    pub credential_copied: bool,
}

impl State {
    /// Top tab bar: tabs on the left, spacer, then sync/settings/lock on the right
    pub fn view_tab_bar<'a>(
        tabs: &'a [Tab],
        active_tab_id: &'a str,
        i18n: &'a I18n,
        show_connect_panel: bool,
        agent_approval_count: usize,
        sync_symbol: &'static str,
        sync_label: String,
    ) -> Element<'a, AppMessage> {
        // Compact chips follow the Oryxis-style 40 px chrome rhythm while
        // keeping Vida's existing independent close action.
        let tab_buttons: Vec<Element<'a, AppMessage>> = tabs
            .iter()
            .map(|tab| {
                let glyph = match tab.kind {
                    TabKind::Host { .. } => icons::SERVER,
                    TabKind::Terminal { .. } => icons::TERMINAL,
                    TabKind::AddHost => icons::CIRCLE_PLUS,
                    TabKind::EditHost { .. } => icons::PENCIL,
                    TabKind::Settings => icons::SETTINGS,
                };
                let is_active = tab.id == active_tab_id;
                let label = row![
                    icons::icon(glyph, 14).color(if is_active {
                        ui::ACCENT
                    } else {
                        ui::TEXT_MUTED
                    }),
                    text(&tab.name).size(13),
                ]
                .spacing(7)
                .align_y(Alignment::Center);
                let switch_btn = button(label)
                    .on_press(AppMessage::SwitchTab(tab.id.clone()))
                    .style(ui::tab(is_active))
                    .padding([7, 9]);
                let close_btn = tooltip(
                    button(icons::icon(icons::X, 12))
                        .on_press(AppMessage::CloseTab(tab.id.clone()))
                        .style(ui::icon_button(false))
                        .padding(7),
                    i18n.tr("main_tab_close"),
                    tooltip::Position::Bottom,
                );
                container(
                    row![switch_btn, close_btn]
                        .spacing(0)
                        .align_y(Alignment::Center),
                )
                .style(ui::tab_surface(is_active))
                .height(36)
                .into()
            })
            .collect();

        let home_badge = container(icons::icon(icons::HOUSE, 16).color(ui::ACCENT))
            .center_x(34)
            .center_y(34)
            .style(ui::accent_badge);

        let tabs_row = row(tab_buttons).spacing(2).align_y(Alignment::Center);
        let tabs_scroll = scrollable(tabs_row)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::hidden(),
            ))
            .width(Length::Fill)
            .height(36);

        let add_btn = tooltip(
            button(icons::icon(icons::PLUS, 16))
                .on_press(AppMessage::OpenAddHostTab)
                .style(ui::icon_button(false))
                .padding(9),
            i18n.tr("main_tab_add"),
            tooltip::Position::Bottom,
        );

        // Stacked/expand button for quick connect panel
        let connect_panel_btn = tooltip(
            button(icons::icon(icons::PANEL_LEFT, 16))
                .on_press(AppMessage::ToggleConnectPanel)
                .style(ui::icon_button(show_connect_panel))
                .padding(9),
            i18n.tr("main_tab_connect"),
            tooltip::Position::Bottom,
        );

        let local_term_btn = tooltip(
            button(icons::icon(icons::TERMINAL, 16))
                .on_press(AppMessage::OpenLocalTerminal)
                .style(ui::icon_button(false))
                .padding(9),
            i18n.tr("terminal_open_local"),
            tooltip::Position::Bottom,
        );

        // Right side: settings gear + lock
        let settings_btn = tooltip(
            button(icons::icon(icons::SETTINGS, 16))
                .on_press(AppMessage::OpenSettingsTab)
                .style(ui::icon_button(false))
                .padding(9),
            i18n.tr("main_tab_settings"),
            tooltip::Position::Bottom,
        );
        let lock_btn = tooltip(
            button(icons::icon(icons::LOCK, 16))
                .on_press(AppMessage::LockVault)
                .style(ui::icon_button(false))
                .padding(9),
            i18n.tr("main_tab_lock"),
            tooltip::Position::Bottom,
        );
        // Sync indicator: always sends SyncTriggered on click.
        // Daemon returns real state (including sync_not_configured).
        let sync_glyph = match sync_symbol {
            "✓" => icons::CIRCLE_CHECK,
            "▲" | "✗" => icons::CIRCLE_ALERT,
            _ => icons::REFRESH,
        };
        let sync_active = sync_symbol == "⟳" || sync_symbol == "▲";
        let sync_btn = tooltip(
            button(icons::icon(sync_glyph, 16))
                .on_press(AppMessage::SyncTriggered)
                .style(ui::icon_button(sync_active))
                .padding(9),
            text(sync_label),
            tooltip::Position::Bottom,
        );

        let approval_btn: Element<'a, AppMessage> = if agent_approval_count > 0 {
            tooltip(
                button(
                    row![
                        icons::icon(icons::CIRCLE_ALERT, 16).color(ui::WARNING),
                        text(agent_approval_count).size(12).color(ui::WARNING),
                    ]
                    .spacing(5)
                    .align_y(Alignment::Center),
                )
                .on_press(AppMessage::ToggleAgentApprovalPanel)
                .style(ui::icon_button(true))
                .padding([9, 10]),
                i18n.tr("agent_approval_open"),
                tooltip::Position::Bottom,
            )
            .into()
        } else {
            Space::new().width(0).into()
        };

        let right_buttons = row![approval_btn, sync_btn, settings_btn, lock_btn]
            .spacing(2)
            .align_y(Alignment::Center);

        // Full tab bar: left | spacer | right
        let tab_bar = row![
            home_badge,
            tabs_scroll,
            add_btn,
            connect_panel_btn,
            local_term_btn,
            right_buttons,
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill);

        column![
            container(tab_bar)
                .padding(iced::padding::Padding::new(5.0).horizontal(8.0))
                .height(46)
                .width(Length::Fill)
                .style(ui::chrome),
            container(Space::new())
                .height(1)
                .width(Length::Fill)
                .style(|_| { container::Style::default().background(ui::ACCENT) }),
        ]
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
            .style(ui::input)
            .padding(11)
            .width(Length::Fill);

        let mut items: Vec<Element<'a, AppMessage>> = Vec::new();
        items.push(search_input.into());
        items.push(container(text("")).height(8).into()); // spacer

        // Recent hosts section
        let mut recent_items: Vec<Element<'a, AppMessage>> = Vec::new();
        for host_id in recent_ids.iter() {
            if let Some(host) = hosts.iter().find(|h| &h.id == host_id) {
                let label = row![
                    icons::icon(icons::HISTORY, 14).color(ui::TEXT_MUTED),
                    column![
                        text(&host.name).size(13),
                        ui::muted(format!("{}@{}", host.user, host.host)).size(11),
                    ]
                    .spacing(2)
                ]
                .spacing(9)
                .align_y(Alignment::Center);
                let item = button(label)
                    .on_press(AppMessage::QuickConnectHost(host.id.clone()))
                    .style(ui::nav_item(false))
                    .padding(8)
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
                    let label = row![
                        icons::icon(icons::SERVER, 14).color(ui::TEXT_MUTED),
                        column![
                            text(&host.name).size(13),
                            ui::muted(format!("{}@{}", host.user, host.host)).size(11),
                        ]
                        .spacing(2)
                    ]
                    .spacing(9)
                    .align_y(Alignment::Center);
                    button(label)
                        .on_press(AppMessage::QuickConnectHost(host.id.clone()))
                        .style(ui::nav_item(false))
                        .padding(8)
                        .width(Length::Fill)
                        .into()
                })
                .collect();
            items.push(column(host_items).spacing(2).into());
        }

        // Add host button
        items.push(rule::horizontal(1).into());
        items.push(
            button(
                row![
                    icons::icon(icons::PLUS, 14),
                    text(i18n.tr("main_connect_add")).size(13),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .on_press(AppMessage::QuickAddHost)
            .style(ui::primary_button)
            .padding([9, 12])
            .width(Length::Fill)
            .into(),
        );

        column(items)
            .spacing(6)
            .padding(10)
            .width(Length::Fill)
            .into()
    }

    pub fn view_host_detail<'a>(
        &'a self,
        hosts: &'a [HostItem],
        host_id: &'a str,
        i18n: &'a I18n,
    ) -> Element<'a, AppMessage> {
        if let Some(host) = hosts.iter().find(|h| h.id == host_id) {
            let identity = row![
                container(icons::icon(icons::SERVER, 22).color(ui::ACCENT))
                    .center_x(48)
                    .center_y(48)
                    .style(ui::accent_badge),
                column![
                    text(&host.name).size(24),
                    ui::muted(format!("{}@{}:{}", host.user, host.host, host.port)).size(13),
                ]
                .spacing(4),
            ]
            .spacing(14)
            .align_y(Alignment::Center);

            let edit_btn = button(
                row![
                    icons::icon(icons::PENCIL, 14),
                    text(i18n.tr("main_edit")).size(13),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .on_press(AppMessage::EditHost(host.id.clone()))
            .style(ui::primary_button)
            .padding([9, 12]);
            let connect_btn = button(
                row![
                    icons::icon(icons::TERMINAL, 14),
                    text(i18n.tr("main_connect")).size(13),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .on_press(AppMessage::OpenSshTerminal(host.id.clone()))
            .style(ui::primary_button)
            .padding([9, 12]);
            let reveal_btn = button(
                row![
                    icons::icon(icons::EYE, 14),
                    text(i18n.tr("main_reveal_credential")).size(13),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .on_press(AppMessage::RevealCredential(host.id.clone()))
            .style(ui::secondary_button)
            .padding([9, 12]);
            let delete_btn = button(
                row![
                    icons::icon(icons::TRASH, 14),
                    text(i18n.tr("main_delete")).size(13),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .on_press(AppMessage::DeleteHostConfirm(host.id.clone()))
            .style(ui::danger_button)
            .padding([9, 12]);

            let header = container(
                row![
                    identity,
                    Space::new().width(Length::Fill),
                    row![connect_btn, edit_btn, reveal_btn, delete_btn].spacing(8),
                ]
                .align_y(Alignment::Center),
            )
            .padding(18)
            .width(Length::Fill)
            .style(ui::surface);

            let group = host
                .group
                .as_deref()
                .filter(|v| !v.is_empty())
                .unwrap_or("—");
            let tags = if host.tags.is_empty() {
                "—".to_string()
            } else {
                host.tags.join(" · ")
            };
            let info = row![
                info_card(icons::KEY, i18n.trf("main_auth_kind", &[&host.auth_kind])),
                info_card(icons::FOLDER, group.to_string()),
                info_card(icons::DATABASE, tags),
                info_card(
                    icons::SHIELD,
                    i18n.tr(match host.agent_trust.as_str() {
                        "readonly" => "agent_trust_readonly",
                        "trusted" => "agent_trust_trusted",
                        _ => "agent_trust_ask",
                    })
                    .to_string(),
                ),
            ]
            .spacing(12)
            .width(Length::Fill);

            let mut detail_items: Vec<Element<'_, AppMessage>> = vec![header.into(), info.into()];

            // Revealed credential block: plaintext + copy button, auto-hides in 15s.
            // Only shown when the revealed credential belongs to THIS host, so
            // switching tabs never leaks another host's password into this view.
            let revealed_for_this_host = self
                .revealed_credential
                .as_ref()
                .filter(|(revealed_host_id, _)| revealed_host_id == host_id)
                .map(|(_, cred)| cred);
            if let Some(cred) = revealed_for_this_host {
                let cred_label = ui::muted(i18n.tr("main_credential_revealed")).size(12);
                let cred_value = text(cred.as_str()).size(14);
                let copy_btn = button(
                    row![
                        icons::icon(icons::COPY, 13),
                        text(i18n.tr("main_credential_copy")).size(12),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .on_press(AppMessage::CopyCredential(cred.clone()))
                .style(ui::secondary_button)
                .padding([7, 10])
                .width(Length::Shrink);
                let mut cred_row = row![cred_value, copy_btn]
                    .spacing(12)
                    .align_y(iced::Alignment::Center);
                if self.credential_copied {
                    cred_row = cred_row.push(text(i18n.tr("main_credential_copied")).size(12));
                }
                let credential_card = container(
                    column![
                        cred_label,
                        cred_row,
                        ui::muted(i18n.tr("main_credential_auto_hide")).size(11),
                    ]
                    .spacing(8),
                )
                .padding(16)
                .width(Length::Fill)
                .style(ui::surface);
                detail_items.push(credential_card.into());
            }

            if let Some(notes) = host.notes.as_deref().filter(|value| !value.is_empty()) {
                detail_items.push(
                    container(
                        column![
                            ui::muted(i18n.tr("editor_notes")).size(12),
                            text(notes).size(13),
                        ]
                        .spacing(8),
                    )
                    .padding(16)
                    .width(Length::Fill)
                    .style(ui::surface)
                    .into(),
                );
            }

            let detail = column(detail_items).spacing(14).padding(24).max_width(1060);

            container(detail)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .style(ui::app_background)
                .into()
        } else {
            text(i18n.tr("main_host_not_found")).into()
        }
    }
}

fn info_card<'a>(glyph: &'static str, value: String) -> Element<'a, AppMessage> {
    container(
        row![
            icons::icon(glyph, 16).color(ui::ACCENT),
            text(value).size(13),
        ]
        .spacing(9)
        .align_y(Alignment::Center),
    )
    .padding(14)
    .width(Length::FillPortion(1))
    .style(ui::surface)
    .into()
}
