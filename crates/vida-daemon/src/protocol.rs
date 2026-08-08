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
    /// Export vault backup (response contains the encrypted age byte array).
    /// passphrase=None → use current unlock passphrase.
    ExportBackup {
        #[serde(default)]
        passphrase: Option<String>,
    },
    /// Validate a backup without changing the current vault.
    PreviewBackup { data: Vec<u8>, passphrase: String },
    /// Replace the current vault with a validated encrypted backup.
    RestoreBackup { data: Vec<u8>, passphrase: String },

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

    // PTY sessions (M2a-2)
    // untagged: 尝试将 JSON 直接反序列化为 PtyRequest（内部
    // tag="method" 匹配）。wire format 不变：
    // {"method":"OpenLocalSession","params":{...}}
    #[serde(untagged)]
    Pty(PtyRequest),
}

/// PTY 会话相关请求。与 Request 分开使编译器能强制穷尽匹配——
/// 新增 PtyRequest 变体时若 handle_pty_request 漏加分支，编译失败。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum PtyRequest {
    /// Open a local shell session. Fixed $SHELL, cwd=HOME.
    OpenLocalSession { cols: u16, rows: u16 },
    /// Send raw bytes to a session (no line/byte conversion).
    SessionInput { session_id: String, data: Vec<u8> },
    /// Paste text, honoring the terminal's bracketed-paste mode.
    PasteSession { session_id: String, data: Vec<u8> },
    /// Resize a session (both PTY ioctl and Term).
    ResizeSession {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    /// Scroll the terminal's real daemon-owned history. Positive lines move up.
    ScrollSession { session_id: String, lines: i32 },
    /// Close a session (kill child + wait).
    CloseSession { session_id: String },
    /// List all sessions.
    ListSessions,
    /// Read current screen as plain text snapshot.
    ReadScreen { session_id: String },
    /// Read current screen with ANSI styling (colors/attributes).
    ReadScreenStyled { session_id: String },
    /// Subscribe to session push: immediate full snapshot, then deltas.
    SubscribeSession { session_id: String },
    /// Unsubscribe from session push (drop the receiver).
    UnsubscribeSession { session_id: String },
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
    Ok {
        result: serde_json::Value,
    },
    Error {
        code: i32,
        message: String,
        /// Error category for GUI to display localized message.
        #[serde(skip_serializing_if = "Option::is_none")]
        category: Option<String>,
    },
    /// Push events (no id correlation) — used for vault-changed notifications.
    Event {
        event: String,
        data: serde_json::Value,
    },
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 必改 1：PTY 请求使用 untagged 序列化，wire format 不变。
    /// 客户端发送的 JSON 与重构前完全一致。
    #[test]
    fn pty_request_wire_format_unchanged() {
        // OpenLocalSession
        let json = r#"{"method":"OpenLocalSession","params":{"cols":80,"rows":24}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Pty(PtyRequest::OpenLocalSession { cols, rows }) => {
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
            }
            other => panic!("expected Pty(OpenLocalSession), got {:?}", other),
        }
        // 序列化回 JSON 应与输入一致（method 字段名不变）
        let back = serde_json::to_string(&req).unwrap();
        assert!(
            back.contains("\"method\":\"OpenLocalSession\""),
            "serialized: {}",
            back
        );
        assert!(back.contains("\"cols\":80"), "serialized: {}", back);

        // SessionInput（data 是字节数组）
        let json =
            r#"{"method":"SessionInput","params":{"session_id":"abc","data":[101,99,104,111]}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Pty(PtyRequest::SessionInput { session_id, data }) => {
                assert_eq!(session_id, "abc");
                assert_eq!(data, b"echo");
            }
            other => panic!("expected Pty(SessionInput), got {:?}", other),
        }

        // PasteSession 与 SessionInput 分流，daemon 才能查询真实 TermMode。
        let json = r#"{"method":"PasteSession","params":{"session_id":"abc","data":[97,10,98]}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Pty(PtyRequest::PasteSession { session_id, data }) => {
                assert_eq!(session_id, "abc");
                assert_eq!(data, b"a\nb");
            }
            other => panic!("expected Pty(PasteSession), got {:?}", other),
        }

        let json = r#"{"method":"ScrollSession","params":{"session_id":"abc","lines":-12}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Pty(PtyRequest::ScrollSession { session_id, lines }) => {
                assert_eq!(session_id, "abc");
                assert_eq!(lines, -12);
            }
            other => panic!("expected Pty(ScrollSession), got {:?}", other),
        }

        // 非 PTY 请求不受影响
        let json = r#"{"method":"Auth","params":{"token":"xyz"}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Auth { token } => assert_eq!(token, "xyz"),
            other => panic!("expected Auth, got {:?}", other),
        }
    }
}
