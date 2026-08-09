use iced::widget::{
    Space, button, column, container, pick_list, row, rule, scrollable, text, text_input,
};
use iced::{Alignment, Element, Length};
use vida_core::i18n::I18n;
use vida_core::vault::Settings;

use crate::app::AppMessage;
use crate::screens::s3_main::HostItem;
use crate::screens::s9_backup;
use crate::term::primitive::{
    DEFAULT_TERMINAL_FONT_FAMILY, TerminalAppearance, load_bundled_terminal_fonts,
};
use crate::ui::{self, icons};

const TERMINAL_FONT_SIZES: &[u16] = &[10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32];

/// 展示内置默认字体和系统安装的等宽字体。比例字体会破坏
/// 终端固定 cell 布局，因此即使已安装也不进入这个选择器。
fn installed_terminal_fonts() -> Vec<String> {
    let mut database = fontdb::Database::new();
    database.load_system_fonts();
    load_bundled_terminal_fonts(&mut database);

    let mut families: Vec<String> = database
        .faces()
        .filter(|face| face.monospaced && !face.post_script_name.contains("Bitmap"))
        .filter_map(|face| face.families.first().map(|(name, _)| name.clone()))
        .collect();
    families.sort_by_key(|name| name.to_lowercase());
    families.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    families
}

fn selected_terminal_font(configured: &str, installed: &[String]) -> String {
    installed
        .iter()
        .find(|name| name.eq_ignore_ascii_case(configured))
        .or_else(|| {
            installed
                .iter()
                .find(|name| name.as_str() == DEFAULT_TERMINAL_FONT_FAMILY)
        })
        .or_else(|| installed.iter().find(|name| name.as_str() == "Menlo"))
        .or_else(|| installed.first())
        .cloned()
        .unwrap_or_else(|| DEFAULT_TERMINAL_FONT_FAMILY.to_string())
}

fn selected_terminal_font_size(configured: f32) -> u16 {
    TERMINAL_FONT_SIZES
        .iter()
        .copied()
        .min_by(|a, b| {
            (*a as f32 - configured)
                .abs()
                .total_cmp(&(*b as f32 - configured).abs())
        })
        .unwrap_or(13)
}

// ---------------------------------------------------------------------------
// Sync mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    None,
    Local,
}

impl SyncMode {
    pub fn from_path(path: &str) -> Self {
        if path.is_empty() {
            SyncMode::None
        } else {
            SyncMode::Local
        }
    }
}

/// Wrapper that carries the i18n-translated label for pick_list display.
#[derive(Debug, Clone)]
pub struct SyncModeItem {
    pub mode: SyncMode,
    label: String,
}

impl SyncModeItem {
    pub fn none(i18n: &I18n) -> Self {
        Self {
            mode: SyncMode::None,
            label: i18n.tr("settings_sync_mode_none").to_string(),
        }
    }

    pub fn local(i18n: &I18n) -> Self {
        Self {
            mode: SyncMode::Local,
            label: i18n.tr("settings_sync_mode_local").to_string(),
        }
    }
}

impl PartialEq for SyncModeItem {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
    }
}

impl std::fmt::Display for SyncModeItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

// ---------------------------------------------------------------------------
// Quick location shortcuts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickLocation {
    ICloud,
    Home,
}

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
    // Sync
    pub sync_mode: SyncMode,
    pub sync_local_path: String,
    // Terminal
    pub scrollback_lines: String,
    pub terminal_font_families: Vec<String>,
    pub terminal_font_family: String,
    pub terminal_font_size: u16,
    pub terminal_cursor_blink: bool,
    /// Backup form is UI state owned by the Settings screen.
    pub backup: s9_backup::State,
    /// 保存时以 daemon 返回的完整 Settings 为底，避免覆盖未在当前页面展示的字段。
    pub vault_settings: Settings,
    // Save state
    pub saving: bool,
    pub saved: bool,
    pub error: Option<String>,
}

