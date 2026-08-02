pub mod s0_connection;
pub mod s1_setup;
pub mod s2_unlock;
pub mod s3_main;
pub mod s4_credential;
pub mod s5_settings;
pub mod s6_conflict;
pub mod s7_conflict_file;
pub mod s8_remote_missing;
pub mod s9_backup;

use iced::widget::text;
use iced::Element;
use vida_core::i18n::I18n;

use crate::app::AppMessage;

// ---------------------------------------------------------------------------
// Tab system (Tabby-style)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum TabKind {
    Host { host_id: String },
    AddHost,
    EditHost { host_id: String },
    Settings,
}

#[derive(Debug, Clone)]
pub struct Tab {
    pub id: String,
    pub name: String,
    pub kind: TabKind,
}

impl Tab {
    pub fn host(host_id: String, name: String) -> Self {
        Self { id: host_id.clone(), name, kind: TabKind::Host { host_id } }
    }

    pub fn add_host(name: String) -> Self {
        Self { id: "add_host".into(), name, kind: TabKind::AddHost }
    }

    pub fn edit_host(host_id: String, name: String) -> Self {
        Self { id: format!("edit_{}", host_id), name, kind: TabKind::EditHost { host_id } }
    }

    pub fn settings(name: String) -> Self {
        Self { id: "settings".into(), name, kind: TabKind::Settings }
    }
}

// ---------------------------------------------------------------------------
// Screen enum (S0-S2 are pre-main overlays, S3+ are in-main screens)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Screen {
    ConnectionFailure(s0_connection::State),
    Setup(s1_setup::State),
    Unlock(s2_unlock::State),
    Main(s3_main::State),
    // Sync conflict screens: revived when a sync trigger entry is added back
    #[allow(dead_code)]
    Conflict(s6_conflict::State),
    #[allow(dead_code)]
    ConflictFile(s7_conflict_file::State),
    #[allow(dead_code)]
    RemoteMissing(s8_remote_missing::State),
    Backup(s9_backup::State),
}

impl Screen {
    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        match self {
            Screen::ConnectionFailure(s) => s.view(i18n),
            Screen::Setup(s) => s.view(i18n),
            Screen::Unlock(s) => s.view(i18n),
            Screen::Main(_) => text("").into(), // handled by app::view with tab bar
            Screen::Conflict(s) => s.view(i18n),
            Screen::ConflictFile(s) => s.view(i18n),
            Screen::RemoteMissing(s) => s.view(i18n),
            Screen::Backup(s) => s.view(i18n),
        }
    }
}
