//! Daemon-owned Agent command gate, approval state, and audit trail.
//!
//! This module is the only path allowed to turn an Agent command into PTY
//! bytes. Human `SessionInput` deliberately remains outside it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use vida_core::agent_policy::{AgentTrust, DangerMatch, analyze_command, redact_for_audit};

pub const APPROVAL_TTL_SECONDS: i64 = 120;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionTarget {
    Local,
    Host { host_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub session_id: String,
    pub host_id: Option<String>,
    pub command: String,
    pub reasons: Vec<String>,
    pub matched_rules: Vec<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Allowed,
    NeedsApproval,
    Approved,
    Denied,
    Expired,
    RejectedReadonly,
    RejectedUnknownSession,
    Failed,
    Cancelled,
}

/// Owner-facing lifecycle event. The full command is included only in the
/// in-memory requested event; persistent audit entries remain fingerprints.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    ApprovalRequested {
        approval: PendingApproval,
    },
    ApprovalResolved {
        approval_id: String,
        status: String,
    },
    SessionOpened {
        session_id: String,
        host_id: String,
        host_name: String,
        cols: u16,
        rows: u16,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: i64,
    pub session_id: String,
    pub host_id: Option<String>,
    pub tool: String,
    /// Command text is never persisted; this contains only length + SHA-256.
    pub input: String,
    pub matched_rules: Vec<String>,
    pub approval_id: Option<String>,
    pub outcome: AuditOutcome,
    pub result_summary: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CommandDecision {
    Allow { matches: Vec<DangerMatch> },
    NeedsApproval(PendingApproval),
    RejectReadonly,
    RejectUnknownSession,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedBindings {
    bindings: HashMap<String, SessionTarget>,
}

pub struct AgentController {
    bindings: HashMap<String, SessionTarget>,
    pending: HashMap<String, PendingApproval>,
    bindings_path: PathBuf,
    audit_path: PathBuf,
    events: tokio::sync::broadcast::Sender<AgentEvent>,
}

impl AgentController {
    pub fn load() -> Result<Self> {
        let config_dir = vida_core::config::config_dir()?;
        Self::load_at(&config_dir, true)
    }

    pub(crate) fn load_at(config_dir: &Path, include_ui_state: bool) -> Result<Self> {
        let bindings_path = config_dir.join("agent-sessions.json");
        let audit_path = config_dir.join("audit.jsonl");
        let mut bindings = std::fs::read(&bindings_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PersistedBindings>(&bytes).ok())
            .unwrap_or_default()
            .bindings;

        // The GUI state is also device-local and lets an upgrade safely map
        // already-open sessions. Unknown sessions remain denied.
        if include_ui_state {
            for entry in vida_core::config::load_ui_state().terminal_layout {
                bindings.entry(entry.session_id).or_insert_with(|| {
                    entry
                        .host_id
                        .map(|host_id| SessionTarget::Host { host_id })
                        .unwrap_or(SessionTarget::Local)
                });
            }
        }

        let (events, _) = tokio::sync::broadcast::channel(64);
        Ok(Self {
            bindings,
            pending: HashMap::new(),
            bindings_path,
            audit_path,
            events,
        })
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    pub fn register_session(&mut self, session_id: &str, target: SessionTarget) -> Result<()> {
        self.bindings.insert(session_id.to_string(), target);
        self.save_bindings()
    }

    pub fn remove_session(&mut self, session_id: &str) -> Result<()> {
        self.bindings.remove(session_id);
        let removed: Vec<_> = self
            .pending
            .values()
            .filter(|item| item.session_id == session_id)
            .cloned()
            .collect();
        for item in removed {
            self.pending.remove(&item.approval_id);
            self.append_audit(AuditEntry::from_pending(
                &item,
                AuditOutcome::Cancelled,
                Some("terminal session closed before approval".into()),
            ))?;
            self.notify_resolved(&item.approval_id, "cancelled");
        }
        self.save_bindings()
    }

    pub fn target(&self, session_id: &str) -> Option<&SessionTarget> {
        self.bindings.get(session_id)
    }

    pub fn evaluate(
        &mut self,
        session_id: &str,
        command: &str,
        trust: AgentTrust,
        now: i64,
    ) -> Result<CommandDecision> {
        self.expire_pending(now)?;
        let Some(target) = self.bindings.get(session_id).cloned() else {
            self.append_audit(AuditEntry::new(
                session_id,
                None,
                command,
                vec![],
                None,
                AuditOutcome::RejectedUnknownSession,
                Some("session is not registered for Agent control".into()),
            ))?;
            return Ok(CommandDecision::RejectUnknownSession);
        };
        let host_id = match target {
            SessionTarget::Local => None,
            SessionTarget::Host { host_id } => Some(host_id),
        };
        let matches = analyze_command(command);

        if trust == AgentTrust::Readonly {
            self.append_audit(AuditEntry::new(
                session_id,
                host_id,
                command,
                &matches,
                None,
                AuditOutcome::RejectedReadonly,
                Some("host trust is readonly".into()),
            ))?;
            return Ok(CommandDecision::RejectReadonly);
        }

        if trust == AgentTrust::Ask && !matches.is_empty() {
            let approval = PendingApproval {
                approval_id: uuid::Uuid::new_v4().to_string(),
                session_id: session_id.to_string(),
                host_id: host_id.clone(),
                command: command.to_string(),
                reasons: matches.iter().map(|item| item.reason.to_string()).collect(),
                matched_rules: matches
                    .iter()
                    .map(|item| item.rule_id.to_string())
                    .collect(),
                created_at: now,
                expires_at: now + APPROVAL_TTL_SECONDS,
            };
            self.append_audit(AuditEntry::new(
                session_id,
                host_id,
                command,
                &matches,
                Some(approval.approval_id.clone()),
                AuditOutcome::NeedsApproval,
                None,
            ))?;
            self.pending
                .insert(approval.approval_id.clone(), approval.clone());
            let _ = self.events.send(AgentEvent::ApprovalRequested {
                approval: approval.clone(),
            });
            return Ok(CommandDecision::NeedsApproval(approval));
        }

        Ok(CommandDecision::Allow { matches })
    }

    pub fn record_allowed(
        &self,
        session_id: &str,
        command: &str,
        matches: &[DangerMatch],
    ) -> Result<()> {
        let host_id = self.target_host_id(session_id);
        self.append_audit(AuditEntry::new(
            session_id,
            host_id,
            command,
            matches,
            None,
            AuditOutcome::Allowed,
            Some("policy allowed; terminal dispatch attempted".into()),
        ))
    }

    pub fn record_dispatch_failed(
        &self,
        session_id: &str,
        command: &str,
        matches: &[DangerMatch],
        approval_id: Option<String>,
        message: &str,
    ) -> Result<()> {
        let host_id = self.target_host_id(session_id);
        self.append_audit(AuditEntry::new(
            session_id,
            host_id,
            command,
            matches,
            approval_id,
            AuditOutcome::Failed,
            Some(message.to_string()),
        ))
    }

    /// Persist the Agent-created connection before notifying owner clients.
    /// Host IDs are already owner-visible metadata; credentials never enter
    /// this record or the event stream.
    pub fn record_session_open(&self, session_id: &str, host_id: &str, reused: bool) -> Result<()> {
        self.append_audit(AuditEntry {
            timestamp: chrono::Utc::now().timestamp(),
            session_id: session_id.to_string(),
            host_id: Some(host_id.to_string()),
            tool: "session_open".into(),
            input: "[configured host selected by id]".into(),
            matched_rules: Vec::new(),
            approval_id: None,
            outcome: AuditOutcome::Allowed,
            result_summary: Some(if reused {
                "existing alive SSH terminal reused".into()
            } else {
                "SSH terminal opened; owner notification emitted".into()
            }),
        })
    }

    pub fn notify_session_opened(
        &self,
        session_id: &str,
        host_id: &str,
        host_name: &str,
        cols: u16,
        rows: u16,
    ) {
        let _ = self.events.send(AgentEvent::SessionOpened {
            session_id: session_id.to_string(),
            host_id: host_id.to_string(),
            host_name: host_name.to_string(),
            cols,
            rows,
        });
    }

    pub fn take_for_approval(&mut self, approval_id: &str, now: i64) -> Result<PendingApproval> {
        self.expire_pending(now)?;
        self.pending
            .remove(approval_id)
            .with_context(|| format!("approval not found or expired: {approval_id}"))
    }

    pub fn deny(&mut self, approval_id: &str, now: i64) -> Result<PendingApproval> {
        let item = self.take_for_approval(approval_id, now)?;
        self.append_audit(AuditEntry::from_pending(
            &item,
            AuditOutcome::Denied,
            Some("owner denied the command".into()),
        ))?;
        self.notify_resolved(&item.approval_id, "denied");
        Ok(item)
    }

    pub fn record_approved(&self, item: &PendingApproval) -> Result<()> {
        self.append_audit(AuditEntry::from_pending(
            item,
            AuditOutcome::Approved,
            Some("owner approved; terminal dispatch attempted".into()),
        ))
    }

    pub fn resolve(&self, approval_id: &str, status: &str) {
        self.notify_resolved(approval_id, status);
    }

    pub fn pending(&mut self, now: i64) -> Result<Vec<PendingApproval>> {
        self.expire_pending(now)?;
        let mut items: Vec<_> = self.pending.values().cloned().collect();
        items.sort_by_key(|item| item.created_at);
        Ok(items)
    }

    pub fn read_audit(&self, limit: usize, host_id: Option<&str>) -> Result<Vec<AuditEntry>> {
        read_audit_file(&self.audit_path, limit, host_id)
    }

    fn target_host_id(&self, session_id: &str) -> Option<String> {
        match self.bindings.get(session_id) {
            Some(SessionTarget::Host { host_id }) => Some(host_id.clone()),
            _ => None,
        }
    }

    fn expire_pending(&mut self, now: i64) -> Result<()> {
        let expired: Vec<_> = self
            .pending
            .values()
            .filter(|item| item.expires_at <= now)
            .cloned()
            .collect();
        for item in expired {
            self.pending.remove(&item.approval_id);
            self.append_audit(AuditEntry::from_pending(
                &item,
                AuditOutcome::Expired,
                Some("approval timed out".into()),
            ))?;
            self.notify_resolved(&item.approval_id, "expired");
        }
        Ok(())
    }

    fn notify_resolved(&self, approval_id: &str, status: &str) {
        let _ = self.events.send(AgentEvent::ApprovalResolved {
            approval_id: approval_id.to_string(),
            status: status.to_string(),
        });
    }

    fn save_bindings(&self) -> Result<()> {
        if let Some(parent) = self.bindings_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.bindings_path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(&PersistedBindings {
                bindings: self.bindings.clone(),
            })?,
        )?;
        std::fs::rename(tmp, &self.bindings_path)?;
        Ok(())
    }

    fn append_audit(&self, entry: AuditEntry) -> Result<()> {
        append_audit_file(&self.audit_path, &entry)
    }
}

impl AuditEntry {
    fn new(
        session_id: &str,
        host_id: Option<String>,
        command: &str,
        matches: impl AsRef<[DangerMatch]>,
        approval_id: Option<String>,
        outcome: AuditOutcome,
        result_summary: Option<String>,
    ) -> Self {
        Self {
            timestamp: chrono::Utc::now().timestamp(),
            session_id: session_id.to_string(),
            host_id,
            tool: "exec".into(),
            input: redact_for_audit(command),
            matched_rules: matches
                .as_ref()
                .iter()
                .map(|item| item.rule_id.to_string())
                .collect(),
            approval_id,
            outcome,
            result_summary,
        }
    }

    fn from_pending(
        item: &PendingApproval,
        outcome: AuditOutcome,
        result_summary: Option<String>,
    ) -> Self {
        Self {
            timestamp: chrono::Utc::now().timestamp(),
            session_id: item.session_id.clone(),
            host_id: item.host_id.clone(),
            tool: "exec".into(),
            input: redact_for_audit(&item.command),
            matched_rules: item.matched_rules.clone(),
            approval_id: Some(item.approval_id.clone()),
            outcome,
            result_summary,
        }
    }
}

fn append_audit_file(path: &Path, entry: &AuditEntry) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(&mut file, entry)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn read_audit_file(path: &Path, limit: usize, host_id: Option<&str>) -> Result<Vec<AuditEntry>> {
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(vec![]);
    };
    let mut entries = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let entry: AuditEntry = serde_json::from_str(&line?)?;
        if host_id.is_none_or(|expected| entry.host_id.as_deref() == Some(expected)) {
            entries.push(entry);
        }
    }
    let keep = limit.max(1);
    if entries.len() > keep {
        entries.drain(0..entries.len() - keep);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(temp: &tempfile::TempDir) -> AgentController {
        AgentController::load_at(temp.path(), false).unwrap()
    }

    #[test]
    fn unknown_session_is_denied_and_audited() {
        let temp = tempfile::tempdir().unwrap();
        let mut controller = controller(&temp);
        assert!(matches!(
            controller
                .evaluate("missing", "echo hello", AgentTrust::Trusted, 10)
                .unwrap(),
            CommandDecision::RejectUnknownSession
        ));
        assert_eq!(controller.read_audit(10, None).unwrap().len(), 1);
    }

    #[test]
    fn ask_requires_approval_only_for_dangerous_commands() {
        let temp = tempfile::tempdir().unwrap();
        let mut controller = controller(&temp);
        controller
            .register_session("session", SessionTarget::Local)
            .unwrap();
        assert!(matches!(
            controller
                .evaluate("session", "df -h", AgentTrust::Ask, 10)
                .unwrap(),
            CommandDecision::Allow { .. }
        ));
        let decision = controller
            .evaluate("session", "rm -rf /tmp/test", AgentTrust::Ask, 10)
            .unwrap();
        let CommandDecision::NeedsApproval(item) = decision else {
            panic!("expected approval");
        };
        assert_eq!(item.expires_at, 10 + APPROVAL_TTL_SECONDS);
        assert_eq!(controller.pending(10).unwrap().len(), 1);
    }

    #[test]
    fn owner_events_cover_request_and_resolution_without_persisting_command() {
        let temp = tempfile::tempdir().unwrap();
        let mut controller = controller(&temp);
        controller
            .register_session("session", SessionTarget::Local)
            .unwrap();
        let mut events = controller.subscribe_events();

        let CommandDecision::NeedsApproval(item) = controller
            .evaluate("session", "rm -rf /tmp/approval-event", AgentTrust::Ask, 10)
            .unwrap()
        else {
            panic!("expected approval");
        };
        let AgentEvent::ApprovalRequested { approval } = events.try_recv().unwrap() else {
            panic!("expected requested event");
        };
        assert_eq!(approval.approval_id, item.approval_id);
        assert_eq!(approval.command, "rm -rf /tmp/approval-event");

        controller.deny(&item.approval_id, 11).unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentEvent::ApprovalResolved { approval_id, status }
                if approval_id == item.approval_id && status == "denied"
        ));

        let audit = std::fs::read_to_string(temp.path().join("audit.jsonl")).unwrap();
        assert!(!audit.contains("approval-event"));
    }

    #[test]
    fn readonly_rejects_even_safe_command_and_trusted_allows_dangerous() {
        let temp = tempfile::tempdir().unwrap();
        let mut controller = controller(&temp);
        controller
            .register_session(
                "session",
                SessionTarget::Host {
                    host_id: "host-1".into(),
                },
            )
            .unwrap();
        assert!(matches!(
            controller
                .evaluate("session", "df -h", AgentTrust::Readonly, 10)
                .unwrap(),
            CommandDecision::RejectReadonly
        ));
        assert!(matches!(
            controller
                .evaluate("session", "reboot", AgentTrust::Trusted, 10)
                .unwrap(),
            CommandDecision::Allow { .. }
        ));
    }

    #[test]
    fn expired_approval_cannot_be_taken_and_is_audited() {
        let temp = tempfile::tempdir().unwrap();
        let mut controller = controller(&temp);
        controller
            .register_session("session", SessionTarget::Local)
            .unwrap();
        let CommandDecision::NeedsApproval(item) = controller
            .evaluate("session", "reboot", AgentTrust::Ask, 10)
            .unwrap()
        else {
            panic!("expected approval");
        };
        assert!(
            controller
                .take_for_approval(&item.approval_id, 131)
                .is_err()
        );
        assert!(
            controller
                .read_audit(10, None)
                .unwrap()
                .iter()
                .any(|entry| entry.outcome == AuditOutcome::Expired)
        );
    }

    #[test]
    fn persisted_audit_never_contains_command_plaintext() {
        let temp = tempfile::tempdir().unwrap();
        let mut controller = controller(&temp);
        controller
            .register_session("session", SessionTarget::Local)
            .unwrap();
        let command = "echo arbitrary-top-secret-value";
        let CommandDecision::Allow { matches } = controller
            .evaluate("session", command, AgentTrust::Ask, 10)
            .unwrap()
        else {
            panic!("safe command should be allowed");
        };
        controller
            .record_allowed("session", command, &matches)
            .unwrap();

        let raw = std::fs::read_to_string(temp.path().join("audit.jsonl")).unwrap();
        assert!(!raw.contains(command));
        assert!(!raw.contains("arbitrary-top-secret-value"));
        assert!(raw.contains("sha256="));
    }
}
