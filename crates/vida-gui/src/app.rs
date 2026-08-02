use iced::{Element, Task, Theme};
use vida_core::i18n::{self, I18n};

// s6/s7/s8 (sync conflict screens) are kept for the future sync trigger entry
#[allow(unused_imports)]
use crate::screens::{
    Screen, Tab, s0_connection, s1_setup, s2_unlock, s3_main, s4_credential, s5_settings,
    s6_conflict, s7_conflict_file, s8_remote_missing, s9_backup,
};
use crate::ws_client::WsClient;

pub fn run() -> Result<(), iced::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vida_gui=info".into()),
        )
        .init();

    iced::application(new, update, view)
        .title(|_: &VidaApp| "vida".to_string())
        .theme(|_: &VidaApp| Theme::Dark)
        .centered()
        .window_size((1024.0, 768.0))
        .run()
}

#[derive(Debug)]
pub struct VidaApp {
    ws_client: Option<WsClient>,
    screen: Screen,
    // Tab system (Tabby-style)
    tabs: Vec<Tab>,
    active_tab_id: String,
    // Per-tab state (not stored in app.screen)
    editor_state: Option<s4_credential::State>,
    settings_state: Option<s5_settings::State>,
    // UI language
    i18n: I18n,
    // Quick connect panel
    show_connect_panel: bool,
    connect_panel_search: String,
    recent_host_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum AppMessage {
    // Connection
    WsConnected(WsClient),
    WsError(String),
    RetryConnection,
    DaemonChecked { locked: bool, vault_exists: bool },

    // Tab management
    SwitchTab(String), // tab id
    OpenAddHostTab,    // open host picker/add tab
    OpenSettingsTab,   // open settings as tab
    CloseTab(String),  // tab id
    // Quick connect panel
    ToggleConnectPanel,
    CloseConnectPanel,
    ConnectPanelSearch(String),
    QuickConnectHost(String), // host_id
    QuickAddHost,

    // S1: Setup
    SetupPassphraseChanged(String),
    SetupConfirmPassphraseChanged(String),
    SetupRiskConfirmed(bool),
    SetupCreateVault,
    SetupVaultCreated,

    // S2: Unlock
    UnlockPassphraseChanged(String),
    UnlockRememberToggled(bool),
    UnlockVault,
    UnlockSuccess,
    UnlockFailed(String),

    // S3: Main
    HostsLoaded(Vec<s3_main::HostItem>),
    EditHost(String),
    DeleteHostConfirm(String),
    DeleteHost,
    RevealCredential(String),
    ShowCredential(String), // credential
    LockVault,
    VaultLocked,
    OpenBackup,

    // S4: Host editor
    EditorNameChanged(String),
    EditorHostChanged(String),
    EditorUserChanged(String),
    EditorPortChanged(String),
    EditorPasswordChanged(String),
    EditorTagsChanged(String),
    EditorGroupChanged(String),
    EditorNotesChanged(String),
    EditorSave,
    EditorSaved,
    EditorCancel,

    // S5: Settings
    SettingsLoaded(serde_json::Value),
    SettingsSyncPathChanged(String),
    SettingsScrollbackChanged(String),
    SettingsLanguageChanged(crate::screens::s5_settings::LangChoice),
    SettingsSectionChanged(crate::screens::s5_settings::SettingsSection),
    SettingsSave,
    SettingsSaved,

    // S6: Conflict
    ConflictResolveLocal,
    ConflictResolveRemote,
    ConflictResolved,

    // S7: Conflict files
    ConflictFileAdopt,
    ConflictFileIgnore,
    ConflictFileHandled,

    // S8: Remote missing
    RemoteMissingAction(String),
    RemoteMissingHandled,

    // S9: Backup
    BackupUseCurrentToggled(bool),
    BackupPassphraseChanged(String),
    BackupExport,
    BackupExported(String),
    BackupBack,
}

fn new() -> (VidaApp, Task<AppMessage>) {
    let i18n = I18n::new(i18n::detect_lang());
    let connecting = i18n.tr("connection_connecting");
    let app = VidaApp {
        ws_client: None,
        screen: Screen::ConnectionFailure(s0_connection::State::new(connecting.into())),
        tabs: Vec::new(),
        active_tab_id: String::new(),
        editor_state: None,
        settings_state: None,
        i18n,
        show_connect_panel: false,
        connect_panel_search: String::new(),
        recent_host_ids: Vec::new(),
    };

    let connect = Task::perform(
        async {
            match WsClient::connect().await {
                Ok(client) => AppMessage::WsConnected(client),
                Err(e) => AppMessage::WsError(e.to_string()),
            }
        },
        |r| r,
    );

    (app, connect)
}

fn update(app: &mut VidaApp, message: AppMessage) -> Task<AppMessage> {
    match message {
        // ---- Connection ----
        AppMessage::WsConnected(client) => {
            app.ws_client = Some(client);
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.vault_status().await {
                        Ok(s) => {
                            let locked = s.get("locked").and_then(|v| v.as_bool()).unwrap_or(true);
                            let vault_exists = s
                                .get("vault_exists")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            AppMessage::DaemonChecked {
                                locked,
                                vault_exists,
                            }
                        }
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::WsError(e) => {
            app.screen = Screen::ConnectionFailure(s0_connection::State::new(e));
            Task::none()
        }
        AppMessage::RetryConnection => Task::perform(
            async {
                match WsClient::connect().await {
                    Ok(client) => AppMessage::WsConnected(client),
                    Err(e) => AppMessage::WsError(e.to_string()),
                }
            },
            |r| r,
        ),
        AppMessage::DaemonChecked {
            locked,
            vault_exists,
        } => {
            if !vault_exists {
                app.screen = Screen::Setup(s1_setup::State::new());
            } else if locked {
                app.screen = Screen::Unlock(s2_unlock::State::new());
            } else {
                // vault exists and unlocked → load hosts and go to Main
                let client = app.ws_client.as_ref().unwrap().clone();
                return Task::perform(
                    async move {
                        match client.list_hosts().await {
                            Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                            Err(e) => AppMessage::WsError(e.to_string()),
                        }
                    },
                    |r| r,
                );
            }
            Task::none()
        }

        // ---- S1: Setup ----
        AppMessage::SetupPassphraseChanged(p) => {
            if let Screen::Setup(s) = &mut app.screen {
                s.passphrase = p;
            }
            Task::none()
        }
        AppMessage::SetupConfirmPassphraseChanged(p) => {
            if let Screen::Setup(s) = &mut app.screen {
                s.confirm_passphrase = p;
            }
            Task::none()
        }
        AppMessage::SetupRiskConfirmed(v) => {
            if let Screen::Setup(s) = &mut app.screen {
                s.risk_confirmed = v;
            }
            Task::none()
        }
        AppMessage::SetupCreateVault => {
            if let Screen::Setup(s) = &mut app.screen {
                s.creating = true;
                s.error = None;
                let passphrase = s.passphrase.clone();
                let client = app.ws_client.as_ref().unwrap().clone();
                Task::perform(
                    async move {
                        match client.create_vault(&passphrase).await {
                            Ok(_) => AppMessage::SetupVaultCreated,
                            Err(e) => AppMessage::WsError(e.to_string()),
                        }
                    },
                    |r| r,
                )
            } else {
                Task::none()
            }
        }
        AppMessage::SetupVaultCreated => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }

        // ---- S2: Unlock ----
        AppMessage::UnlockPassphraseChanged(p) => {
            if let Screen::Unlock(s) = &mut app.screen {
                s.passphrase = p;
            }
            Task::none()
        }
        AppMessage::UnlockRememberToggled(v) => {
            if let Screen::Unlock(s) = &mut app.screen {
                s.remember = v;
            }
            Task::none()
        }
        AppMessage::UnlockVault => {
            if let Screen::Unlock(s) = &mut app.screen {
                s.unlocking = true;
                s.error = None;
                let passphrase = s.passphrase.clone();
                let remember = s.remember;
                let client = app.ws_client.as_ref().unwrap().clone();
                Task::perform(
                    async move {
                        match client.unlock(&passphrase, remember).await {
                            Ok(_) => AppMessage::UnlockSuccess,
                            Err(e) => AppMessage::UnlockFailed(e.to_string()),
                        }
                    },
                    |r| r,
                )
            } else {
                Task::none()
            }
        }
        AppMessage::UnlockFailed(e) => {
            if let Screen::Unlock(s) = &mut app.screen {
                s.unlocking = false;
                s.error = Some(e);
            }
            Task::none()
        }
        AppMessage::UnlockSuccess => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }

        // ---- Tab management ----
        AppMessage::SwitchTab(tab_id) => {
            app.active_tab_id = tab_id;
            Task::none()
        }
        AppMessage::OpenAddHostTab => {
            // Add a new "新增主机" tab if not already present
            let existing = app.tabs.iter().find(|t| t.id == "add_host");
            if existing.is_none() {
                app.tabs
                    .push(Tab::add_host(app.i18n.tr("main_add_host_tab").to_string()));
            }
            app.active_tab_id = "add_host".into();
            app.editor_state = Some(s4_credential::State::new_add());
            Task::none()
        }
        AppMessage::OpenSettingsTab => {
            // Add settings tab if not already present, or switch to it
            let existing = app.tabs.iter().find(|t| t.id == "settings");
            if existing.is_none() {
                app.tabs
                    .push(Tab::settings(app.i18n.tr("main_tab_settings").to_string()));
            }
            app.active_tab_id = "settings".into();
            // Load settings if not already loaded
            if app.settings_state.is_none() {
                let client = app.ws_client.as_ref().unwrap().clone();
                Task::perform(
                    async move {
                        match client.get_settings().await {
                            Ok(val) => AppMessage::SettingsLoaded(val),
                            Err(e) => AppMessage::WsError(e.to_string()),
                        }
                    },
                    |r| r,
                )
            } else {
                Task::none()
            }
        }
        AppMessage::CloseTab(tab_id) => {
            app.tabs.retain(|t| t.id != tab_id);
            if app.active_tab_id == tab_id {
                app.active_tab_id = app.tabs.first().map(|t| t.id.clone()).unwrap_or_default();
            }
            Task::none()
        }
        AppMessage::ToggleConnectPanel => {
            app.show_connect_panel = !app.show_connect_panel;
            app.connect_panel_search.clear();
            if app.show_connect_panel {
                // Focus the search input when panel opens
                use iced::widget::Id;
                let search_id = Id::from("connect_panel_search");
                iced::widget::operation::focus::<AppMessage>(search_id).map(|_| unreachable!())
            } else {
                Task::none()
            }
        }
        AppMessage::CloseConnectPanel => {
            app.show_connect_panel = false;
            Task::none()
        }
        AppMessage::ConnectPanelSearch(q) => {
            app.connect_panel_search = q;
            Task::none()
        }
        AppMessage::QuickConnectHost(host_id) => {
            // Open host tab and close panel
            app.show_connect_panel = false;
            let existing = app.tabs.iter().find(|t| t.id == host_id);
            if existing.is_none()
                && let Screen::Main(s) = &app.screen
                && let Some(h) = s.hosts.iter().find(|h| h.id == host_id)
            {
                app.tabs.push(Tab::host(host_id.clone(), h.name.clone()));
            }
            app.active_tab_id = host_id.clone();
            // Update recent hosts: move to front, dedup, limit to 10
            app.recent_host_ids.retain(|id| *id != host_id);
            app.recent_host_ids.insert(0, host_id);
            app.recent_host_ids.truncate(10);
            Task::none()
        }
        AppMessage::QuickAddHost => {
            app.show_connect_panel = false;
            let existing = app.tabs.iter().find(|t| t.id == "add_host");
            if existing.is_none() {
                app.tabs
                    .push(Tab::add_host(app.i18n.tr("main_add_host_tab").to_string()));
            }
            app.active_tab_id = "add_host".into();
            app.editor_state = Some(s4_credential::State::new_add());
            Task::none()
        }

        // ---- S3: Main ----
        AppMessage::HostsLoaded(hosts) => {
            use crate::screens::TabKind;
            let search_query = if let Screen::Main(s) = &app.screen {
                s.search_query.clone()
            } else {
                String::new()
            };
            // Preserve non-host tabs (settings, add host, edit host)
            let non_host_tabs: Vec<Tab> = app
                .tabs
                .iter()
                .filter(|t| !matches!(t.kind, TabKind::Host { .. }))
                .cloned()
                .collect();
            // Create tabs for hosts
            let host_tabs: Vec<Tab> = hosts
                .iter()
                .map(|h| Tab::host(h.id.clone(), h.name.clone()))
                .collect();
            // Merge: host tabs first, then non-host tabs
            app.tabs = host_tabs;
            app.tabs.extend(non_host_tabs);
            // Preserve active tab if it still exists (e.g., Settings tab after re-entry)
            if !app.active_tab_id.is_empty() && app.tabs.iter().any(|t| t.id == app.active_tab_id) {
                // Keep current active tab
            } else {
                // Set active tab to first host, or first tab if no hosts
                app.active_tab_id = app.tabs.first().map(|t| t.id.clone()).unwrap_or_default();
            }
            app.screen = Screen::Main(s3_main::State {
                hosts,
                search_query,
            });
            Task::none()
        }
        AppMessage::EditHost(host_id) => {
            // Find host data and open editor in a new tab
            if let Screen::Main(s) = &app.screen
                && let Some(h) = s.hosts.iter().find(|h| h.id == host_id)
            {
                let tab_id = format!("edit_{}", host_id);
                let existing = app.tabs.iter().find(|t| t.id == tab_id);
                if existing.is_none() {
                    app.tabs.push(Tab::edit_host(
                        host_id.clone(),
                        app.i18n.trf("main_edit_host_tab", &[&h.name]),
                    ));
                }
                app.active_tab_id = tab_id;
                app.editor_state = Some(s4_credential::State::new_edit(
                    h.id.clone(),
                    h.name.clone(),
                    h.host.clone(),
                    h.user.clone(),
                    h.port,
                    h.tags.clone(),
                    h.group.clone(),
                    h.color.clone(),
                    h.notes.clone(),
                ));
            }
            Task::none()
        }
        AppMessage::DeleteHostConfirm(host_id) => {
            // TODO: Show confirmation dialog. For now, delete directly.
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client
                        .send("DeleteHost", serde_json::json!({"host_id": host_id}))
                        .await
                    {
                        Ok(_) => AppMessage::DeleteHost,
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::DeleteHost => {
            // Reload hosts after deletion
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::RevealCredential(host_id) => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.reveal_credential(&host_id).await {
                        Ok(val) => {
                            let cred = val
                                .get("credential")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            AppMessage::ShowCredential(cred)
                        }
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::ShowCredential(_cred) => {
            // Return to main screen (credential display removed from S4 scope)
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }

        // ---- S4: Host editor ----
        AppMessage::EditorNameChanged(v) => {
            if let Some(s) = &mut app.editor_state {
                s.name = v;
            }
            Task::none()
        }
        AppMessage::EditorHostChanged(v) => {
            if let Some(s) = &mut app.editor_state {
                s.host = v;
            }
            Task::none()
        }
        AppMessage::EditorUserChanged(v) => {
            if let Some(s) = &mut app.editor_state {
                s.user = v;
            }
            Task::none()
        }
        AppMessage::EditorPortChanged(v) => {
            if let Some(s) = &mut app.editor_state {
                s.port = v;
            }
            Task::none()
        }
        AppMessage::EditorPasswordChanged(v) => {
            if let Some(s) = &mut app.editor_state {
                // Intercept: if user clears the field on an edit, warn them
                if v.is_empty()
                    && !s.password.is_empty()
                    && matches!(s.mode, s4_credential::EditorMode::Edit { .. })
                {
                    s.password_cleared = true;
                }
                s.password = v;
                s.password_cleared = false;
            }
            Task::none()
        }
        AppMessage::EditorTagsChanged(v) => {
            if let Some(s) = &mut app.editor_state {
                s.tags = v;
            }
            Task::none()
        }
        AppMessage::EditorGroupChanged(v) => {
            if let Some(s) = &mut app.editor_state {
                s.group = v;
            }
            Task::none()
        }
        AppMessage::EditorNotesChanged(v) => {
            if let Some(s) = &mut app.editor_state {
                s.notes = v;
            }
            Task::none()
        }
        AppMessage::EditorSave => {
            if let Some(s) = &mut app.editor_state {
                s.saving = true;
                s.error = None;
                let host_id = match &s.mode {
                    s4_credential::EditorMode::Edit { host_id } => Some(host_id.clone()),
                    _ => None,
                };
                let name = s.name.clone();
                let host = s.host.clone();
                let user = s.user.clone();
                let port: u16 = s.port.parse().unwrap_or(22);
                let tags: Vec<String> = s
                    .tags
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                let group = if s.group.is_empty() {
                    None
                } else {
                    Some(s.group.clone())
                };
                let notes = if s.notes.is_empty() {
                    None
                } else {
                    Some(s.notes.clone())
                };
                // Password: empty on edit → None (keep existing); non-empty → Some
                let password = if s.password.is_empty() {
                    None // keep existing (edit) or validation catches (add)
                } else {
                    Some(s.password.clone())
                };
                let client = app.ws_client.as_ref().unwrap().clone();
                Task::perform(
                    async move {
                        let req = serde_json::json!({
                            "host": {
                                "id": host_id,
                                "name": name,
                                "host": host,
                                "user": user,
                                "port": port,
                                "tags": tags,
                                "group": group,
                                "color": null,
                                "password": password,
                                "notes": notes,
                            }
                        });
                        match client.send("UpdateHost", req).await {
                            Ok(_) => AppMessage::EditorSaved,
                            Err(e) => AppMessage::WsError(e.to_string()),
                        }
                    },
                    |r| r,
                )
            } else {
                Task::none()
            }
        }
        AppMessage::EditorSaved => {
            // Close the editor tab and reload hosts
            let active_id = app.active_tab_id.clone();
            app.tabs.retain(|t| {
                t.id != active_id
                    && t.kind != crate::screens::TabKind::AddHost
                    && !t.id.starts_with("edit_")
            });
            app.active_tab_id = app.tabs.first().map(|t| t.id.clone()).unwrap_or_default();
            app.editor_state = None;
            // Reload hosts
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::EditorCancel => {
            // Close the editor tab and reload hosts
            let active_id = app.active_tab_id.clone();
            app.tabs.retain(|t| {
                t.id != active_id
                    && t.kind != crate::screens::TabKind::AddHost
                    && !t.id.starts_with("edit_")
            });
            app.active_tab_id = app.tabs.first().map(|t| t.id.clone()).unwrap_or_default();
            app.editor_state = None;
            // Reload hosts
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }

        AppMessage::LockVault => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.lock().await {
                        Ok(_) => AppMessage::VaultLocked,
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::VaultLocked => {
            // Just switch to unlock screen, preserve all tabs and their state
            app.screen = Screen::Unlock(s2_unlock::State::new());
            Task::none()
        }
        AppMessage::OpenBackup => {
            app.screen = Screen::Backup(s9_backup::State::new());
            Task::none()
        }

        // ---- S5: Settings ----
        AppMessage::SettingsLoaded(val) => {
            let mut settings = s5_settings::State::from_json(&val, &app.i18n);
            // Pass hosts from main screen to settings
            if let Screen::Main(s) = &app.screen {
                settings.set_hosts(s.hosts.clone());
            }
            app.settings_state = Some(settings);
            Task::none()
        }
        AppMessage::SettingsLanguageChanged(choice) => {
            if let Some(s) = &mut app.settings_state {
                s.language = choice.clone();
            }
            // Save to config file
            let choice_str = choice.as_str().to_string();
            let _ = vida_core::config::save_language_choice(&choice_str);
            // Rebuild i18n: explicit choice applies immediately
            if choice_str == "system" {
                app.i18n = I18n::new(vida_core::i18n::detect_lang());
            } else {
                let lang = vida_core::i18n::Lang::from_code(&choice_str)
                    .unwrap_or(vida_core::i18n::Lang::En);
                app.i18n = I18n::new(lang);
            }
            // Refresh non-host tab names (settings tab name changes with language)
            for tab in app.tabs.iter_mut() {
                match &tab.kind {
                    crate::screens::TabKind::Settings => {
                        tab.name = app.i18n.tr("main_tab_settings").to_string();
                    }
                    crate::screens::TabKind::AddHost => {
                        tab.name = app.i18n.tr("main_add_host_tab").to_string();
                    }
                    _ => {}
                }
            }
            Task::none()
        }
        AppMessage::SettingsSectionChanged(section) => {
            if let Some(s) = &mut app.settings_state {
                s.active_section = section;
                s.saved = false;
                s.error = None;
            }
            Task::none()
        }
        AppMessage::SettingsSyncPathChanged(p) => {
            if let Some(s) = &mut app.settings_state {
                s.sync_local_path = p;
                s.saved = false;
            }
            Task::none()
        }
        AppMessage::SettingsScrollbackChanged(v) => {
            if let Some(s) = &mut app.settings_state {
                s.scrollback_lines = v;
                s.saved = false;
            }
            Task::none()
        }
        AppMessage::SettingsSave => {
            if let Some(s) = &mut app.settings_state {
                s.saving = true;
                s.error = None;
                let scrollback = s.scrollback_lines.parse::<usize>().unwrap_or(5000);
                let sync_path = if s.sync_local_path.is_empty() {
                    None
                } else {
                    Some(s.sync_local_path.clone())
                };
                let client = app.ws_client.as_ref().unwrap().clone();
                Task::perform(
                    async move {
                        let settings = serde_json::json!({
                            "sync_local_path": sync_path,
                            "scrollback_lines": scrollback,
                        });
                        match client.update_settings(settings).await {
                            Ok(_) => AppMessage::SettingsSaved,
                            Err(e) => AppMessage::WsError(e.to_string()),
                        }
                    },
                    |r| r,
                )
            } else {
                Task::none()
            }
        }
        AppMessage::SettingsSaved => {
            if let Some(s) = &mut app.settings_state {
                s.saving = false;
                s.saved = true;
            }
            Task::none()
        }

        // ---- S6: Conflict ----
        AppMessage::ConflictResolveLocal => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client
                        .send("ResolveConflict", serde_json::json!({"choice": "local"}))
                        .await
                    {
                        Ok(_) => AppMessage::ConflictResolved,
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::ConflictResolveRemote => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client
                        .send("ResolveConflict", serde_json::json!({"choice": "remote"}))
                        .await
                    {
                        Ok(_) => AppMessage::ConflictResolved,
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::ConflictResolved => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }

        // ---- S7: Conflict files ----
        AppMessage::ConflictFileAdopt => {
            if let Screen::ConflictFile(s) = &app.screen
                && let Some(first) = s.files.first()
            {
                let path = first.path.clone();
                let client = app.ws_client.as_ref().unwrap().clone();
                return Task::perform(
                    async move {
                        match client
                            .send("AdoptConflictFile", serde_json::json!({"path": path}))
                            .await
                        {
                            Ok(_) => AppMessage::ConflictFileHandled,
                            Err(e) => AppMessage::WsError(e.to_string()),
                        }
                    },
                    |r| r,
                );
            }
            Task::none()
        }
        AppMessage::ConflictFileIgnore => {
            if let Screen::ConflictFile(s) = &app.screen
                && let Some(first) = s.files.first()
            {
                let path = first.path.clone();
                let client = app.ws_client.as_ref().unwrap().clone();
                return Task::perform(
                    async move {
                        match client
                            .send("IgnoreConflictFile", serde_json::json!({"path": path}))
                            .await
                        {
                            Ok(_) => AppMessage::ConflictFileHandled,
                            Err(e) => AppMessage::WsError(e.to_string()),
                        }
                    },
                    |r| r,
                );
            }
            Task::none()
        }
        AppMessage::ConflictFileHandled => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }

        // ---- S8: Remote missing ----
        AppMessage::RemoteMissingAction(action) => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client
                        .send("HandleRemoteMissing", serde_json::json!({"action": action}))
                        .await
                    {
                        Ok(_) => AppMessage::RemoteMissingHandled,
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::RemoteMissingHandled => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }

        // ---- S9: Backup ----
        AppMessage::BackupUseCurrentToggled(v) => {
            if let Screen::Backup(s) = &mut app.screen {
                s.use_current = v;
            }
            Task::none()
        }
        AppMessage::BackupPassphraseChanged(p) => {
            if let Screen::Backup(s) = &mut app.screen {
                s.export_passphrase = p;
            }
            Task::none()
        }
        AppMessage::BackupExport => {
            if let Screen::Backup(s) = &mut app.screen {
                s.exporting = true;
                s.error = None;
                let passphrase = if s.use_current {
                    None
                } else {
                    Some(s.export_passphrase.clone())
                };
                let client = app.ws_client.as_ref().unwrap().clone();
                let i18n = app.i18n.clone();
                Task::perform(
                    async move {
                        match client
                            .send(
                                "ExportBackup",
                                serde_json::json!({"passphrase": passphrase}),
                            )
                            .await
                        {
                            Ok(val) => {
                                let bytes = val.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                                AppMessage::BackupExported(
                                    i18n.trf("backup_exported", &[&bytes.to_string()]),
                                )
                            }
                            Err(e) => AppMessage::WsError(e.to_string()),
                        }
                    },
                    |r| r,
                )
            } else {
                Task::none()
            }
        }
        AppMessage::BackupExported(msg) => {
            if let Screen::Backup(s) = &mut app.screen {
                s.exporting = false;
                s.result = Some(msg);
            }
            Task::none()
        }
        AppMessage::BackupBack => {
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.list_hosts().await {
                        Ok(hosts_val) => AppMessage::HostsLoaded(parse_hosts(&hosts_val)),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
    }
}

fn view(app: &VidaApp) -> Element<'_, AppMessage> {
    use crate::screens::TabKind;
    use iced::Length;
    use iced::widget::{column, container, text};

    match &app.screen {
        // Pre-main screens: full screen, no tab bar
        Screen::ConnectionFailure(_) | Screen::Setup(_) | Screen::Unlock(_) => {
            app.screen.view(&app.i18n)
        }

        // Main interface: tab bar + content based on active tab kind
        Screen::Main(s) => {
            let tab_bar = s3_main::State::view_tab_bar(
                &app.tabs,
                &app.active_tab_id,
                &app.i18n,
                app.show_connect_panel,
            );

            let content =
                if let Some(active_tab) = app.tabs.iter().find(|t| t.id == app.active_tab_id) {
                    match &active_tab.kind {
                        TabKind::Host { host_id } => s.view_host_detail(host_id, &app.i18n),
                        TabKind::AddHost | TabKind::EditHost { .. } => {
                            if let Some(editor) = &app.editor_state {
                                editor.view(&app.i18n)
                            } else {
                                text(app.i18n.tr("main_editor_loading")).into()
                            }
                        }
                        TabKind::Settings => {
                            if let Some(settings) = &app.settings_state {
                                settings.view(&app.i18n)
                            } else {
                                text(app.i18n.tr("main_settings_loading")).into()
                            }
                        }
                    }
                } else {
                    let placeholder = text(app.i18n.tr("main_no_hosts")).size(16);
                    container(placeholder)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .center_x(Length::Fill)
                        .center_y(Length::Fill)
                        .into()
                };

            let base = column![tab_bar, content]
                .width(Length::Fill)
                .height(Length::Fill);

            if app.show_connect_panel {
                // Floating overlay with dimmed background + panel
                use iced::Color;
                use iced::widget::button;
                use iced::widget::stack;

                let panel = s3_main::State::view_connect_panel(
                    &s.hosts,
                    &app.recent_host_ids,
                    &app.connect_panel_search,
                    &app.i18n,
                );

                // Semi-transparent overlay that closes panel on click.
                // Clip avoids edge artifacts when layered over the base content.
                let overlay_bg: Element<'_, AppMessage> = container(
                    button(text(""))
                        .on_press(AppMessage::CloseConnectPanel)
                        .style(button::text)
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true)
                .style(|_: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(Color::from_rgba(
                        0.0, 0.0, 0.0, 0.4,
                    ))),
                    ..Default::default()
                })
                .into();

                // Panel centered horizontally, near top with gap
                let panel_inner: Element<'_, AppMessage> = container(panel)
                    .padding(4)
                    .width(Length::Fixed(420.0))
                    .style(|_: &iced::Theme| container::Style {
                        background: Some(iced::Background::Color(Color::from_rgba(
                            0.12, 0.12, 0.15, 1.0,
                        ))),
                        border: iced::Border::default().rounded(8),
                        ..Default::default()
                    })
                    .into();

                // Full-screen layer with the same dim color as the overlay so no
                // underlying pixels peek through layer seams; panel sits on top.
                let panel_el: Element<'_, AppMessage> = container(panel_inner)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .clip(true)
                    .padding(iced::padding::Padding::new(0.0).top(50))
                    .center_x(Length::Fill)
                    .style(|_: &iced::Theme| container::Style {
                        background: Some(iced::Background::Color(Color::from_rgba(
                            0.0, 0.0, 0.0, 0.4,
                        ))),
                        ..Default::default()
                    })
                    .into();

                let base_el: Element<'_, AppMessage> = base.into();

                stack![base_el, overlay_bg, panel_el]
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            } else {
                base.into()
            }
        }

        // Other screens (conflict, etc.): show with tab bar
        _ => {
            let tab_bar =
                s3_main::State::view_tab_bar(&app.tabs, &app.active_tab_id, &app.i18n, false);
            let content = app.screen.view(&app.i18n);
            column![tab_bar, content]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        }
    }
}

fn parse_hosts(val: &serde_json::Value) -> Vec<s3_main::HostItem> {
    let arr = match val.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };

    arr.iter()
        .filter_map(|h| {
            Some(s3_main::HostItem {
                id: h.get("id")?.as_str()?.to_string(),
                name: h.get("name")?.as_str()?.to_string(),
                host: h.get("host")?.as_str()?.to_string(),
                user: h.get("user")?.as_str()?.to_string(),
                port: h.get("port")?.as_u64()? as u16,
                tags: h
                    .get("tags")?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                group: h.get("group").and_then(|v| v.as_str()).map(String::from),
                color: h.get("color").and_then(|v| v.as_str()).map(String::from),
                auth_kind: h.get("auth_kind")?.as_str()?.to_string(),
                notes: h.get("notes").and_then(|v| v.as_str()).map(String::from),
            })
        })
        .collect()
}
