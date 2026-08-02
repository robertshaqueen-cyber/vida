use iced::widget::{button, column, container, pick_list, row, rule, text, text_input};
use iced::{Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::screens::s3_main::HostItem;

// ---------------------------------------------------------------------------
// Settings section navigation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Application,
    Connections,
    Sync,
    Terminal,
    Backup,
}

impl SettingsSection {
    pub fn all() -> &'static [SettingsSection] {
        &[
            SettingsSection::Application,
            SettingsSection::Connections,
            SettingsSection::Sync,
            SettingsSection::Terminal,
            SettingsSection::Backup,
        ]
    }

    pub fn label(&self, i18n: &I18n) -> &'static str {
        match self {
            SettingsSection::Application => i18n.tr("settings_nav_application"),
            SettingsSection::Connections => i18n.tr("settings_nav_connections"),
            SettingsSection::Sync => i18n.tr("settings_nav_sync"),
            SettingsSection::Terminal => i18n.tr("settings_nav_terminal"),
            SettingsSection::Backup => i18n.tr("settings_nav_backup"),
        }
    }
}

// ---------------------------------------------------------------------------
// Language choice
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LangChoice {
    System,
    ZhCn,
    En,
}

impl LangChoice {
    pub fn as_str(&self) -> &'static str {
        match self {
            LangChoice::System => "system",
            LangChoice::ZhCn => "zh-CN",
            LangChoice::En => "en",
        }
    }
}

impl std::fmt::Display for LangChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            LangChoice::System => "System",
            LangChoice::ZhCn => "中文",
            LangChoice::En => "English",
        };
        write!(f, "{}", name)
    }
}

impl From<&str> for LangChoice {
    fn from(s: &str) -> Self {
        match s {
            "zh-CN" => LangChoice::ZhCn,
            "en" => LangChoice::En,
            _ => LangChoice::System,
        }
    }
}

// ---------------------------------------------------------------------------
// Settings state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct State {
    pub active_section: SettingsSection,
    // Application
    pub language: LangChoice,
    // Connections
    pub hosts: Vec<HostItem>,
    // Sync
    pub sync_local_path: String,
    // Terminal
    pub scrollback_lines: String,
    // Save state
    pub saving: bool,
    pub saved: bool,
    pub error: Option<String>,
}

impl State {
    pub fn from_json(val: &serde_json::Value, _i18n: &I18n) -> Self {
        let sync_local_path = val.get("sync_local_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let scrollback_lines = val.get("scrollback_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(5000)
            .to_string();

        let language = match vida_core::config::load_language_choice().as_deref() {
            Some("zh-CN") => LangChoice::ZhCn,
            Some("en") => LangChoice::En,
            _ => LangChoice::System,
        };

        Self {
            active_section: SettingsSection::Application,
            language,
            hosts: Vec::new(),
            sync_local_path,
            scrollback_lines,
            saving: false,
            saved: false,
            error: None,
        }
    }

    pub fn set_hosts(&mut self, hosts: Vec<HostItem>) {
        self.hosts = hosts;
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let sidebar = self.view_sidebar(i18n);
        let content = self.view_content(i18n);

        row![sidebar, content]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_sidebar(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("main_tab_settings")).size(20);

        let nav_items: Vec<Element<'_, AppMessage>> = SettingsSection::all()
            .iter()
            .map(|section| {
                let label = text(section.label(i18n)).size(14);
                let is_active = self.active_section == *section;
                let btn = button(label)
                    .on_press(AppMessage::SettingsSectionChanged(*section))
                    .width(Length::Fill);
                if is_active {
                    btn.style(button::secondary).into()
                } else {
                    btn.style(button::text).into()
                }
            })
            .collect();

        let nav_list = column(nav_items).spacing(2);

        let sidebar_content = column![title, nav_list]
            .spacing(16)
            .padding(16)
            .width(200);

        container(sidebar_content)
            .height(Length::Fill)
            .into()
    }

    fn view_content(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let content: Element<'_, AppMessage> = match self.active_section {
            SettingsSection::Application => self.view_application(i18n),
            SettingsSection::Connections => self.view_connections(i18n),
            SettingsSection::Sync => self.view_sync(i18n),
            SettingsSection::Terminal => self.view_terminal(i18n),
            SettingsSection::Backup => self.view_backup(i18n),
        };

        let wrapper = column![content]
            .spacing(16)
            .padding(24)
            .width(Length::Fill);

        container(wrapper)
            .height(Length::Fill)
            .into()
    }

    fn view_application(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("settings_application")).size(20);

        let lang_label = text(i18n.tr("settings_language")).size(14);
        let lang_options: Vec<LangChoice> = vec![
            LangChoice::System,
            LangChoice::ZhCn,
            LangChoice::En,
        ];
        let lang_pick = pick_list(lang_options, Some(self.language.clone()), AppMessage::SettingsLanguageChanged)
            .width(Length::Fill);

        let can_save = !self.saving;
        let save_btn = if self.saving {
            button(i18n.tr("common_saving")).width(Length::Shrink)
        } else {
            button(i18n.tr("common_save")).width(Length::Shrink)
        };

