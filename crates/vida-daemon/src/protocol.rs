use serde::{Deserialize, Serialize};
use vida_core::sync::ConflictFile;
use vida_core::vault::Settings;

// ---------------------------------------------------------------------------
// Request — GUI → Daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    // Auth
    /// First message after WebSocket connect. Daemon rejects if token mismatches.
    Auth { token: String },

    // Vault
    /// Create a new vault with the given passphrase.
    CreateVault { passphrase: String },
    /// Unlock an existing vault.
    /// `remember`: if true, cache passphrase in OS keyring for next unlock.
    Unlock {
        passphrase: String,
        #[serde(default)]
        remember: bool,
    },
    /// Lock the vault (clear in-memory state).
    Lock,
    /// Get vault status (locked/unlocked, host count).
    VaultStatus,
    /// Export vault backup (returns encrypted age bytes).
    /// passphrase=None → use current unlock passphrase.
    ExportBackup {
        #[serde(default)]
        passphrase: Option<String>,
    },

    // Settings
    /// Get current vault settings.
    GetSettings,
    /// Update vault settings (sync path, scrollback, etc.).
    UpdateSettings { settings: Settings },

    // Hosts
    /// List all hosts (returns summary, no credentials).
    ListHosts,
    /// Reveal a host's credential (returns actual password/key).
    RevealCredential { host_id: String },
    /// Add or update a host. id=None → create, id=Some → update.
    /// password=None on update → keep existing credential.
    UpdateHost { host: HostRequest },
    /// Delete a host by id.
    DeleteHost { host_id: String },

    // Sync
    /// Trigger a sync cycle.
    Sync,
    /// Resolve a conflict by choosing "local" or "remote".
    ResolveConflict { choice: ConflictChoice },

    // Conflict files (cloud-service conflicts like Dropbox)
    /// Read a conflict file and return its decrypted host summaries.
    ReadConflictFile { path: String },
    /// Adopt a conflict file: backup current vault → replace with conflict file content.
    AdoptConflictFile { path: String },
    /// Ignore a conflict file: rename to .reviewed suffix.
    IgnoreConflictFile { path: String },

    // Remote missing
    /// Handle RemoteMissing: "reupload" or "clear_state".
    HandleRemoteMissing { action: String },
}

// ---------------------------------------------------------------------------
// Response — Daemon → GUI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub payload: ResponsePayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsePayload {
    Ok { result: serde_json::Value },
    Error { code: i32, message: String },
    /// Push events (no id correlation) — used for vault-changed notifications.
    Event { event: String, data: serde_json::Value },
}

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRequest {
    pub id: Option<String>,
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub tags: Vec<String>,
    pub group: Option<String>,
    pub color: Option<String>,
    /// Only sent when the user explicitly reveals or changes the credential.
    /// None = keep existing (update only). Some = replace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConflictChoice {
    #[serde(rename = "local")]
    Local,
    #[serde(rename = "remote")]
    Remote,
}

/// Vault status returned to GUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultStatusInfo {
    pub locked: bool,
    pub host_count: usize,
    pub revision: u64,
    pub device_id: String,
    pub vault_exists: bool,
}

/// Host summary returned to GUI (no credentials).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSummary {
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

/// Sync response — includes host list when vault may have changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_meta: Option<serde_json::Value>,
    /// Present when sync changed the vault (Downloaded, Conflict-resolved).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<HostSummary>>,
    /// Present when ConflictFilesDetected — conflict files with path + pattern.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<ConflictFile>>,
    /// Present when ConflictFilesDetected — decoded hosts from conflict files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_hosts: Option<Vec<HostSummary>>,
}