impl State {
    pub fn from_json(val: &serde_json::Value, _i18n: &I18n) -> Self {
        let vault_settings: Settings =
            serde_json::from_value(val.clone()).unwrap_or_else(|_| Settings::default());
        let sync_local_path = vault_settings.sync_local_path.clone().unwrap_or_default();
        let sync_mode = SyncMode::from_path(&sync_local_path);

        let scrollback_lines = vault_settings.scrollback_lines.to_string();

        let language = match vida_core::config::load_language_choice().as_deref() {
            Some("zh-CN") => LangChoice::ZhCn,
            Some("en") => LangChoice::En,
            _ => LangChoice::System,
        };

        let terminal_font_families = installed_terminal_fonts();
        let terminal_font_family = selected_terminal_font(
            &vault_settings.terminal_font_family,
            &terminal_font_families,
        );
        let terminal_font_size = selected_terminal_font_size(vault_settings.terminal_font_size);

        Self {
            active_section: SettingsSection::Application,
            language,
            sync_mode,
            sync_local_path,
            scrollback_lines,
            terminal_font_families,
            terminal_font_family,
            terminal_font_size,
            terminal_cursor_blink: vault_settings.terminal_cursor_blink,
            backup: s9_backup::State::new(),
            vault_settings,
            saving: false,
            saved: false,
            error: None,
        }
    }

    pub fn terminal_appearance(&self) -> TerminalAppearance {
        TerminalAppearance {
            font_family: self.terminal_font_family.clone(),
            font_size: self.terminal_font_size as f32,
            cursor_blink: self.terminal_cursor_blink,
        }
    }

