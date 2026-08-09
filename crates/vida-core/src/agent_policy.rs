//! Agent-only command policy.
//!
//! Human terminal input never enters this module. This is a guardrail for the
//! product's Agent protocol, not a shell sandbox: shells can transform input
//! in ways no regex policy can prove safe.

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTrust {
    Readonly,
    #[default]
    Ask,
    Trusted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DangerMatch {
    pub rule_id: &'static str,
    pub reason: &'static str,
}

struct DangerRule {
    id: &'static str,
    reason: &'static str,
    regex: Regex,
}

static RULES: LazyLock<Vec<DangerRule>> = LazyLock::new(|| {
    [
        (
            "recursive_delete",
            "recursive or forced file deletion",
            r"(?i)(^|[;&|]\s*)rm\s+-[^\s;|&]*[rf][^\s;|&]*",
        ),
        (
            "raw_disk_write",
            "raw disk write",
            r"(?i)(^|[;&|]\s*)dd(\s|$)|>\s*/dev/(sd|disk|nvme)",
        ),
        (
            "format_filesystem",
            "filesystem formatting",
            r"(?i)(^|[;&|]\s*)mkfs([.\s]|$)",
        ),
        (
            "power_control",
            "server shutdown or restart",
            r"(?i)(^|[;&|]\s*)(shutdown|reboot|halt|poweroff)(\s|$)",
        ),
        (
            "delete_user",
            "user deletion",
            r"(?i)(^|[;&|]\s*)userdel(\s|$)",
        ),
        (
            "flush_firewall",
            "firewall rules flush",
            r"(?i)(^|[;&|]\s*)iptables\s+(-[^\s]+\s+)*-F(\s|$)",
        ),
        (
            "disable_firewall",
            "firewall disable",
            r"(?i)(^|[;&|]\s*)ufw\s+disable(\s|$)",
        ),
        (
            "disable_service",
            "service stop or disable",
            r"(?i)(^|[;&|]\s*)systemctl\s+(stop|disable)(\s|$)",
        ),
        (
            "delete_cluster_resource",
            "Kubernetes resource deletion",
            r"(?i)(^|[;&|]\s*)kubectl\s+delete(\s|$)",
        ),
        (
            "docker_prune",
            "Docker system-wide pruning",
            r"(?i)(^|[;&|]\s*)docker\s+system\s+prune(\s|$)",
        ),
        (
            "drop_database",
            "database or table deletion",
            r"(?i)\bDROP\s+(TABLE|DATABASE)\b",
        ),
        (
            "root_world_writable",
            "world-writable root permissions",
            r"(?i)(^|[;&|]\s*)chmod\s+(-[^\s]+\s+)*777\s+/(\s|$)",
        ),
        (
            "fork_bomb",
            "shell fork bomb",
            r":\s*\(\s*\)\s*\{[^}]*:\s*\|\s*:\s*&[^}]*\}",
        ),
        (
            "account_file_write",
            "write to a system account database",
            r"(?i)(>|\btee\b|\bsed\s+-i\b|\b(cp|mv)\b).*(/etc/(passwd|shadow|sudoers)(\s|$))",
        ),
    ]
    .into_iter()
    .map(|(id, reason, pattern)| DangerRule {
        id,
        reason,
        regex: Regex::new(pattern).expect("built-in danger rule must compile"),
    })
    .collect()
});

/// Return every matching built-in rule. Backslash-escaped command names such
/// as `r\\m` are normalized because the shell resolves them to the same word.
pub fn analyze_command(command: &str) -> Vec<DangerMatch> {
    static PRIVILEGE_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(^|[;&|]\s*)(sudo|doas)(\s+-[^\s]+)*\s+").expect("privilege prefix regex")
    });
    let unescaped = command.replace('\\', "");
    let normalized = PRIVILEGE_PREFIX.replace_all(&unescaped, "$1");
    RULES
        .iter()
        .filter(|rule| rule.regex.is_match(&normalized))
        .map(|rule| DangerMatch {
            rule_id: rule.id,
            reason: rule.reason,
        })
        .collect()
}

/// Build a non-reversible audit reference without persisting command text.
///
/// Agent commands may contain secrets in arbitrary shapes that a redaction
/// regex cannot recognize. The original command therefore remains only in
/// memory while pending approval; the audit file gets its byte length and a
/// SHA-256 fingerprint for correlation.
pub fn redact_for_audit(command: &str) -> String {
    let digest = Sha256::digest(command.as_bytes());
    format!(
        "[command omitted; bytes={}; sha256={}]",
        command.len(),
        hex::encode(digest)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_trust_is_ask() {
        assert_eq!(AgentTrust::default(), AgentTrust::Ask);
    }

    #[test]
    fn dangerous_command_table_covers_spacing_and_flag_order() {
        for command in [
            "rm -rf /tmp/test",
            "rm   -fr /tmp/test",
            r"r\m -rf /tmp/test",
            "sudo reboot",
            "docker system prune -af",
            "DROP   DATABASE production",
            "echo bad | tee /etc/sudoers",
        ] {
            assert!(!analyze_command(command).is_empty(), "missed: {command}");
        }
    }

    #[test]
    fn ordinary_read_commands_are_not_flagged() {
        for command in [
            "df -h",
            "docker ps",
            "cat /etc/passwd",
            "systemctl status ssh",
        ] {
            assert!(
                analyze_command(command).is_empty(),
                "false positive: {command}"
            );
        }
    }

    #[test]
    fn audit_reference_never_contains_command_text() {
        let command = "PASSWORD=hunter2 curl --token abc arbitrary-secret-value";
        let value = redact_for_audit(command);
        assert!(!value.contains("hunter2"));
        assert!(!value.contains("abc"));
        assert!(!value.contains("arbitrary-secret-value"));
        assert!(value.contains(&format!("bytes={}", command.len())));
        assert!(value.contains("sha256="));
    }
}
