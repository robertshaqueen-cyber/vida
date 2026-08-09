use std::collections::HashMap;

use iced::{Element, Task};
use vida_core::i18n::{self, I18n};

use crate::screens::{
    Screen, Tab, s_terminal, s0_connection, s1_setup, s2_unlock, s3_main, s4_credential,
    s5_settings, s6_conflict, s7_conflict_file, s8_remote_missing, s9_backup,
};
use crate::term::frame;
use crate::ws_client::{PushMsg, WsClient};

pub fn run() -> Result<(), iced::Error> {
    // 默认 filter 用 bin 名 "vida"（module_path! 以 bin 名为前缀，
    // 不是 package 名 "vida_gui"——用错会静默吞掉全部 GUI 日志）。
    // VIDA_LOG 可覆盖；输出到 stderr（stdout 在管道下是块缓冲）。
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            std::env::var("VIDA_LOG").unwrap_or_else(|_| "vida=info".to_string()),
        ))
        .with_writer(std::io::stderr)
        .init();

    iced::application(new, update, view)
        .font(crate::term::primitive::BUNDLED_REGULAR)
        .font(crate::term::primitive::BUNDLED_BOLD)
        .font(crate::ui::icons::FONT_BYTES)
        .subscription(subscription)
        .title(|_: &VidaApp| "vida".to_string())
        .theme(|_: &VidaApp| crate::ui::theme())
        .centered()
        .window_size((1024.0, 768.0))
        .run()
}

#[derive(Debug)]
pub struct VidaApp {
    ws_client: Option<WsClient>,
    screen: Screen,
    // Business data lives on VidaApp (not inside Screen), so any screen —
    // e.g. conflict/backup screens — can read the host list.
    hosts: Vec<s3_main::HostItem>,
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
    // Sync status for the tab bar indicator
    sync_state: SyncState,
    /// Monotonic token: invalidates pending credential-hide timers when the
    /// user switches tabs or reveals another credential.
    cred_hide_token: u64,
    /// Clipboard guard: (token, written content). ClearClipboard only wipes
    /// the clipboard if the current content still matches what we wrote.
    clipboard_guard: Option<(u64, String)>,
    clipboard_token: u64,
    /// 正式终端标签的 UI 状态。key 是稳定 tab id；会话重连后 session_id
    /// 可以变化，但 tab id 不变。
    terminal_sessions: HashMap<String, s_terminal::TerminalSession>,
    terminal_opening: bool,
    terminal_error: Option<String>,
    next_terminal_number: u32,
    /// 终端重连冷却（10 秒内最多重连一次，防止 daemon 未恢复时空转）。
    terminal_reconnect_cooldown: Option<std::time::Instant>,
    /// 当前持久化的终端外观；业务设置不放在 Screen 内。
    terminal_appearance: crate::term::primitive::TerminalAppearance,
    /// daemon 连接中断后保留终端标签；金库重新解锁后按原类型恢复会话。
    terminal_restore_pending: bool,
    /// 防止自动定时重连和手动“重试连接”同时建立两条连接。
    /// 成功连回 daemon 后保持为 true，直到终端标签恢复完成。
    terminal_reconnect_in_flight: bool,
}

/// Sync status shown by the tab bar sync button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncState {
    /// No sync has run yet in this session; state is unverified.
    #[default]
    Unknown,
    /// Last sync completed successfully.
    Synced,
    /// Sync request in flight.
    Syncing,
    /// Last sync failed.
    Error,
    /// Sync completed but requires user attention (conflict / conflict files /
    /// remote missing).
    NeedsAttention,
    /// Sync not configured — sync_local_path is empty.
    NotConfigured,
}