    /// Hosts are business data on VidaApp; the screen reads them from the
    /// caller instead of caching a snapshot (prevents stale lists).
    pub fn view<'a>(&'a self, i18n: &'a I18n, hosts: &'a [HostItem]) -> Element<'a, AppMessage> {
        let sidebar = self.view_sidebar(i18n);
        let content = self.view_content(i18n, hosts);

        row![sidebar, content]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_sidebar(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = row![
            icons::icon(icons::SETTINGS, 18).color(ui::ACCENT),
            text(i18n.tr("main_tab_settings")).size(18),
        ]
        .spacing(9)
        .align_y(Alignment::Center);

        let nav_items: Vec<Element<'_, AppMessage>> = SettingsSection::all()
            .iter()
            .map(|section| {
                let glyph = match section {
                    SettingsSection::Application => icons::LANGUAGES,
                    SettingsSection::Connections => icons::SERVER,
                    SettingsSection::Sync => icons::CLOUD,
                    SettingsSection::Terminal => icons::TERMINAL,
                    SettingsSection::Backup => icons::SHIELD,
                };
                let label = row![icons::icon(glyph, 15), text(section.label(i18n)).size(13),]
                    .spacing(10)
                    .align_y(Alignment::Center);
                let is_active = self.active_section == *section;
                button(label)
                    .on_press(AppMessage::SettingsSectionChanged(*section))
                    .style(ui::nav_item(is_active))
                    .padding([10, 12])
                    .width(Length::Fill)
                    .into()
            })
            .collect();

        let nav_list = column(nav_items).spacing(2);

        let sidebar_content = column![title, nav_list, Space::new().height(Length::Fill)]
            .spacing(18)
            .padding(14)
            .width(204);

        container(sidebar_content)
            .height(Length::Fill)
            .style(ui::sidebar)
            .into()
    }

    fn view_content<'a>(
        &'a self,
        i18n: &'a I18n,
        hosts: &'a [HostItem],
    ) -> Element<'a, AppMessage> {
        let content: Element<'_, AppMessage> = match self.active_section {
            SettingsSection::Application => self.view_application(i18n),
            SettingsSection::Connections => self.view_connections(i18n, hosts),
            SettingsSection::Sync => self.view_sync(i18n),
            SettingsSection::Terminal => self.view_terminal(i18n),
            SettingsSection::Backup => self.view_backup(i18n),
        };

        let wrapper = scrollable(
            container(content)
                .padding(24)
                .width(Length::Fill)
                .max_width(980)
                .center_x(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill);

        container(wrapper)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(ui::app_background)
            .into()
    }

    fn view_application(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("settings_application")).size(20);

        let lang_label = text(i18n.tr("settings_language")).size(13);
        let lang_options: Vec<LangChoice> =
            vec![LangChoice::System, LangChoice::ZhCn, LangChoice::En];
        let lang_pick = pick_list(
            lang_options,
            Some(self.language.clone()),
            AppMessage::SettingsLanguageChanged,
        )
        .style(ui::picker)
        .padding(10)
        .width(Length::Fill);

        let can_save = !self.saving;
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

        settings_section(
            title,
            column![
                lang_label,
                lang_pick,
                row![save_btn, status_text].spacing(10),
                error_text
            ]
            .spacing(10)
            .into(),
        )
    }

    fn view_connections<'a>(
        &'a self,
        i18n: &'a I18n,
        hosts: &'a [HostItem],
    ) -> Element<'a, AppMessage> {
        let title = text(i18n.tr("settings_connections")).size(20);

        let mut items: Vec<Element<'_, AppMessage>> = Vec::new();

        // Group hosts by group
        let mut grouped: std::collections::HashMap<String, Vec<&HostItem>> =
            std::collections::HashMap::new();
        let mut ungrouped: Vec<&HostItem> = Vec::new();

        for host in hosts {
            match &host.group {
                Some(g) if !g.is_empty() => {
                    grouped.entry(g.clone()).or_default().push(host);
                }
                _ => ungrouped.push(host),
            }
        }

        // Ungrouped hosts
        if !ungrouped.is_empty() {
            items.push(
                text(i18n.tr("settings_connections_ungrouped"))
                    .size(13)
                    .into(),
            );
            for host in &ungrouped {
                items.push(connection_host_item(host, i18n));
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
                    items.push(connection_host_item(host, i18n));
                }
            }
        }

        if items.is_empty() {
            items.push(text(i18n.tr("settings_connections_empty")).size(14).into());
        }

        let add_btn = button(
            row![
                icons::icon(icons::PLUS, 14),
                text(i18n.tr("main_connect_add")).size(13),
            ]
            .spacing(7)
            .align_y(Alignment::Center),
        )
        .on_press(AppMessage::OpenAddHostTab)
        .style(ui::primary_button)
        .padding([9, 12])
        .width(Length::Shrink);

        let items_col = iced::widget::Column::from_vec(items)
            .spacing(4)
            .width(Length::Fill);

        settings_section(
            title,
            column![add_btn, items_col]
                .spacing(12)
                .width(Length::Fill)
                .into(),
        )
    }

    fn view_sync(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("settings_sync")).size(20);

        // Explanation
        let explain = text(i18n.tr("settings_sync_explain")).size(12);

        // Sync mode dropdown (with i18n labels via SyncModeItem wrapper)
        let mode_label = text(i18n.tr("settings_sync_mode")).size(14);
        let mode_options: Vec<SyncModeItem> =
            vec![SyncModeItem::none(i18n), SyncModeItem::local(i18n)];
        let selected_item = match self.sync_mode {
            SyncMode::None => Some(SyncModeItem::none(i18n)),
            SyncMode::Local => Some(SyncModeItem::local(i18n)),
        };
        let mode_pick = pick_list(
            mode_options,
            selected_item,
            AppMessage::SettingsSyncModeChanged,
        )
        .style(ui::picker)
        .padding(10)
        .width(Length::Fill);

        // Path input + folder button (only when Local mode)
        let path_section: Element<'_, AppMessage> = match self.sync_mode {
            SyncMode::Local => {
                let path_label = text(i18n.tr("settings_sync_path")).size(14);
                let path_input =
                    text_input(i18n.tr("settings_sync_path_hint"), &self.sync_local_path)
                        .on_input(AppMessage::SettingsSyncPathChanged)
                        .style(ui::input)
                        .padding(10)
                        .width(Length::Fill);
                // Keep this constructor identical to the Backup/Restore file
                // chooser so both settings rows have the same label metrics,
                // padding, and secondary-button treatment.
                let pick_btn = button(i18n.tr("settings_sync_pick_folder"))
                    .on_press(AppMessage::SettingsSyncPickFolder)
                    .style(ui::secondary_button)
                    .padding([9, 12])
                    .width(Length::Shrink);
                let path_row = row![path_input, pick_btn]
                    .spacing(10)
                    .align_y(Alignment::Center)
                    .width(Length::Fill);

                // Quick location shortcuts
                let icloud_label = text("iCloud Drive").size(12);
                let icloud_btn = button(icloud_label)
                    .on_press(AppMessage::SettingsSyncQuickLocation(QuickLocation::ICloud))
                    .style(ui::nav_item(false))
                    .padding([2, 6]);
                let home_label = text("~").size(12);
                let home_btn = button(home_label)
                    .on_press(AppMessage::SettingsSyncQuickLocation(QuickLocation::Home))
                    .style(ui::nav_item(false))
                    .padding([2, 6]);
                let shortcuts_hint = text(i18n.tr("settings_sync_quick_locations")).size(11);
                let shortcuts_row = row![shortcuts_hint, icloud_btn, home_btn]
                    .spacing(6)
                    .align_y(iced::Alignment::Center);

                column![path_label, path_row, shortcuts_row]
                    .spacing(4)
                    .into()
            }
            SyncMode::None => text("").into(),
        };

        // Save button
        let can_save = !self.saving;
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

        settings_section(
            title,
            column![
                explain,
                mode_label,
                mode_pick,
                path_section,
                row![save_btn, status_text].spacing(10),
                error_text,
            ]
            .spacing(12)
            .into(),
        )
    }

    fn view_terminal(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("settings_terminal")).size(20);
        let renderer_hint = ui::muted(i18n.tr("settings_terminal_renderer_hint")).size(12);

        let scroll_label = text(i18n.tr("settings_scrollback")).size(14);
        let scroll_input = text_input("3000", &self.scrollback_lines)
            .on_input(AppMessage::SettingsScrollbackChanged)
            .style(ui::input)
            .padding(10)
            .width(Length::Fill);
        let scroll_hint = text(i18n.tr("settings_scrollback_hint")).size(11);

        let font_family_label = text(i18n.tr("settings_terminal_font_family")).size(14);
        let font_family_input = pick_list(
            self.terminal_font_families.as_slice(),
            Some(&self.terminal_font_family),
            AppMessage::SettingsTerminalFontFamilyChanged,
        )
        .style(ui::picker)
        .padding(10)
        .width(Length::Fill);
        let font_family_hint = text(i18n.tr("settings_terminal_font_family_hint")).size(11);

        let font_size_label = text(i18n.tr("settings_terminal_font_size")).size(14);
        let font_size_input = pick_list(
            TERMINAL_FONT_SIZES,
            Some(self.terminal_font_size),
            AppMessage::SettingsTerminalFontSizeChanged,
        )
        .style(ui::picker)
        .padding(10)
        .width(Length::Fill);
        let font_size_hint = text(i18n.tr("settings_terminal_font_size_hint")).size(11);

        let cursor_blink = iced::widget::checkbox(self.terminal_cursor_blink)
            .label(i18n.tr("settings_terminal_cursor_blink"))
            .on_toggle(AppMessage::SettingsTerminalCursorBlinkChanged);

        let can_save = !self.saving;
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

        settings_section(
            title,
            column![
                renderer_hint,
                scroll_label,
                scroll_input,
                scroll_hint,
                rule::horizontal(1),
                font_family_label,
                font_family_input,
                font_family_hint,
                font_size_label,
                font_size_input,
                font_size_hint,
                cursor_blink,
                row![save_btn, status_text].spacing(10),
                error_text,
            ]
            .spacing(11)
            .into(),
        )
    }

    fn view_backup(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text(i18n.tr("backup_page_title")).size(20);
        settings_section(title, self.backup.view_settings_content(i18n))
    }
}