        let save_btn = if can_save {
            save_btn.on_press(AppMessage::SettingsSave)
        } else {
            save_btn
        };

        let status_text = if self.saved {
            text(i18n.tr("common_saved")).size(12)
        } else {
            text("")
        };

        let error_text = match &self.error {
            Some(e) => text(e).size(12),
            None => text(""),
        };

        column![
            title,
            rule::horizontal(1),
            lang_label,
            lang_pick,
            save_btn,
            status_text,
            error_text,
        ]
        .spacing(12)
        .into()
    }

    fn view_connections(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("settings_connections")).size(20);

        let mut items: Vec<Element<'_, AppMessage>> = Vec::new();

        // Group hosts by group
        let mut grouped: std::collections::HashMap<String, Vec<&HostItem>> = std::collections::HashMap::new();
        let mut ungrouped: Vec<&HostItem> = Vec::new();

        for host in &self.hosts {
            match &host.group {
                Some(g) if !g.is_empty() => {
                    grouped.entry(g.clone()).or_default().push(host);
                }
                _ => ungrouped.push(host),
            }
        }

        // Ungrouped hosts
        if !ungrouped.is_empty() {
            items.push(text(i18n.tr("settings_connections_ungrouped")).size(13).into());
            for host in &ungrouped {
                let label = text(format!("  {}  {}@{}", host.name, host.user, host.host)).size(14);
                items.push(
                    button(label).style(button::text).width(Length::Fill).into()
                );
            }
        }

        // Grouped hosts
        let mut group_names: Vec<String> = grouped.keys().cloned().collect();
        group_names.sort();
        for group_name in &group_names {
            if let Some(hosts) = grouped.get(group_name) {
                items.push(rule::horizontal(1).into());
                items.push(text(group_name.clone()).size(13).into());
                for host in hosts {
                    let label = text(format!("  {}  {}@{}", host.name, host.user, host.host)).size(14);
                    items.push(
                        button(label).style(button::text).width(Length::Fill).into()
                    );
                }
            }
        }

        if items.is_empty() {
            items.push(text(i18n.tr("settings_connections_empty")).size(14).into());
        }

        let add_btn = button(i18n.tr("main_connect_add"))
            .on_press(AppMessage::OpenAddHostTab)
            .width(Length::Shrink);

        let items_col = iced::widget::Column::from_vec(items).spacing(4);

        column![
            title,
            rule::horizontal(1),
            add_btn,
            items_col,
        ]
        .spacing(12)
        .into()
    }

    fn view_sync(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("settings_sync")).size(20);

        let sync_label = text(i18n.tr("settings_sync_path")).size(14);
        let sync_input = text_input(i18n.tr("settings_sync_path_hint"), &self.sync_local_path)
            .on_input(AppMessage::SettingsSyncPathChanged)
            .width(Length::Fill);
        let sync_hint = text(i18n.tr("settings_sync_path_hint")).size(11);

        let can_save = !self.saving;
        let save_btn = if self.saving {
            button(i18n.tr("common_saving")).width(Length::Shrink)
        } else {
            button(i18n.tr("common_save")).width(Length::Shrink)
        };

        let save_btn = if can_save {
            save_btn.on_press(AppMessage::SettingsSave)
        } else {
            save_btn
        };

        let status_text = if self.saved {
            text(i18n.tr("common_saved")).size(12)
        } else {
            text("")
        };

        let error_text = match &self.error {
            Some(e) => text(e).size(12),
            None => text(""),
        };

        column![
            title,
            rule::horizontal(1),
            sync_label,
            sync_input,
            sync_hint,
            save_btn,
            status_text,
            error_text,
        ]
        .spacing(12)
        .into()
    }

    fn view_terminal(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("settings_terminal")).size(20);

        let scroll_label = text(i18n.tr("settings_scrollback")).size(14);
        let scroll_input = text_input("5000", &self.scrollback_lines)
            .on_input(AppMessage::SettingsScrollbackChanged)
            .width(Length::Fill);

        let can_save = !self.saving;
        let save_btn = if self.saving {
            button(i18n.tr("common_saving")).width(Length::Shrink)
        } else {
            button(i18n.tr("common_save")).width(Length::Shrink)
        };

        let save_btn = if can_save {
            save_btn.on_press(AppMessage::SettingsSave)
        } else {
            save_btn
        };

        let status_text = if self.saved {
            text(i18n.tr("common_saved")).size(12)
        } else {
            text("")
        };

        let error_text = match &self.error {
            Some(e) => text(e).size(12),
            None => text(""),
        };

        column![
            title,
            rule::horizontal(1),
            scroll_label,
            scroll_input,
            save_btn,
            status_text,
            error_text,
        ]
        .spacing(12)
        .into()
    }

    fn view_backup(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("backup_title")).size(20);

        let export_btn = button(i18n.tr("backup_title"))
            .on_press(AppMessage::OpenBackup)
            .width(Length::Shrink);

        column![
            title,
            rule::horizontal(1),
            export_btn,
        ]
        .spacing(12)
        .into()
    }
}