impl SyncState {
    /// Symbol for the tab bar button.
    pub fn symbol(&self) -> &'static str {
        match self {
            SyncState::Unknown => "—",
            SyncState::Synced => "✓",
            SyncState::Syncing => "⟳",
            SyncState::Error => "✗",
            SyncState::NeedsAttention => "▲",
            SyncState::NotConfigured => "—",
        }
    }

    pub fn label(&self, i18n: &I18n) -> String {
        match self {
            SyncState::Unknown => i18n.tr("sync_state_unknown").to_string(),
            SyncState::Synced => i18n.tr("sync_state_synced").to_string(),
            SyncState::Syncing => i18n.tr("sync_state_syncing").to_string(),
            SyncState::Error => i18n.tr("sync_state_error").to_string(),
            SyncState::NeedsAttention => i18n.tr("sync_state_needs_attention").to_string(),
            SyncState::NotConfigured => i18n.tr("sync_state_not_configured").to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AppMessage {
    // Formal local terminal tabs (M2b-3)
    OpenLocalTerminal,
    OpenSshTerminal(String),
    TerminalPush(PushMsg),
    /// 订阅建立完成。
    TerminalOpened {
        session_id: String,
        title: String,
        host_id: Option<String>,
    },
    /// 推送 stream 结束（后台连接断开）——会话标记为已断开。
    TerminalDisconnected {
        session_id: String,
    },
    /// 自动重连完成。每个 tuple 是 (旧 session_id, 新 session_id,
    /// 是否因 daemon 丢失旧会话而创建了替代会话)。
    TerminalReconnected {
        client: WsClient,
        mappings: Vec<(String, String, bool)>,
    },
    /// 自动重连失败；界面显示原因，并继续定时重试。
    TerminalReconnectFailed(String),
    /// 自动重连失败后的下一次定时尝试。
    TerminalReconnectRetry,
    TerminalSetupError(String),
    /// 人在终端 widget 中产生的原始输入字节。
    TerminalInput(Vec<u8>),
    /// 剪贴板文本；daemon 会按当前终端模式安全封装 bracketed paste。
    TerminalPaste(Vec<u8>),
    /// 终端画布按真实物理像素 cell 换算出的尺寸。
    TerminalResize {
        cols: u16,
        rows: u16,
    },
    /// Mouse wheel scroll in daemon-owned terminal history; positive moves up.
    TerminalScroll(i32),
    TerminalCursorBlink,

    // Connection
    WsConnected(WsClient),
    WsError(String),
    RetryConnection,
    DaemonChecked {
        locked: bool,
        vault_exists: bool,
    },

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
    UnlockFailed(String, Option<String>), // (message, category)
    UnlockToastHide,

    // S3: Main
    HostsLoaded(Vec<s3_main::HostItem>),
    EditHost(String),
    DeleteHostConfirm(String),
    DeleteHost,
    RevealCredential(String),
    ShowCredential(String), // credential (from RevealCredential)
    HideCredential(u64),    // hide token; stale timers are ignored
    CopyCredential(String),
    ClearClipboard(u64), // clipboard token; only clears if content matches
    SyncTriggered,
    SyncCompleted(serde_json::Value),
    LockVault,
    VaultLocked,

    // S4: Host editor
    EditorNameChanged(String),
    EditorHostChanged(String),
    EditorUserChanged(String),
    EditorPortChanged(String),
    EditorAuthChanged(crate::screens::s4_credential::AuthKindItem),
    EditorPasswordChanged(String),
    EditorKeyPathChanged(String),
    EditorPickKeyFile,
    EditorImportKeyFile,
    EditorKeyPassphraseChanged(String),
    EditorTagsChanged(String),
    EditorGroupChanged(String),
    EditorNotesChanged(String),
    EditorFocusNext,
    EditorFocusPrevious,
    EditorSave,
    EditorSaved,
    EditorCancel,

    // S5: Settings
    SettingsLoaded(serde_json::Value),
    SettingsSyncModeChanged(crate::screens::s5_settings::SyncModeItem),
    SettingsSyncPathChanged(String),
    SettingsSyncPickFolder,
    SettingsSyncQuickLocation(crate::screens::s5_settings::QuickLocation),
    SettingsScrollbackChanged(String),
    SettingsTerminalFontFamilyChanged(String),
    SettingsTerminalFontSizeChanged(u16),
    SettingsTerminalCursorBlinkChanged(bool),
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
    BackupFailed(String),
    BackupRestoreChooseFile,
    BackupRestorePassphraseChanged(String),
    BackupRestorePreview,
    BackupRestorePreviewed {
        data: Vec<u8>,
        host_count: usize,
        modified_at: String,
        host_names: Vec<String>,
    },
    BackupRestoreConfirm,
    BackupRestored {
        hosts: Vec<s3_main::HostItem>,
        settings: serde_json::Value,
        warning: Option<String>,
    },
    BackupRestoreFailed(String),
}

fn new() -> (VidaApp, Task<AppMessage>) {
    let i18n = I18n::new(i18n::detect_lang());
    let connecting = i18n.tr("connection_connecting");
    let app = VidaApp {
        ws_client: None,
        screen: Screen::ConnectionFailure(s0_connection::State::new(connecting.into())),
        hosts: Vec::new(),
        tabs: Vec::new(),
        active_tab_id: String::new(),
        editor_state: None,
        settings_state: None,
        i18n,
        show_connect_panel: false,
        connect_panel_search: String::new(),
        recent_host_ids: Vec::new(),
        sync_state: SyncState::default(),
        cred_hide_token: 0,
        clipboard_guard: None,
        clipboard_token: 0,
        terminal_sessions: HashMap::new(),
        terminal_opening: false,
        terminal_error: None,
        next_terminal_number: 1,
        terminal_reconnect_cooldown: None,
        terminal_appearance: crate::term::primitive::TerminalAppearance::default(),
        terminal_restore_pending: false,
        terminal_reconnect_in_flight: false,
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

impl VidaApp {
    /// Replace the host list AND rebuild host tabs from it.
    ///
    /// This is the ONLY entry point for modifying `app.hosts` — all other
    /// callers (HostsLoaded, SyncCompleted downloaded) must route through
    /// it so tab titles never drift from the host list.
    fn set_hosts(&mut self, hosts: Vec<s3_main::HostItem>) {
        use crate::screens::TabKind;
        self.hosts = hosts;
        // Preserve non-host tabs (settings, add host, edit host)
        let non_host_tabs: Vec<Tab> = self
            .tabs
            .iter()
            .filter(|t| !matches!(t.kind, TabKind::Host { .. }))
            .cloned()
            .collect();
        // Rebuild host tabs from the new list (names may have changed)
        let host_tabs: Vec<Tab> = self
            .hosts
            .iter()
            .map(|h| Tab::host(h.id.clone(), h.name.clone()))
            .collect();
        self.tabs = host_tabs;
        self.tabs.extend(non_host_tabs);
        // Preserve active tab if it still exists, else fall back to first
        if !self.active_tab_id.is_empty() && self.tabs.iter().any(|t| t.id == self.active_tab_id) {
            // Keep current active tab
        } else {
            self.active_tab_id = self.tabs.first().map(|t| t.id.clone()).unwrap_or_default();
        }
    }
}

fn active_terminal(app: &VidaApp) -> Option<&s_terminal::TerminalSession> {
    app.terminal_sessions.get(&app.active_tab_id)
}

fn active_terminal_mut(app: &mut VidaApp) -> Option<&mut s_terminal::TerminalSession> {
    app.terminal_sessions.get_mut(&app.active_tab_id)
}

fn classify_ssh_failure(i18n: &I18n, output: &str) -> String {
    let output = output.to_ascii_lowercase();
    let key = if output.contains("permission denied")
        || output.contains("authentication failed")
        || output.contains("too many authentication failures")
    {
        "terminal_ssh_auth_failed"
    } else if output.contains("connection refused") {
        "terminal_ssh_connection_refused"
    } else if output.contains("operation timed out")
        || output.contains("connection timed out")
        || output.contains("no route to host")
        || output.contains("network is unreachable")
    {
        "terminal_ssh_unreachable"
    } else if output.contains("could not resolve hostname")
        || output.contains("name or service not known")
    {
        "terminal_ssh_dns_failed"
    } else if output.contains("host key verification failed")
        || output.contains("remote host identification has changed")
    {
        "terminal_ssh_host_key_failed"
    } else {
        "terminal_ssh_failed_generic"
    };
    i18n.tr(key).to_string()
}

/// Close the daemon session owned by a terminal tab. Human input is already
/// serialized through the client's ordered queue; closing uses the same queue
/// so no late keystroke can overtake CloseSession.
fn close_terminal_session(app: &mut VidaApp, tab_id: &str) {
    let Some(session) = app.terminal_sessions.remove(tab_id) else {
        return;
    };
    if let Some(client) = app.ws_client.as_ref() {
        client.unsubscribe(&session.session_id);
        if let Err(error) = client.send_queued(
            "CloseSession",
            serde_json::json!({"session_id": session.session_id}),
        ) {
            tracing::warn!("关闭终端标签时未能关闭会话：{}", error.message);
        }
    }
}

fn close_all_terminal_sessions(app: &mut VidaApp) {
    let tab_ids: Vec<String> = app.terminal_sessions.keys().cloned().collect();
    for tab_id in tab_ids {
        close_terminal_session(app, &tab_id);
    }
    app.tabs
        .retain(|tab| !matches!(tab.kind, crate::screens::TabKind::Terminal { .. }));
    app.terminal_reconnect_cooldown = None;
    app.terminal_restore_pending = false;
    app.terminal_reconnect_in_flight = false;
}

fn restore_terminal_sessions(app: &VidaApp) -> Task<AppMessage> {
    let Some(client) = app.ws_client.as_ref().cloned() else {
        return Task::none();
    };
    let snapshots: Vec<(String, u16, u16, Option<String>)> = app
        .terminal_sessions
        .values()
        .filter(|session| session.closed)
        .map(|session| {
            (
                session.session_id.clone(),
                session.grid.rows,
                session.grid.cols,
                session.remote_host_id.clone(),
            )
        })
        .collect();
    if snapshots.is_empty() {
        return Task::none();
    }

    Task::perform(
        async move {
            let mut mappings = Vec::with_capacity(snapshots.len());
            for (old_session_id, rows, cols, remote_host_id) in snapshots {
                match client
                    .send(
                        "SubscribeSession",
                        serde_json::json!({"session_id": old_session_id}),
                    )
                    .await
                {
                    Ok(_) => {
                        let _ = client
                            .send(
                                "ResizeSession",
                                serde_json::json!({
                                    "session_id": old_session_id,
                                    "cols": cols,
                                    "rows": rows,
                                }),
                            )
                            .await;
                        mappings.push((old_session_id.clone(), old_session_id, false));
                    }
                    Err(error) if error.message.contains("会话不存在") => {
                        let reopened = match remote_host_id {
                            Some(host_id) => open_ssh_and_subscribe(&client, &host_id).await,
                            None => open_and_subscribe(&client).await,
                        };
                        match reopened {
                            Ok(new_session_id) => {
                                let _ = client
                                    .send(
                                        "ResizeSession",
                                        serde_json::json!({
                                            "session_id": new_session_id,
                                            "cols": cols,
                                            "rows": rows,
                                        }),
                                    )
                                    .await;
                                mappings.push((old_session_id, new_session_id, true));
                            }
                            Err(error) => {
                                return AppMessage::TerminalReconnectFailed(error);
                            }
                        }
                    }
                    Err(error) => {
                        return AppMessage::TerminalReconnectFailed(error.message);
                    }
                }
            }
            AppMessage::TerminalReconnected { client, mappings }
        },
        |message| message,
    )
}

/// Every open terminal tab gets its own push stream. Inactive tabs keep their
/// grids current without repaint polling; each receiver sleeps in recv().await
/// when no frame is available. A single 500 ms timer is added only when the
/// active terminal requests cursor blinking.
fn subscription(app: &VidaApp) -> iced::Subscription<AppMessage> {
    let mut subscriptions = Vec::new();

    let editor_is_active = app
        .tabs
        .iter()
        .find(|tab| tab.id == app.active_tab_id)
        .is_some_and(|tab| {
            matches!(
                tab.kind,
                crate::screens::TabKind::AddHost | crate::screens::TabKind::EditHost { .. }
            )
        });
    if editor_is_active {
        subscriptions.push(iced::event::listen_with(editor_focus_event));
    }

    let Some(ws) = app.ws_client.as_ref() else {
        return iced::Subscription::batch(subscriptions);
    };

    for (tab_id, session) in &app.terminal_sessions {
        // identity includes stable tab id, current session id, and WsClient's
        // Arc identity. Reconnect or replacement restarts only the right stream.
        let data = (tab_id.clone(), session.session_id.clone(), ws.clone());
        subscriptions.push(iced::Subscription::run_with(data, |d| {
            let sid = d.1.clone();
            let ws = d.2.clone();
            futures_util::stream::unfold(
                Some((
                    ws,
                    sid,
                    None::<tokio::sync::mpsc::UnboundedReceiver<PushMsg>>,
                )),
                |state| async move {
                    let (ws, sid, rx_opt) = state?;
                    let mut rx = match rx_opt {
                        Some(rx) => rx,
                        None => ws.subscribe(&sid),
                    };
                    let msg = match rx.recv().await {
                        Some(msg) => msg,
                        None => {
                            return Some((
                                AppMessage::TerminalDisconnected { session_id: sid },
                                None,
                            ));
                        }
                    };
                    Some((AppMessage::TerminalPush(msg), Some((ws, sid, Some(rx)))))
                },
            )
        }));
    }

    if active_terminal(app).is_some_and(|session| session.appearance.cursor_blink) {
        subscriptions.push(
            iced::time::every(std::time::Duration::from_millis(500))
                .map(|_| AppMessage::TerminalCursorBlink),
        );
    }

    iced::Subscription::batch(subscriptions)
}

fn editor_focus_event(
    event: iced::Event,
    _status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<AppMessage> {
    use iced::keyboard::{Key, key};

    match event {
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: Key::Named(key::Named::Tab),
            modifiers,
            repeat: false,
            ..
        }) if !modifiers.command() && !modifiers.control() && !modifiers.alt() => {
            Some(if modifiers.shift() {
                AppMessage::EditorFocusPrevious
            } else {
                AppMessage::EditorFocusNext
            })
        }
        _ => None,
    }
}

/// 打开本地会话并订阅推送，返回 session_id。
/// 首次打开与「原会话已结束」兜底路径共用（重连后必须重新订阅拿全量帧）。
async fn open_and_subscribe(client: &WsClient) -> Result<String, String> {
    let resp = client
        .send(
            "OpenLocalSession",
            serde_json::json!({"cols": 100, "rows": 40}),
        )
        .await
        .map_err(|e| format!("打开会话失败: {}", e.message))?;
    let sid = resp
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "OpenLocalSession 响应缺少 session_id".to_string())?
        .to_string();
    client
        .send("SubscribeSession", serde_json::json!({"session_id": sid}))
        .await
        .map_err(|e| format!("订阅失败: {}", e.message))?;
    Ok(sid)
}

/// 通过 daemon 使用金库中的主机与凭据打开系统 SSH，并订阅终端推送。
async fn open_ssh_and_subscribe(client: &WsClient, host_id: &str) -> Result<String, String> {
    let resp = client
        .send(
            "OpenSshSession",
            serde_json::json!({"host_id": host_id, "cols": 100, "rows": 40}),
        )
        .await
        .map_err(|e| format!("打开 SSH 会话失败: {}", e.message))?;
    let sid = resp
        .get("session_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "OpenSshSession 响应缺少 session_id".to_string())?
        .to_string();
    client
        .send("SubscribeSession", serde_json::json!({"session_id": sid}))
        .await
        .map_err(|e| format!("订阅 SSH 会话失败: {}", e.message))?;
    Ok(sid)
}

fn update(app: &mut VidaApp, message: AppMessage) -> Task<AppMessage> {
    match message {
        // ---- Connection ----
        AppMessage::WsConnected(client) => {
            if !app.terminal_restore_pending {
                app.terminal_reconnect_in_flight = false;
            }
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
            app.terminal_reconnect_in_flight = false;
            // If a sync was in flight, surface the failure via the sync indicator
            if app.sync_state == SyncState::Syncing {
                app.sync_state = SyncState::Error;
            }
            // Strip the internal AUTH_FAILED: marker used by the token-retry logic
            let display = e.strip_prefix("AUTH_FAILED:").unwrap_or(&e).to_string();
            app.screen = Screen::ConnectionFailure(s0_connection::State::new(display));
            Task::none()
        }
        AppMessage::RetryConnection | AppMessage::TerminalReconnectRetry => {
            if app.terminal_reconnect_in_flight {
                return Task::none();
            }
            app.terminal_reconnect_in_flight = true;
            let restoring_terminals = app.terminal_restore_pending;
            Task::perform(
                async move {
                    match WsClient::connect().await {
                        Ok(client) => AppMessage::WsConnected(client),
                        Err(e) if restoring_terminals => {
                            AppMessage::TerminalReconnectFailed(e.to_string())
                        }
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
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
                // Clear error and hide toast when user starts typing
                s.error = None;
                s.error_category = None;
                s.toast_visible = false;
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
                            Err(e) => AppMessage::UnlockFailed(e.message, e.category),
                        }
                    },
                    |r| r,
                )
            } else {
                Task::none()
            }
        }
        AppMessage::UnlockFailed(msg, category) => {
            if let Screen::Unlock(s) = &mut app.screen {
                s.unlocking = false;
                s.error = Some(msg);
                s.error_category = category;
                s.toast_visible = true;
            }
            // Focus password input and select all text for easy re-input
            use iced::widget::Id;
            let id = Id::from(crate::screens::s2_unlock::UNLOCK_PASSPHRASE_ID);
            let focus =
                iced::widget::operation::focus::<AppMessage>(id.clone()).map(|_| unreachable!());
            let select =
                iced::widget::operation::select_all::<AppMessage>(id).map(|_| unreachable!());
            // Auto-hide toast after 3 seconds
            let toast_hide = Task::perform(
                async {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    AppMessage::UnlockToastHide
                },
                |r| r,
            );
            Task::batch([focus, select, toast_hide])
        }
        AppMessage::UnlockToastHide => {
            if let Screen::Unlock(s) = &mut app.screen {
                s.toast_visible = false;
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
            // Invalidate any pending credential-hide timer and clear the
            // revealed credential: it belongs to the previous host.
            app.cred_hide_token += 1;
            if let Screen::Main(s) = &mut app.screen {
                s.revealed_credential = None;
                s.credential_copied = false;
            }
            if active_terminal(app).is_some() {
                iced::widget::operation::focus::<AppMessage>(crate::term::widget::id())
            } else {
                Task::none()
            }
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
            let closed_position = app.tabs.iter().position(|tab| tab.id == tab_id);
            close_terminal_session(app, &tab_id);
            app.tabs.retain(|t| t.id != tab_id);
            if app.active_tab_id == tab_id {
                app.active_tab_id = closed_position
                    .and_then(|position| {
                        let next = position.min(app.tabs.len().saturating_sub(1));
                        app.tabs.get(next)
                    })
                    .or_else(|| app.tabs.first())
                    .map(|tab| tab.id.clone())
                    .unwrap_or_default();
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
            // Quick connect is a direct SSH action; the host detail tab remains
            // available separately in the persistent host tabs.
            app.show_connect_panel = false;
            // Update recent hosts: move to front, dedup, limit to 10
            app.recent_host_ids.retain(|id| *id != host_id);
            app.recent_host_ids.insert(0, host_id.clone());
            app.recent_host_ids.truncate(10);
            update(app, AppMessage::OpenSshTerminal(host_id))
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
            // Business data lives on VidaApp so conflict/backup screens can
            // read it regardless of the current screen.
            app.set_hosts(hosts);
            app.screen = Screen::Main(s3_main::State {
                revealed_credential: None,
                credential_copied: false,
            });
            let settings_task = if app.settings_state.is_none() {
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
            };
            let restore_task = if app.terminal_restore_pending {
                restore_terminal_sessions(app)
            } else {
                Task::none()
            };
            Task::batch([settings_task, restore_task])
        }
        AppMessage::EditHost(host_id) => {
            // Find host data and open editor in a new tab
            if let Some(h) = app.hosts.iter().find(|h| h.id == host_id) {
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
                    h.auth_kind.clone(),
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
        AppMessage::ShowCredential(cred) => {
            // Store plaintext bound to the CURRENT host tab, auto-hide after
            // 15 seconds. The token invalidates any earlier pending hide timer.
            app.cred_hide_token += 1;
            let hide_token = app.cred_hide_token;
            let host_id = if matches!(
                app.tabs.iter().find(|t| t.id == app.active_tab_id),
                Some(t) if matches!(t.kind, crate::screens::TabKind::Host { .. })
            ) {
                app.active_tab_id.clone()
            } else {
                String::new()
            };
            if let Screen::Main(s) = &mut app.screen {
                s.revealed_credential = Some((host_id, cred));
                s.credential_copied = false;
            }
            Task::perform(
                async move {
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    AppMessage::HideCredential(hide_token)
                },
                |r| r,
            )
        }
        AppMessage::HideCredential(hide_token) => {
            // Only a timer whose token is still current may clear the display;
            // stale timers (tab switched, another reveal happened) are ignored.
            if hide_token == app.cred_hide_token
                && let Screen::Main(s) = &mut app.screen
            {
                s.revealed_credential = None;
                s.credential_copied = false;
            }
            Task::none()
        }
        AppMessage::CopyCredential(cred) => {
            // Write to clipboard, auto-clear after 45 seconds. The clipboard
            // guard records (token, content); ClearClipboard only wipes when
            // the token is current AND the clipboard still holds our content,
            // so user-copied text after ours is never cleared.
            app.clipboard_token += 1;
            let clear_token = app.clipboard_token;
            app.clipboard_guard = Some((clear_token, cred.clone()));
            if let Screen::Main(s) = &mut app.screen {
                s.credential_copied = true;
            }
            let copy = iced::clipboard::write::<AppMessage>(cred);
            let clear_after = Task::perform(
                async move {
                    tokio::time::sleep(std::time::Duration::from_secs(45)).await;
                    AppMessage::ClearClipboard(clear_token)
                },
                |r| r,
            );
            Task::batch([copy, clear_after])
        }
        AppMessage::ClearClipboard(clear_token) => {
            if let Screen::Main(s) = &mut app.screen {
                s.credential_copied = false;
            }
            // Stale timer (a newer copy happened after this one) → do nothing.
            let expected = match &app.clipboard_guard {
                Some((token, content)) if *token == clear_token => content.clone(),
                _ => return Task::none(),
            };
            app.clipboard_guard = None;
            // Read the clipboard first; only clear if it still contains what
            // we wrote (user may have copied something else since).
            iced::clipboard::read().then(move |current| {
                if current.as_deref() == Some(expected.as_str()) {
                    iced::clipboard::write::<AppMessage>(String::new())
                } else {
                    Task::none()
                }
            })
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
                s.credential_dirty = true;
                s.password_cleared = false;
            }
            Task::none()
        }
        AppMessage::EditorAuthChanged(item) => {
            if let Some(state) = &mut app.editor_state
                && state.auth_kind != item.kind
            {
                state.auth_kind = item.kind;
                state.password.clear();
                state.private_key_path.clear();
                state.inline_key.clear();
                state.key_passphrase.clear();
                state.credential_dirty = true;
            }
            Task::none()
        }
        AppMessage::EditorKeyPathChanged(value) => {
            if let Some(state) = &mut app.editor_state {
                state.private_key_path = value;
                state.credential_dirty = true;
            }
            Task::none()
        }
        AppMessage::EditorPickKeyFile => {
            if let Some(state) = &mut app.editor_state
                && let Some(path) = rfd::FileDialog::new()
                    .set_title(app.i18n.tr("editor_choose_key"))
                    .pick_file()
            {
                state.private_key_path = path.to_string_lossy().into_owned();
                state.credential_dirty = true;
                state.error = None;
            }
            Task::none()
        }
        AppMessage::EditorImportKeyFile => {
            if let Some(state) = &mut app.editor_state
                && let Some(path) = rfd::FileDialog::new()
                    .set_title(app.i18n.tr("editor_import_key"))
                    .pick_file()
            {
                match std::fs::read_to_string(path) {
                    Ok(contents) if !contents.is_empty() => {
                        state.inline_key = contents;
                        state.credential_dirty = true;
                        state.error = None;
                    }
                    Ok(_) => {
                        state.error = Some(app.i18n.tr("editor_key_empty").to_string());
                    }
                    Err(error) => {
                        state.error = Some(
                            app.i18n
                                .trf("editor_key_read_failed", &[&error.to_string()]),
                        );
                    }
                }
            }
            Task::none()
        }
        AppMessage::EditorKeyPassphraseChanged(value) => {
            if let Some(state) = &mut app.editor_state {
                state.key_passphrase = value;
                state.credential_dirty = true;
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
        AppMessage::EditorFocusNext => iced::widget::operation::focus_next(),
        AppMessage::EditorFocusPrevious => iced::widget::operation::focus_previous(),
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
                let auth = if !s.credential_dirty {
                    None
                } else {
                    let passphrase =
                        (!s.key_passphrase.is_empty()).then(|| s.key_passphrase.clone());
                    Some(match s.auth_kind {
                        s4_credential::AuthKind::Password => serde_json::json!({
                            "kind": "password",
                            "password": s.password,
                        }),
                        s4_credential::AuthKind::KeyFile => serde_json::json!({
                            "kind": "key",
                            "private_key_path": s.private_key_path,
                            "passphrase": passphrase,
                        }),
                        s4_credential::AuthKind::KeyInline => serde_json::json!({
                            "kind": "key_inline",
                            "private_key": s.inline_key,
                            "passphrase": passphrase,
                        }),
                    })
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
                                "password": null,
                                "auth": auth,
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

        AppMessage::SyncTriggered => {
            app.sync_state = SyncState::Syncing;
            let client = app.ws_client.as_ref().unwrap().clone();
            Task::perform(
                async move {
                    match client.sync().await {
                        Ok(val) => AppMessage::SyncCompleted(val),
                        Err(e) => AppMessage::WsError(e.to_string()),
                    }
                },
                |r| r,
            )
        }
        AppMessage::SyncCompleted(val) => {
            let status = val
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            match status {
                "conflict" => {
                    // Sync finished but requires a decision: the abandoned side
                    // goes to a backup file. Show real data, never empty lists.
                    app.sync_state = SyncState::NeedsAttention;
                    let remote_hosts: Vec<String> = val
                        .get("remote_hosts")
                        .and_then(|r| r.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|h| {
                                    h.get("name").and_then(|n| n.as_str()).map(String::from)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // Local list always comes from app.hosts (business data
                    // lives on VidaApp), so the conflict screen shows real
                    // data even when sync was triggered from another screen.
                    let local_hosts: Vec<String> =
                        app.hosts.iter().map(|h| h.name.clone()).collect();
                    app.screen =
                        Screen::Conflict(s6_conflict::State::new(local_hosts, remote_hosts));
                }
                "conflict_files_detected" => {
                    // Requires user attention (adopt/ignore conflict files)
                    app.sync_state = SyncState::NeedsAttention;
                    let files: Vec<s7_conflict_file::ConflictFileInfo> = val
                        .get("files")
                        .and_then(|f| f.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|f| {
                                    let path = f.get("path")?.as_str()?.to_string();
                                    let pattern = f.get("pattern")?.as_str()?.to_string();
                                    let pattern_display = match pattern.as_str() {
                                        "DropboxCopy" => {
                                            app.i18n.tr("app_conflict_dropbox_copy").to_string()
                                        }
                                        "DropboxVersion" => {
                                            app.i18n.tr("app_conflict_dropbox_version").to_string()
                                        }
                                        "Syncthing" => {
                                            app.i18n.tr("app_conflict_syncthing").to_string()
                                        }
                                        "IcloudPlaceholder" => {
                                            app.i18n.tr("app_conflict_icloud").to_string()
                                        }
                                        _ => app.i18n.tr("app_conflict_generic").to_string(),
                                    };
                                    Some(s7_conflict_file::ConflictFileInfo {
                                        path,
                                        pattern: pattern_display,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    let remote_hosts: Vec<String> = val
                        .get("remote_hosts")
                        .and_then(|r| r.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|h| {
                                    h.get("name").and_then(|n| n.as_str()).map(String::from)
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    app.screen =
                        Screen::ConflictFile(s7_conflict_file::State::new(files, remote_hosts));
                }
                "remote_missing" => {
                    // Requires user action (re-upload / clear state)
                    app.sync_state = SyncState::NeedsAttention;
                    app.screen = Screen::RemoteMissing(s8_remote_missing::State::new());
                }
                "downloaded" => {
                    app.sync_state = SyncState::Synced;
                    // Replace business data + rebuild host tabs (names may
                    // have changed on the remote side).
                    app.set_hosts(val.get("hosts").map(parse_hosts).unwrap_or_default());
                    if !matches!(app.screen, Screen::Main(_)) {
                        app.screen = Screen::Main(s3_main::State {
                            revealed_credential: None,
                            credential_copied: false,
                        });
                    }
                }
                "sync_not_configured" => {
                    app.sync_state = SyncState::NotConfigured;
                }
                _ => {
                    app.sync_state = SyncState::Synced;
                    tracing::info!("Sync completed: {}", status);
                }
            }
            Task::none()
        }

        AppMessage::LockVault => {
            // During daemon recovery the old client cannot perform Lock. More
            // importantly, closing tabs here would erase the metadata needed
            // to restore them. Preserve the tabs and make the action an
            // immediate reconnect attempt instead.
            if app.terminal_restore_pending {
                return update(app, AppMessage::RetryConnection);
            }
            let client = app.ws_client.as_ref().unwrap().clone();
            close_all_terminal_sessions(app);
            if !app.tabs.iter().any(|tab| tab.id == app.active_tab_id) {
                app.active_tab_id = app
                    .tabs
                    .first()
                    .map(|tab| tab.id.clone())
                    .unwrap_or_default();
            }
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
            app.terminal_opening = false;
            app.terminal_error = None;
            app.screen = Screen::Unlock(s2_unlock::State::new());
            Task::none()
        }
        // ---- S5: Settings ----
        AppMessage::SettingsLoaded(val) => {
            let settings = s5_settings::State::from_json(&val, &app.i18n);
            app.terminal_appearance = settings.terminal_appearance();
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
                    crate::screens::TabKind::Terminal { number, .. } => {
                        tab.name = app
                            .i18n
                            .trf("terminal_local_numbered", &[&number.to_string()]);
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
        AppMessage::SettingsSyncModeChanged(item) => {
            if let Some(s) = &mut app.settings_state {
                use crate::screens::s5_settings::SyncMode;
                s.sync_mode = item.mode;
                match item.mode {
                    SyncMode::None => {
                        s.sync_local_path.clear();
                    }
                    SyncMode::Local => {
                        // Keep existing path if any
                    }
                }
                s.saved = false;
            }
            Task::none()
        }
        AppMessage::SettingsSyncPickFolder => {
            if let Some(s) = &mut app.settings_state
                && let Some(handle) = rfd::FileDialog::new().pick_folder()
                && let Some(path) = handle.to_str()
            {
                s.sync_local_path = path.to_string();
                s.saved = false;
            }
            Task::none()
        }
        AppMessage::SettingsSyncQuickLocation(loc) => {
            if let Some(s) = &mut app.settings_state {
                use crate::screens::s5_settings::QuickLocation;
                let path = match loc {
                    QuickLocation::ICloud => {
                        let home = std::env::var("HOME").unwrap_or_default();
                        let icloud_base = std::path::PathBuf::from(&home)
                            .join("Library/Mobile Documents/com~apple~CloudDocs");
                        if !icloud_base.exists() {
                            s.error =
                                Some(app.i18n.tr("settings_sync_icloud_not_found").to_string());
                            return Task::none();
                        }
                        let vida_dir = icloud_base.join("vida");
                        if let Err(e) = std::fs::create_dir_all(&vida_dir) {
                            s.error = Some(format!(
                                "{}: {}",
                                app.i18n.tr("settings_sync_create_dir_failed"),
                                e
                            ));
                            return Task::none();
                        }
                        vida_dir.to_str().unwrap_or_default().to_string()
                    }
                    QuickLocation::Home => std::env::var("HOME").unwrap_or_default(),
                };
                s.sync_local_path = path;
                s.saved = false;
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
        AppMessage::SettingsTerminalFontFamilyChanged(v) => {
            if let Some(s) = &mut app.settings_state {
                s.terminal_font_family = v;
                s.saved = false;
            }
            Task::none()
        }
        AppMessage::SettingsTerminalFontSizeChanged(v) => {
            if let Some(s) = &mut app.settings_state {
                s.terminal_font_size = v;
                s.saved = false;
            }
            Task::none()
        }
        AppMessage::SettingsTerminalCursorBlinkChanged(v) => {
            if let Some(s) = &mut app.settings_state {
                s.terminal_cursor_blink = v;
                s.saved = false;
            }
            Task::none()
        }
        AppMessage::SettingsSave => {
            if let Some(s) = &mut app.settings_state {
                s.error = None;
                let scrollback = s.scrollback_lines.parse::<usize>().unwrap_or(3000);
                let font_family = s.terminal_font_family.clone();
                let font_size = s.terminal_font_size as f32;
                s.saving = true;
                let sync_path = if s.sync_local_path.is_empty() {
                    None
                } else {
                    Some(s.sync_local_path.clone())
                };
                let mut settings = s.vault_settings.clone();
                settings.sync_local_path = sync_path;
                settings.scrollback_lines = scrollback;
                settings.terminal_font_family = font_family;
                settings.terminal_font_size = font_size;
                settings.terminal_cursor_blink = s.terminal_cursor_blink;
                s.vault_settings = settings.clone();
                let client = app.ws_client.as_ref().unwrap().clone();
                Task::perform(
                    async move {
                        match serde_json::to_value(settings) {
                            Ok(settings) => match client.update_settings(settings).await {
                                Ok(_) => AppMessage::SettingsSaved,
                                Err(e) => AppMessage::WsError(e.to_string()),
                            },
                            Err(e) => AppMessage::WsError(format!("设置序列化失败：{}", e)),
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
                app.terminal_appearance = s.terminal_appearance();
                s.vault_settings.terminal_font_family = app.terminal_appearance.font_family.clone();
                s.vault_settings.terminal_font_size = app.terminal_appearance.font_size;
                s.vault_settings.terminal_cursor_blink = app.terminal_appearance.cursor_blink;
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
            // User resolved the situation; the follow-up list reload reflects
            // the synced state, so the indicator can leave NeedsAttention.
            app.sync_state = SyncState::Synced;
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
            // User resolved the situation; the follow-up list reload reflects
            // the synced state, so the indicator can leave NeedsAttention.
            app.sync_state = SyncState::Synced;
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
            // User resolved the situation; the follow-up list reload reflects
            // the synced state, so the indicator can leave NeedsAttention.
            app.sync_state = SyncState::Synced;
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
            if let Some(settings) = &mut app.settings_state {
                settings.backup.use_current = v;
            }
            Task::none()
        }
        AppMessage::BackupPassphraseChanged(p) => {
            if let Some(settings) = &mut app.settings_state {
                settings.backup.export_passphrase = p;
            }
            Task::none()
        }
        AppMessage::BackupExport => {
            if let Some(settings) = &mut app.settings_state {
                let Some(path) = rfd::FileDialog::new()
                    .set_title(app.i18n.tr("backup_choose_location"))
                    .set_file_name("vida-backup.age")
                    .add_filter("age", &["age"])
                    .save_file()
                else {
                    return Task::none();
                };

                let s = &mut settings.backup;
                s.exporting = true;
                s.error = None;
                s.result = None;
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
                                let data = match parse_backup_bytes(&val) {
                                    Ok(data) => data,
                                    Err(()) => {
                                        return AppMessage::BackupFailed(
                                            i18n.tr("backup_invalid_data").to_string(),
                                        );
                                    }
                                };
                                let bytes = data.len();
                                let path_display = path.display().to_string();
                                match tokio::fs::write(&path, data).await {
                                    Ok(()) => {
                                        AppMessage::BackupExported(i18n.trf(
                                            "backup_saved",
                                            &[&path_display, &bytes.to_string()],
                                        ))
                                    }
                                    Err(error) => AppMessage::BackupFailed(i18n.trf(
                                        "backup_write_failed",
                                        &[&path_display, &error.to_string()],
                                    )),
                                }
                            }
                            Err(e) => AppMessage::BackupFailed(e.to_string()),
                        }
                    },
                    |r| r,
                )
            } else {
                Task::none()
            }
        }
        AppMessage::BackupExported(msg) => {
            if let Some(settings) = &mut app.settings_state {
                settings.backup.exporting = false;
                settings.backup.result = Some(msg);
            }
            Task::none()
        }
        AppMessage::BackupFailed(message) => {
            if let Some(settings) = &mut app.settings_state {
                settings.backup.exporting = false;
                settings.backup.error = Some(message);
            }
            Task::none()
        }
        AppMessage::BackupRestoreChooseFile => {
            let title = app.i18n.tr("backup_restore_choose_file");
            let Some(path) = rfd::FileDialog::new()
                .set_title(title)
                .add_filter("age", &["age"])
                .pick_file()
            else {
                return Task::none();
            };
            if let Some(settings) = &mut app.settings_state {
                let backup = &mut settings.backup;
                backup.restore_path = path.display().to_string();
                backup.restore_data.clear();
                backup.restore_preview = None;
                backup.restore_result = None;
                backup.restore_error = None;
            }
            Task::none()
        }
        AppMessage::BackupRestorePassphraseChanged(passphrase) => {
            if let Some(settings) = &mut app.settings_state {
                let backup = &mut settings.backup;
                backup.restore_passphrase = passphrase;
                // The preview is bound to the exact passphrase and ciphertext
                // that were validated. Editing either requires validation again.
                backup.restore_data.clear();
                backup.restore_preview = None;
                backup.restore_result = None;
                backup.restore_error = None;
            }
            Task::none()
        }
        AppMessage::BackupRestorePreview => {
            let Some(settings) = &mut app.settings_state else {
                return Task::none();
            };
            let backup = &mut settings.backup;
            if backup.restore_path.is_empty() || backup.restore_passphrase.is_empty() {
                return Task::none();
            }
            backup.restore_validating = true;
            backup.restore_data.clear();
            backup.restore_preview = None;
            backup.restore_result = None;
            backup.restore_error = None;

            let path = std::path::PathBuf::from(&backup.restore_path);
            let path_display = backup.restore_path.clone();
            let passphrase = backup.restore_passphrase.clone();
            let client = app.ws_client.as_ref().unwrap().clone();
            let i18n = app.i18n.clone();
            Task::perform(
                async move {
                    let data = match tokio::fs::read(&path).await {
                        Ok(data) => data,
                        Err(error) => {
                            return AppMessage::BackupRestoreFailed(i18n.trf(
                                "backup_restore_read_failed",
                                &[&path_display, &error.to_string()],
                            ));
                        }
                    };
                    match client
                        .send(
                            "PreviewBackup",
                            serde_json::json!({
                                "data": data.clone(),
                                "passphrase": passphrase,
                            }),
                        )
                        .await
                    {
                        Ok(value) => {
                            let Some(host_count) = value
                                .get("host_count")
                                .and_then(serde_json::Value::as_u64)
                                .and_then(|count| usize::try_from(count).ok())
                            else {
                                return AppMessage::BackupRestoreFailed(
                                    i18n.tr("backup_restore_invalid_response").to_string(),
                                );
                            };
                            let Some(modified_at) = value
                                .get("modified_at")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                            else {
                                return AppMessage::BackupRestoreFailed(
                                    i18n.tr("backup_restore_invalid_response").to_string(),
                                );
                            };
                            let Some(host_names) = value
                                .get("host_names")
                                .and_then(serde_json::Value::as_array)
                                .map(|names| {
                                    names
                                        .iter()
                                        .filter_map(serde_json::Value::as_str)
                                        .map(str::to_string)
                                        .collect::<Vec<_>>()
                                })
                            else {
                                return AppMessage::BackupRestoreFailed(
                                    i18n.tr("backup_restore_invalid_response").to_string(),
                                );
                            };
                            AppMessage::BackupRestorePreviewed {
                                data,
                                host_count,
                                modified_at,
                                host_names,
                            }
                        }
                        Err(error) => AppMessage::BackupRestoreFailed(error.to_string()),
                    }
                },
                |message| message,
            )
        }
        AppMessage::BackupRestorePreviewed {
            data,
            host_count,
            modified_at,
            host_names,
        } => {
            if let Some(settings) = &mut app.settings_state {
                let backup = &mut settings.backup;
                backup.restore_validating = false;
                backup.restore_data = data;
                backup.restore_preview = Some(s9_backup::RestorePreview {
                    host_count,
                    modified_at,
                    host_names,
                });
            }
            Task::none()
        }
        AppMessage::BackupRestoreConfirm => {
            let Some(settings) = &mut app.settings_state else {
                return Task::none();
            };
            let backup = &mut settings.backup;
            if backup.restore_preview.is_none() || backup.restore_data.is_empty() {
                return Task::none();
            }
            backup.restoring = true;
            backup.restore_error = None;
            backup.restore_result = None;
            let data = backup.restore_data.clone();
            let passphrase = backup.restore_passphrase.clone();
            let client = app.ws_client.as_ref().unwrap().clone();
            let i18n = app.i18n.clone();
            Task::perform(
                async move {
                    let restored = match client
                        .send(
                            "RestoreBackup",
                            serde_json::json!({
                                "data": data,
                                "passphrase": passphrase,
                            }),
                        )
                        .await
                    {
                        Ok(value) => value,
                        Err(error) => {
                            return AppMessage::BackupRestoreFailed(error.to_string());
                        }
                    };
                    let Some(host_values) = restored.get("hosts") else {
                        return AppMessage::BackupRestoreFailed(
                            i18n.tr("backup_restore_invalid_response").to_string(),
                        );
                    };
                    let Some(settings) = restored.get("settings").cloned() else {
                        return AppMessage::BackupRestoreFailed(
                            i18n.tr("backup_restore_invalid_response").to_string(),
                        );
                    };
                    let hosts = parse_hosts(host_values);
                    let warning = restored
                        .get("warning")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    AppMessage::BackupRestored {
                        hosts,
                        settings,
                        warning,
                    }
                },
                |message| message,
            )
        }
        AppMessage::BackupRestored {
            hosts,
            settings,
            warning,
        } => {
            app.set_hosts(hosts);
            let mut state = s5_settings::State::from_json(&settings, &app.i18n);
            state.active_section = s5_settings::SettingsSection::Backup;
            let mut result = app.i18n.tr("backup_restore_success").to_string();
            if let Some(warning) = warning {
                result.push(' ');
                result.push_str(&warning);
            }
            state.backup.restore_result = Some(result);
            app.terminal_appearance = state.terminal_appearance();
            app.settings_state = Some(state);
            Task::none()
        }
        AppMessage::BackupRestoreFailed(message) => {
            if let Some(settings) = &mut app.settings_state {
                settings.backup.restore_validating = false;
                settings.backup.restoring = false;
                settings.backup.restore_error = Some(message);
            }
            Task::none()
        }
        // ---- Formal local terminal tabs (M2b-3) ----
        AppMessage::OpenLocalTerminal => {
            if app.terminal_opening || !matches!(app.screen, Screen::Main(_)) {
                return Task::none();
            }
            let client = match app.ws_client.as_ref() {
                Some(c) => c.clone(),
                None => return Task::none(),
            };
            app.terminal_opening = true;
            app.terminal_error = None;
            Task::perform(
                async move {
                    match open_and_subscribe(&client).await {
                        Ok(sid) => AppMessage::TerminalOpened {
                            session_id: sid,
                            title: "".to_string(),
                            host_id: None,
                        },
                        Err(e) => AppMessage::TerminalSetupError(e),
                    }
                },
                |msg| msg,
            )
        }
        AppMessage::OpenSshTerminal(host_id) => {
            if app.terminal_opening || !matches!(app.screen, Screen::Main(_)) {
                return Task::none();
            }
            let Some(host) = app.hosts.iter().find(|host| host.id == host_id) else {
                app.terminal_error = Some(app.i18n.tr("main_host_not_found").to_string());
                return Task::none();
            };
            let title = host.name.clone();
            let client = match app.ws_client.as_ref() {
                Some(client) => client.clone(),
                None => return Task::none(),
            };
            app.terminal_opening = true;
            app.terminal_error = None;
            Task::perform(
                async move {
                    match open_ssh_and_subscribe(&client, &host_id).await {
                        Ok(session_id) => AppMessage::TerminalOpened {
                            session_id,
                            title,
                            host_id: Some(host_id),
                        },
                        Err(error) => AppMessage::TerminalSetupError(error),
                    }
                },
                |message| message,
            )
        }
        AppMessage::TerminalOpened {
            session_id,
            title,
            host_id,
        } => {
            app.terminal_opening = false;
            if !matches!(app.screen, Screen::Main(_)) {
                if let Some(client) = app.ws_client.as_ref() {
                    client.unsubscribe(&session_id);
                    let _ = client.send_queued(
                        "CloseSession",
                        serde_json::json!({"session_id": session_id}),
                    );
                }
                return Task::none();
            }
            let number = app.next_terminal_number;
            app.next_terminal_number = app.next_terminal_number.saturating_add(1);
            let local_title = app.i18n.tr("terminal_local_title").to_string();
            let name = if host_id.is_some() {
                title.clone()
            } else {
                app.i18n
                    .trf("terminal_local_numbered", &[&number.to_string()])
            };
            let tab = Tab::terminal(session_id.clone(), number, name);
            let tab_id = tab.id.clone();
            app.terminal_sessions.insert(
                tab_id.clone(),
                s_terminal::TerminalSession::new(
                    session_id,
                    host_id,
                    if title.is_empty() { local_title } else { title },
                    40,
                    100,
                    app.terminal_appearance.clone(),
                ),
            );
            app.tabs.push(tab);
            app.active_tab_id = tab_id;
            app.show_connect_panel = false;
            iced::widget::operation::focus::<AppMessage>(crate::term::widget::id())
        }
        AppMessage::TerminalDisconnected { session_id } => {
            if !app
                .terminal_sessions
                .values()
                .any(|session| session.session_id == session_id && !session.closed)
            {
                return Task::none();
            }

            for session in app
                .terminal_sessions
                .values_mut()
                .filter(|session| !session.closed)
            {
                session.closed = true;
                session.notice = Some(app.i18n.tr("terminal_reconnecting").to_string());
            }
            app.terminal_restore_pending = true;

            let now = std::time::Instant::now();
            if app
                .terminal_reconnect_cooldown
                .is_some_and(|until| now < until)
            {
                return Task::none();
            }
            app.terminal_reconnect_cooldown = Some(now + std::time::Duration::from_secs(10));
            app.terminal_reconnect_in_flight = true;

            Task::perform(
                async move {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    match WsClient::connect().await {
                        Ok(client) => AppMessage::WsConnected(client),
                        Err(error) => AppMessage::TerminalReconnectFailed(error.to_string()),
                    }
                },
                |message| message,
            )
        }
        AppMessage::TerminalReconnected { client, mappings } => {
            app.ws_client = Some(client);
            app.terminal_reconnect_cooldown = None;
            app.terminal_restore_pending = false;
            app.terminal_reconnect_in_flight = false;
            app.terminal_error = None;

            for (old_session_id, new_session_id, replaced) in mappings {
                if let Some(session) = app
                    .terminal_sessions
                    .values_mut()
                    .find(|session| session.session_id == old_session_id)
                {
                    session.session_id = new_session_id.clone();
                    session.closed = false;
                    session.exit_code = None;
                    session.notice =
                        replaced.then(|| app.i18n.tr("terminal_session_replaced").to_string());
                    let (rows, cols) = (session.grid.rows, session.grid.cols);
                    session.grid.reset(rows, cols);
                    session.cursor_on = true;
                }
                for tab in &mut app.tabs {
                    if let crate::screens::TabKind::Terminal { session_id, .. } = &mut tab.kind
                        && *session_id == old_session_id
                    {
                        *session_id = new_session_id.clone();
                    }
                }
            }
            Task::none()
        }
        AppMessage::TerminalReconnectFailed(message) => {
            app.terminal_reconnect_in_flight = false;
            app.terminal_opening = false;
            let display = app.i18n.trf("terminal_reconnect_failed", &[&message]);
            app.terminal_error = Some(display.clone());
            app.screen = Screen::ConnectionFailure(s0_connection::State::new(display));
            Task::perform(
                async {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                },
                |_| AppMessage::TerminalReconnectRetry,
            )
        }
        AppMessage::TerminalSetupError(message) => {
            app.terminal_opening = false;
            app.terminal_error = Some(app.i18n.trf("terminal_open_failed", &[&message]));
            Task::none()
        }
        AppMessage::TerminalInput(data) => {
            if data.is_empty() {
                return Task::none();
            }
            let Some(client) = app.ws_client.as_ref().cloned() else {
                return Task::none();
            };
            let closed_notice = if app.terminal_restore_pending {
                app.i18n.tr("terminal_reconnecting").to_string()
            } else {
                app.i18n.tr("terminal_closed_input").to_string()
            };
            let Some(session) = active_terminal_mut(app) else {
                return Task::none();
            };
            if session.closed {
                session.notice = Some(closed_notice);
                return Task::none();
            }
            session.cursor_on = true;
            if let Err(error) = client.send_queued(
                "SessionInput",
                serde_json::json!({
                    "session_id": session.session_id,
                    "data": data,
                }),
            ) {
                session.notice = Some(error.message);
            }
            Task::none()
        }
        AppMessage::TerminalPaste(data) => {
            if data.is_empty() {
                return Task::none();
            }
            let Some(client) = app.ws_client.as_ref().cloned() else {
                return Task::none();
            };
            let closed_notice = app.i18n.tr("terminal_closed_paste").to_string();
            let Some(session) = active_terminal_mut(app) else {
                return Task::none();
            };
            if session.closed {
                session.notice = Some(closed_notice);
                return Task::none();
            }
            session.cursor_on = true;
            if let Err(error) = client.send_queued(
                "PasteSession",
                serde_json::json!({
                    "session_id": session.session_id,
                    "data": data,
                }),
            ) {
                session.notice = Some(error.message);
            }
            Task::none()
        }
        AppMessage::TerminalResize { cols, rows } => {
            let Some(client) = app.ws_client.as_ref().cloned() else {
                return Task::none();
            };
            let Some(session) = active_terminal_mut(app) else {
                return Task::none();
            };
            if session.closed || (session.grid.cols == cols && session.grid.rows == rows) {
                return Task::none();
            }

            session.grid.reset(rows, cols);
            if let Err(error) = client.send_queued(
                "ResizeSession",
                serde_json::json!({
                    "session_id": session.session_id,
                    "cols": cols,
                    "rows": rows,
                }),
            ) {
                session.notice = Some(error.message);
            }
            Task::none()
        }
        AppMessage::TerminalScroll(lines) => {
            if lines == 0 {
                return Task::none();
            }
            let Some(client) = app.ws_client.as_ref().cloned() else {
                return Task::none();
            };
            let Some(session) = active_terminal_mut(app) else {
                return Task::none();
            };
            if session.closed {
                return Task::none();
            }
            if let Err(error) = client.send_queued(
                "ScrollSession",
                serde_json::json!({
                    "session_id": session.session_id,
                    "lines": lines,
                }),
            ) {
                session.notice = Some(error.message);
            }
            Task::none()
        }
        AppMessage::TerminalPush(PushMsg::Frame { session_id, bytes }) => {
            let Some(session) = app
                .terminal_sessions
                .values_mut()
                .find(|session| session.session_id == session_id)
            else {
                return Task::none();
            };
            let first = session.grid.last_seq.is_none();
            match frame::decode_frame(&bytes) {
                Some(frame) => {
                    if first {
                        let (rows, cols) = (session.grid.rows, session.grid.cols);
                        session.grid.reset(rows, cols);
                    }
                    session.apply_frame(&frame);
                    session.cursor_on = true;
                }
                None => tracing::warn!("终端帧解码失败（丢弃）"),
            }
            Task::none()
        }
        AppMessage::TerminalCursorBlink => {
            let Some(session) = active_terminal_mut(app) else {
                return Task::none();
            };
            if session.appearance.cursor_blink && session.grid.cursor_visible && !session.closed {
                session.cursor_on = !session.cursor_on;
            } else {
                session.cursor_on = true;
            }
            Task::none()
        }
        AppMessage::TerminalPush(PushMsg::SessionClosed {
            session_id,
            exit_code,
        }) => {
            let Some(session) = app
                .terminal_sessions
                .values_mut()
                .find(|session| session.session_id == session_id)
            else {
                return Task::none();
            };
            session.closed = true;
            session.exit_code = exit_code;
            if session.remote_host_id.is_some() && exit_code.is_some_and(|code| code != 0) {
                session.notice = Some(classify_ssh_failure(&app.i18n, &session.grid.plain_text()));
            }
            match exit_code {
                Some(code) => tracing::info!("终端会话 {} 结束, exit_code={}", session_id, code),
                None => tracing::info!("终端会话 {} 结束, 退出码未知", session_id),
            }
            Task::none()
        }
    }
}

fn view(app: &VidaApp) -> Element<'_, AppMessage> {
    use crate::screens::TabKind;
    use iced::Length;
    use iced::widget::{Space, column, container, row, text};

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
                app.sync_state.symbol(),
                app.sync_state.label(&app.i18n),
            );

            let content = if let Some(active_tab) =
                app.tabs.iter().find(|t| t.id == app.active_tab_id)
            {
                match &active_tab.kind {
                    TabKind::Host { host_id } => s.view_host_detail(&app.hosts, host_id, &app.i18n),
                    TabKind::Terminal { .. } => app
                        .terminal_sessions
                        .get(&active_tab.id)
                        .map(|session| session.view(&app.i18n))
                        .unwrap_or_else(|| text(app.i18n.tr("terminal_state_missing")).into()),
                    TabKind::AddHost | TabKind::EditHost { .. } => {
                        if let Some(editor) = &app.editor_state {
                            editor.view(&app.i18n)
                        } else {
                            text(app.i18n.tr("main_editor_loading")).into()
                        }
                    }
                    TabKind::Settings => {
                        if let Some(settings) = &app.settings_state {
                            settings.view(&app.i18n, &app.hosts)
                        } else {
                            text(app.i18n.tr("main_settings_loading")).into()
                        }
                    }
                }
            } else {
                let placeholder = container(
                    column![
                        container(
                            crate::ui::icons::icon(crate::ui::icons::SERVER, 22)
                                .color(crate::ui::ACCENT),
                        )
                        .center_x(48)
                        .center_y(48)
                        .style(crate::ui::accent_badge),
                        crate::ui::muted(app.i18n.tr("main_no_hosts")).size(14),
                    ]
                    .spacing(12)
                    .align_x(iced::Alignment::Center),
                )
                .padding([22, 28])
                .style(crate::ui::surface);
                container(placeholder)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .into()
            };

            let terminal_error: Element<'_, AppMessage> = match &app.terminal_error {
                Some(message) => container(
                    row![
                        crate::ui::icons::icon(crate::ui::icons::CIRCLE_ALERT, 15)
                            .color(crate::ui::DANGER_TEXT),
                        text(message)
                            .size(12)
                            .color(crate::ui::DANGER_TEXT)
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .width(Length::Fill),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                )
                .padding([8, 12])
                .width(Length::Fill)
                .style(crate::ui::error_notice)
                .into(),
                None => container(Space::new()).height(0).into(),
            };

            let base = column![tab_bar, terminal_error, content]
                .width(Length::Fill)
                .height(Length::Fill);

            if app.show_connect_panel {
                // Floating quick-connect panel over a single dimmed overlay layer.
                //
                // Rendering note: the earlier leak (a vertical strip of the
                // underlying text at the window's left edge) came from the base
                // layer being transparent while two separate semi-transparent
                // overlay layers (overlay + panel wrapper) were stacked on top;
                // the alpha compositing of two 0.4 layers at the seams reached
                // 0.64 and left edge artifacts where layers ended.
                // Fix: exactly ONE dim overlay layer; the base gets an opaque
                // background so nothing shows through around it.
                use iced::Color;
                use iced::widget::button;
                use iced::widget::stack;

                let panel = s3_main::State::view_connect_panel(
                    &app.hosts,
                    &app.recent_host_ids,
                    &app.connect_panel_search,
                    &app.i18n,
                );

                // Opaque background on the base: nothing underneath can peek out.
                let base_el: Element<'_, AppMessage> = container(base)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(crate::ui::app_background)
                    .into();

                // Single dim overlay layer; clicking anywhere closes the panel.
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

                // Panel centered horizontally, near top with gap. Transparent
                // wrapper: clicks outside the panel fall through to the overlay.
                let panel_inner: Element<'_, AppMessage> = container(panel)
                    .padding(4)
                    .width(Length::Fixed(420.0))
                    .style(crate::ui::elevated)
                    .into();

                let panel_el: Element<'_, AppMessage> = container(panel_inner)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .clip(true)
                    .padding(iced::padding::Padding::new(0.0).top(50))
                    .center_x(Length::Fill)
                    .into();

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
            let tab_bar = s3_main::State::view_tab_bar(
                &app.tabs,
                &app.active_tab_id,
                &app.i18n,
                false,
                app.sync_state.symbol(),
                app.sync_state.label(&app.i18n),
            );
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

fn parse_backup_bytes(value: &serde_json::Value) -> Result<Vec<u8>, ()> {
    value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or(())?
        .iter()
        .map(|byte| {
            byte.as_u64()
                .filter(|value| *value <= u8::MAX as u64)
                .map(|value| value as u8)
                .ok_or(())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        AppMessage, Screen, Tab, VidaApp, classify_ssh_failure, close_all_terminal_sessions,
        editor_focus_event, parse_backup_bytes, update,
    };
    use crate::screens::{s_terminal, s3_main};
    use crate::term::primitive::TerminalAppearance;
    use crate::ws_client::PushMsg;

    fn tab_key_event(modifiers: iced::keyboard::Modifiers) -> iced::Event {
        use iced::keyboard::{Key, Location, key};

        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: Key::Named(key::Named::Tab),
            modified_key: Key::Named(key::Named::Tab),
            physical_key: key::Physical::Code(key::Code::Tab),
            location: Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn editor_tab_event_maps_forward_and_reverse_focus() {
        let forward = editor_focus_event(
            tab_key_event(iced::keyboard::Modifiers::NONE),
            iced::event::Status::Ignored,
            iced::window::Id::unique(),
        );
        let reverse = editor_focus_event(
            tab_key_event(iced::keyboard::Modifiers::SHIFT),
            iced::event::Status::Ignored,
            iced::window::Id::unique(),
        );

        assert!(matches!(forward, Some(AppMessage::EditorFocusNext)));
        assert!(matches!(reverse, Some(AppMessage::EditorFocusPrevious)));
    }

    #[test]
    fn editor_tab_event_does_not_capture_command_tab() {
        let result = editor_focus_event(
            tab_key_event(iced::keyboard::Modifiers::COMMAND),
            iced::event::Status::Ignored,
            iced::window::Id::unique(),
        );

        assert!(result.is_none());
    }

    fn app_with_two_terminal_tabs() -> VidaApp {
        let (mut app, _) = super::new();
        app.screen = Screen::Main(s3_main::State {
            revealed_credential: None,
            credential_copied: false,
        });
        for (session_id, number) in [("session-a", 1), ("session-b", 2)] {
            let tab = Tab::terminal(
                session_id.to_string(),
                number,
                format!("Local terminal {number}"),
            );
            app.terminal_sessions.insert(
                tab.id.clone(),
                s_terminal::TerminalSession::new(
                    session_id.to_string(),
                    None,
                    format!("Local terminal {number}"),
                    40,
                    100,
                    TerminalAppearance::default(),
                ),
            );
            app.tabs.push(tab);
        }
        app.active_tab_id = "terminal:session-a".to_string();
        app
    }

    #[test]
    fn backup_bytes_accept_full_byte_range() {
        let value = serde_json::json!({"data": [0, 1, 127, 128, 254, 255]});
        assert_eq!(
            parse_backup_bytes(&value),
            Ok(vec![0, 1, 127, 128, 254, 255])
        );
    }

    #[test]
    fn backup_bytes_reject_missing_or_out_of_range_values() {
        assert_eq!(parse_backup_bytes(&serde_json::json!({})), Err(()));
        assert_eq!(
            parse_backup_bytes(&serde_json::json!({"data": [256]})),
            Err(())
        );
    }

    #[test]
    fn ssh_failure_classifier_turns_exit_255_output_into_actionable_copy() {
        let i18n = vida_core::i18n::I18n::new(vida_core::i18n::Lang::ZhCn);
        assert!(
            classify_ssh_failure(&i18n, "Permission denied (publickey,password)")
                .contains("认证失败")
        );
        assert!(
            classify_ssh_failure(&i18n, "ssh: connect to host x port 22: Connection refused")
                .contains("服务器拒绝")
        );
        assert!(classify_ssh_failure(&i18n, "").contains("常见原因"));
    }

    #[test]
    fn terminal_closed_event_updates_only_matching_tab_session() {
        let mut app = app_with_two_terminal_tabs();
        let _ = update(
            &mut app,
            AppMessage::TerminalPush(PushMsg::SessionClosed {
                session_id: "session-b".to_string(),
                exit_code: Some(7),
            }),
        );

        assert!(!app.terminal_sessions["terminal:session-a"].closed);
        assert!(app.terminal_sessions["terminal:session-b"].closed);
        assert_eq!(
            app.terminal_sessions["terminal:session-b"].exit_code,
            Some(7)
        );
    }

    #[test]
    fn closing_terminal_tab_removes_only_its_session_and_selects_neighbor() {
        let mut app = app_with_two_terminal_tabs();
        let _ = update(
            &mut app,
            AppMessage::CloseTab("terminal:session-a".to_string()),
        );

        assert!(!app.terminal_sessions.contains_key("terminal:session-a"));
        assert!(app.terminal_sessions.contains_key("terminal:session-b"));
        assert_eq!(app.active_tab_id, "terminal:session-b");
    }

    #[test]
    fn daemon_disconnect_preserves_terminal_tabs_for_unlock_restore() {
        let mut app = app_with_two_terminal_tabs();
        let terminal_tab_ids: Vec<String> = app
            .tabs
            .iter()
            .filter(|tab| matches!(tab.kind, crate::screens::TabKind::Terminal { .. }))
            .map(|tab| tab.id.clone())
            .collect();

        let _ = update(
            &mut app,
            AppMessage::TerminalDisconnected {
                session_id: "session-a".to_string(),
            },
        );

        assert!(app.terminal_restore_pending);
        assert!(app.terminal_sessions.values().all(|session| session.closed));
        assert!(
            terminal_tab_ids
                .iter()
                .all(|id| app.tabs.iter().any(|tab| &tab.id == id))
        );

        let _ = update(&mut app, AppMessage::LockVault);
        assert!(app.terminal_restore_pending);
        assert_eq!(app.terminal_sessions.len(), 2);
        assert!(
            terminal_tab_ids
                .iter()
                .all(|id| app.tabs.iter().any(|tab| &tab.id == id))
        );

        close_all_terminal_sessions(&mut app);
        assert!(!app.terminal_restore_pending);
        assert!(app.terminal_sessions.is_empty());
    }
}