fn connection_host_item<'a>(host: &'a HostItem, i18n: &'a I18n) -> Element<'a, AppMessage> {
    let identity = row![
        container(icons::icon(icons::SERVER, 15).color(ui::ACCENT))
            .center_x(34)
            .center_y(34)
            .style(ui::accent_badge),
        column![
            text(host.name.clone()).size(14),
            ui::muted(format!("{}@{}:{}", host.user, host.host, host.port)).size(12),
        ]
        .spacing(2)
        .width(Length::Fill),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .width(Length::Fill);
    let connect = button(
        row![
            icons::icon(icons::TERMINAL, 13),
            text(i18n.tr("main_connect")).size(12),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .on_press(AppMessage::OpenSshTerminal(host.id.clone()))
    .style(ui::primary_button)
    .padding([7, 10]);
    let details = button(
        row![
            icons::icon(icons::EYE, 13),
            text(i18n.tr("main_details")).size(12),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .on_press(AppMessage::OpenHostDetail(host.id.clone()))
    .style(ui::secondary_button)
    .padding([7, 10]);
    let edit = button(
        row![
            icons::icon(icons::PENCIL, 13),
            text(i18n.tr("main_edit")).size(12),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .on_press(AppMessage::EditHost(host.id.clone()))
    .style(ui::secondary_button)
    .padding([7, 10]);

    container(
        row![identity, connect, edit, details]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .padding([8, 10])
    .width(Length::Fill)
    .style(ui::surface)
    .into()
}

fn settings_section<'a>(
    title: iced::widget::Text<'a>,
    content: Element<'a, AppMessage>,
) -> Element<'a, AppMessage> {
    column![
        title,
        container(content)
            .padding(18)
            .width(Length::Fill)
            .style(ui::surface),
    ]
    .spacing(14)
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_json_populates_terminal_appearance_and_preserves_full_settings() {
        let installed = installed_terminal_fonts();
        let configured_font = installed.first().cloned().unwrap_or_else(|| "Menlo".into());
        let value = serde_json::json!({
            "s3_endpoint": "https://example.invalid",
            "s3_bucket": null,
            "s3_access_key": null,
            "s3_secret_key": null,
            "sync_local_path": null,
            "scrollback_lines": 4096,
            "terminal_font_family": configured_font.clone(),
            "terminal_font_size": 15.0,
            "terminal_cursor_blink": false
        });
        let i18n = I18n::new(vida_core::i18n::Lang::ZhCn);
        let state = State::from_json(&value, &i18n);

        let appearance = state.terminal_appearance();
        assert_eq!(appearance.font_family, configured_font);
        assert_eq!(appearance.font_size, 15.0);
        assert!(!appearance.cursor_blink);
        assert_eq!(
            state.vault_settings.s3_endpoint.as_deref(),
            Some("https://example.invalid")
        );
        assert!(
            state
                .terminal_font_families
                .contains(&state.terminal_font_family),
            "当前字体必须来自下拉选项"
        );
    }

    #[test]
    fn terminal_font_selection_falls_back_to_bundled_default() {
        let installed = vec!["JetBrains Mono".to_string(), "Menlo".to_string()];
        assert_eq!(
            selected_terminal_font("not installed", &installed),
            "JetBrains Mono"
        );
        assert_eq!(
            selected_terminal_font("jetbrains mono", &installed),
            "JetBrains Mono"
        );
    }

    #[test]
    fn terminal_font_list_excludes_unrenderable_bitmap_faces() {
        assert!(
            installed_terminal_fonts()
                .iter()
                .all(|name| !name.contains("Bitmap"))
        );
    }

    #[test]
    fn terminal_font_size_uses_nearest_available_choice() {
        assert_eq!(selected_terminal_font_size(13.0), 13);
        assert_eq!(selected_terminal_font_size(17.0), 16);
        assert_eq!(selected_terminal_font_size(31.0), 32);
    }
}
