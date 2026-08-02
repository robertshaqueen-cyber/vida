use age::secrecy::SecretString;
use age::{Decryptor, Encryptor};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::iter;
use std::path::Path;
use zeroize::{Zeroize, Zeroizing};

/// Production work factor for age scrypt. DO NOT change without
/// re-evaluating decrypt time and memory budget.
pub const PRODUCTION_LOG_N: u8 = 18;

/// Current vault format version. Bump when the struct layout changes.
/// When bumping, MUST also add migration function and test (see AGENTS.md).
pub const CURRENT_VAULT_VERSION: u32 = 4;

/// A string wrapper that zeroizes its contents on drop.
/// Used for sensitive data (private keys, passphrases, secrets)
/// that must not linger in memory after use.
///
/// Debug and Display always output "[REDACTED]" to prevent leaks.
/// Never log the real content.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecureString(String);

impl SecureString {
    pub fn new(s: String) -> Self {
        Self(s)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecureString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecureString([REDACTED])")
    }
}

impl std::fmt::Display for SecureString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED]")
    }
}

impl PartialEq for SecureString {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Drop for SecureString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Encrypted vault file format (age-compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vault {
    pub version: u32,
    /// Monotonically increasing revision. Incremented on every save.
    /// Used for sync conflict detection (not the same as version).
    pub revision: u64,
    /// UUID of the device that last modified this vault.
    pub device_id: String,
    /// Unix timestamp of last modification.
    pub modified_at: i64,
    pub hosts: Vec<HostEntry>,
    pub settings: Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostEntry {
    /// Stable UUID. Created once, never changes. Used for IPC addressing.
    pub id: String,
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub tags: Vec<String>,
    pub group: Option<String>,
    pub color: Option<String>,
    pub auth: AuthMethod,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthMethod {
    Password {
        password: SecureString,
    },
    Key {
        private_key_path: String,
        passphrase: Option<SecureString>,
    },
    KeyInline {
        private_key: SecureString,
        passphrase: Option<SecureString>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub s3_endpoint: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<SecureString>,
    /// Local folder path for LocalPath sync backend.
    /// `None` = sync disabled. `Some(path)` = sync via this folder.
    pub sync_local_path: Option<String>,
    pub scrollback_lines: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            s3_endpoint: None,
            s3_bucket: None,
            s3_access_key: None,
            s3_secret_key: None,
            sync_local_path: None,
            scrollback_lines: 5000,
        }
    }
}

impl Default for Vault {
    fn default() -> Self {
        Self {
            version: CURRENT_VAULT_VERSION,
            revision: 1,
            device_id: uuid::Uuid::new_v4().to_string(),
            modified_at: now_unix(),
            hosts: Vec::new(),
            settings: Settings::default(),
        }
    }
}

/// Encrypt vault content with age using scrypt (passphrase-based).
///
/// Uses age's standard scrypt recipient, compatible with the `age` CLI.
/// Matches age CLI default. Target: ~1s decrypt on typical hardware.
pub fn encrypt(vault: &Vault, passphrase: &str) -> Result<Vec<u8>> {
    encrypt_inner(vault, passphrase, PRODUCTION_LOG_N)
}

/// Internal encrypt with explicit work factor.
/// `pub(crate)` for persist.rs's `save_vault_inner`; tests also access directly.
pub(crate) fn encrypt_inner(vault: &Vault, passphrase: &str, log_n: u8) -> Result<Vec<u8>> {
    let mut plaintext =
        Zeroizing::new(serde_json::to_vec_pretty(vault).context("Failed to serialize vault")?);

    let secret = SecretString::from(passphrase.to_owned());
    let mut recipient = age::scrypt::Recipient::new(secret);
    recipient.set_work_factor(log_n);
    let encryptor = Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient))
        .context("Failed to create encryptor")?;

    let mut encrypted = vec![];
    let mut writer = encryptor
        .wrap_output(&mut encrypted)
        .context("Failed to create age encryptor")?;
    writer
        .write_all(&plaintext)
        .context("Failed to write vault data")?;
    writer
        .finish()
        .context("Failed to finalize age encryption")?;

    plaintext.zeroize();
    Ok(encrypted)
}

/// Maximum scrypt work factor accepted during decryption.
/// Files with log_n > this will be rejected to prevent memory DoS.
const MAX_DECRYPT_LOG_N: u8 = 20;

/// Decrypt age-encrypted content with the given passphrase.
///
/// Compatible with files encrypted by the `age` CLI tool.
/// Rejects work factor > MAX_DECRYPT_LOG_N to prevent memory exhaustion
/// from tampered files (e.g. from S3).
///
/// Handles version migration: if the vault was written by an older version
/// of vida, it is transparently migrated to the current format.
pub fn decrypt(ciphertext: &[u8], passphrase: &str) -> Result<Vault> {
    let secret = SecretString::from(passphrase.to_owned());
    let mut identity = age::scrypt::Identity::new(secret);
    identity.set_max_work_factor(MAX_DECRYPT_LOG_N);

    let decryptor = Decryptor::new(ciphertext).context("Failed to parse age encryption header")?;

    let mut reader = decryptor
        .decrypt(iter::once(&identity as _))
        .context("Failed to decrypt vault (wrong passphrase or work factor too high?)")?;

    let mut plaintext = Zeroizing::new(vec![]);
    reader
        .read_to_end(&mut plaintext)
        .context("Failed to read decrypted vault data")?;

    // Check version before deserializing (migration at JSON level)
    // Missing version → treat as v1 (pre-versioning era)
    let version = serde_json::from_slice::<serde_json::Value>(&plaintext)
        .ok()
        .and_then(|v| v.get("version")?.as_u64())
        .unwrap_or(1) as u32;

    if version > CURRENT_VAULT_VERSION {
        anyhow::bail!(
            "Vault version {} is newer than this program (supports up to {}). \
             Upgrade vida or use a compatible version.",
            version,
            CURRENT_VAULT_VERSION
        );
    }

    let json_bytes = if version < CURRENT_VAULT_VERSION {
        // Migration at JSON level (in-memory, no backup needed for decrypt)
        migrate_json(&plaintext, version)?
    } else {
        plaintext.to_vec()
    };

    let vault: Vault = serde_json::from_slice(&json_bytes)
        .context("Failed to parse vault JSON (corrupted data?)")?;

    Ok(vault)
}

/// Apply JSON-level migration from old_version to CURRENT_VAULT_VERSION.
/// Missing version field is treated as v1 (pre-versioning era).
fn migrate_json(plaintext: &[u8], from_version: u32) -> Result<Vec<u8>> {
    let mut root: serde_json::Value =
        serde_json::from_slice(plaintext).context("Failed to parse vault JSON for migration")?;

    if from_version < 2 {
        migrate_json_v1_to_v2(&mut root)?;
    }
    if from_version < 3 {
        migrate_json_v2_to_v3(&mut root)?;
    }
    if from_version < 4 {
        migrate_json_v3_to_v4(&mut root)?;
    }

    serde_json::to_vec_pretty(&root).context("Failed to serialize migrated vault")
}

/// Migrate raw vault JSON bytes from an old version to current.
/// Creates a backup of the original file before writing.
///
/// Migration happens at the JSON level (not struct level) because
/// old formats may not deserialize into current structs.
///
/// Used by `persist::load_vault` when it detects a version mismatch.
pub fn migrate_vault_json(plaintext: &[u8], backup_path: &Path) -> Result<Vec<u8>> {
    let mut root: serde_json::Value =
        serde_json::from_slice(plaintext).context("Failed to parse vault JSON for migration")?;

    // Missing version field → treat as v1 (pre-versioning)
    let version = root.get("version").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

    if version > CURRENT_VAULT_VERSION {
        anyhow::bail!(
            "Vault version {} is newer than this program (supports up to {}). \
             Upgrade vida or use a compatible version.",
            version,
            CURRENT_VAULT_VERSION
        );
    }
    if version == CURRENT_VAULT_VERSION {
        return Ok(plaintext.to_vec());
    }

    // Backup original before migration
    std::fs::write(backup_path, plaintext).context("Failed to create pre-migration backup")?;

    // Apply migration chain at JSON level
    if version < 2 {
        migrate_json_v1_to_v2(&mut root)?;
    }
    if version < 3 {
        migrate_json_v2_to_v3(&mut root)?;
    }
    if version < 4 {
        migrate_json_v3_to_v4(&mut root)?;
    }

    let migrated =
        serde_json::to_vec_pretty(&root).context("Failed to serialize migrated vault")?;
    Ok(migrated)
}

/// v1 → v2: AuthMethod::Password changed from unit variant "Password"
/// to struct variant { "Password": { "password": "" } }.
/// Passwords must be re-entered by the user after migration.
fn migrate_json_v1_to_v2(root: &mut serde_json::Value) -> Result<()> {
    root["version"] = serde_json::json!(2);

    if let Some(hosts) = root.get_mut("hosts").and_then(|h| h.as_array_mut()) {
        for host in hosts {
            if let Some(auth) = host.get_mut("auth") {
                // v1: "auth": "Password"  →  v2: "auth": {"Password": {"password": ""}}
                if auth.as_str() == Some("Password") {
                    *auth = serde_json::json!({
                        "Password": { "password": "" }
                    });
                }
            }
        }
    }
    Ok(())
}

/// v2 → v3: Added revision, device_id, modified_at to Vault;
/// added id (UUID) to each HostEntry.
fn migrate_json_v2_to_v3(root: &mut serde_json::Value) -> Result<()> {
    root["version"] = serde_json::json!(3);

    // Add revision (start at 1 for migrated vaults)
    if root.get("revision").is_none() {
        root["revision"] = serde_json::json!(1);
    }

    // Add device_id (generate new UUID for migrated vaults)
    if root.get("device_id").is_none() {
        root["device_id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
    }

    // Add modified_at (current time)
    if root.get("modified_at").is_none() {
        root["modified_at"] = serde_json::json!(now_unix());
    }

    // Add id to each host entry
    if let Some(hosts) = root.get_mut("hosts").and_then(|h| h.as_array_mut()) {
        for host in hosts {
            if host.get("id").is_none() {
                host["id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
            }
        }
    }

    Ok(())
}

/// v3 → v4: Added sync_local_path to Settings.
fn migrate_json_v3_to_v4(root: &mut serde_json::Value) -> Result<()> {
    root["version"] = serde_json::json!(4);

    // Add sync_local_path to settings (null = sync disabled)
    if let Some(settings) = root.get_mut("settings")
        && settings.get("sync_local_path").is_none()
    {
        settings["sync_local_path"] = serde_json::Value::Null;
    }

    Ok(())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fast roundtrip test (log_n=10, ~0.04s).
    #[test]
    fn encrypt_decrypt_roundtrip() {
        let vault = Vault::default();
        let passphrase = "test-passphrase-123";

        let encrypted = encrypt_inner(&vault, passphrase, 10).unwrap();
        let decrypted = decrypt(&encrypted, passphrase).unwrap();

        assert_eq!(decrypted.version, CURRENT_VAULT_VERSION);
        assert!(decrypted.hosts.is_empty());
    }

    #[test]
    fn decrypt_wrong_passphrase_fails() {
        let vault = Vault::default();
        let encrypted = encrypt_inner(&vault, "correct-passphrase", 10).unwrap();
        let result = decrypt(&encrypted, "wrong-passphrase");
        assert!(result.is_err());
    }

    /// Production work factor constant guard. If this fails, someone
    /// changed PRODUCTION_LOG_N without re-evaluating the security/perf tradeoff.
    #[test]
    fn production_log_n_is_18() {
        assert_eq!(PRODUCTION_LOG_N, 18);
    }

    /// age CLI interop MUST use production work factor (log_n=18).
    /// This test is slow (~0.36s) but validates real-world compatibility.
    ///
    /// This is the only validation of design rule "vault must be decryptable
    /// by the standard age CLI" — it decrypts with the REAL `age` binary.
    /// It must NOT skip when age/expect is missing; it fails loudly instead,
    /// because CI installs age via `brew install age`.
    #[test]
    fn age_cli_interop() {
        let mut vault = Vault::default();
        vault.hosts.push(HostEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name: "interop-test".into(),
            host: "192.168.1.1".into(),
            user: "root".into(),
            port: 22,
            tags: vec!["test".into()],
            group: None,
            color: None,
            auth: AuthMethod::Password {
                password: SecureString::new("secret123".to_owned()),
            },
            notes: Some("Interop test host".into()),
        });

        let passphrase = "age-cli-interop-test";
        let encrypted = encrypt(&vault, passphrase).unwrap();

        // Verify age header declares scrypt with log_n=18
        let header = String::from_utf8_lossy(&encrypted);
        assert!(header.contains("scrypt"), "should be scrypt recipient");
        assert!(header.contains("18"), "should contain log_n=18");

        // Decrypt back with our own implementation
        let decrypted = decrypt(&encrypted, passphrase).unwrap();
        assert_eq!(decrypted.hosts.len(), 1);
        assert_eq!(decrypted.hosts[0].name, "interop-test");
        assert_eq!(decrypted.hosts[0].user, "root");

        // Now decrypt with the REAL standard age CLI. age requires a TTY for
        // passphrase input, so we drive it with `expect` (present on macOS CI).
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.age");
        std::fs::write(&vault_path, &encrypted).unwrap();

        let script_path = tmp.path().join("decrypt.exp");
        let script = format!(
            "set timeout 60\n\
             spawn age -d {path}\n\
             expect \"Enter passphrase\"\n\
             send \"{pass}\\r\"\n\
             expect eof\n",
            path = vault_path.display(),
            pass = passphrase,
        );
        std::fs::write(&script_path, script).unwrap();

        let output = std::process::Command::new("expect")
            .arg(&script_path)
            .output()
            .expect("expect not found: standard age CLI interop requires `expect` (preinstalled on macOS)");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "age CLI decryption failed. stdout: {}\nstderr: {}",
            stdout,
            stderr
        );

        // The decrypted output must be the vault JSON that our own decrypt produces.
        // expect echoes the spawn line and terminal control chars; extract the JSON
        // between the first `{` and the last `}`.
        let start = stdout.find('{').expect("no JSON in age CLI output");
        let end = stdout.rfind('}').expect("no JSON in age CLI output");
        let theirs_json: serde_json::Value =
            serde_json::from_str(&stdout[start..=end]).expect("age CLI output is not valid JSON");
        let ours_json = serde_json::to_value(decrypt(&encrypted, passphrase).unwrap()).unwrap();
        assert_eq!(
            theirs_json, ours_json,
            "age CLI output differs from our decryption"
        );
    }

    /// Verify Debug output never leaks secrets.
    #[test]
    fn debug_output_redacts_secrets() {
        // HostEntry with password
        let entry = HostEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name: "test".into(),
            host: "1.2.3.4".into(),
            user: "admin".into(),
            port: 22,
            tags: vec![],
            group: None,
            color: None,
            auth: AuthMethod::Password {
                password: SecureString::new("hunter2".to_owned()),
            },
            notes: None,
        };
        let debug_str = format!("{:?}", entry);
        assert!(
            !debug_str.contains("hunter2"),
            "Debug leaked password: {}",
            debug_str
        );
        assert!(
            debug_str.contains("[REDACTED]"),
            "should contain [REDACTED]: {}",
            debug_str
        );

        // Settings with s3_secret_key
        let settings = Settings {
            s3_endpoint: None,
            s3_bucket: None,
            s3_access_key: None,
            s3_secret_key: Some(SecureString::new("super-secret-key".to_owned())),
            sync_local_path: None,
            scrollback_lines: 5000,
        };
        let settings_debug = format!("{:?}", settings);
        assert!(
            !settings_debug.contains("super-secret-key"),
            "Debug leaked s3_secret_key: {}",
            settings_debug
        );
        assert!(
            settings_debug.contains("[REDACTED]"),
            "s3_secret_key should be [REDACTED]"
        );

        // Full Vault
        let vault = Vault {
            version: 2,
            revision: 1,
            device_id: uuid::Uuid::new_v4().to_string(),
            modified_at: 0,
            hosts: vec![entry],
            settings,
        };
        let vault_debug = format!("{:?}", vault);
        assert!(
            !vault_debug.contains("hunter2"),
            "Vault Debug leaked password"
        );
        assert!(
            !vault_debug.contains("super-secret-key"),
            "Vault Debug leaked s3_secret_key"
        );
        assert!(
            vault_debug.contains("[REDACTED]"),
            "Vault Debug should contain [REDACTED]"
        );

        // SecureString directly
        let secret = SecureString::new("my-secret-key".to_owned());
        assert_eq!(format!("{:?}", secret), "SecureString([REDACTED])");
        assert_eq!(format!("{}", secret), "[REDACTED]");
    }

    /// Reject vault version newer than this program supports.
    #[test]
    fn reject_future_version() {
        let future_json = r#"{"version": 99, "hosts": [], "settings": {}}"#;
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.json");
        let result = migrate_vault_json(future_json.as_bytes(), &backup);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("newer than this program"), "error: {}", err);
    }

    /// Migrate from v1 (unit-variant Password) to v2 (struct variant).
    /// This is the REAL migration that handles the AuthMethod::Password change.
    #[test]
    fn migrate_v1_password_unit_to_v2() {
        // v1 format: "auth": "Password" (unit variant)
        let v1_json = r##"{
  "version": 1,
  "hosts": [
    {
      "name": "production-web",
      "host": "10.0.1.50",
      "user": "deploy",
      "port": 22,
      "tags": ["prod"],
      "group": "servers",
      "color": "#ff6b6b",
      "auth": "Password",
      "notes": "Production web server"
    },
    {
      "name": "staging-db",
      "host": "10.0.2.100",
      "user": "admin",
      "port": 2222,
      "tags": ["staging", "db"],
      "group": null,
      "color": null,
      "auth": "Password",
      "notes": null
    }
  ],
  "settings": {
    "s3_endpoint": "https://s3.example.com",
    "s3_bucket": "my-vault",
    "s3_access_key": "AKIAIOSFODNN7EXAMPLE",
    "s3_secret_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    "scrollback_lines": 10000
  }
}"##;

        let dir = tempfile::tempdir().unwrap();
        let backup_path = dir.path().join("backup.json");
        let migrated_bytes = migrate_vault_json(v1_json.as_bytes(), &backup_path).unwrap();

        // Backup should contain original v1 data
        assert!(backup_path.exists());
        let backup = std::fs::read_to_string(&backup_path).unwrap();
        assert!(backup.contains("\"version\": 1"));
        assert!(backup.contains("\"auth\": \"Password\""));

        // Migrated should deserialize into current Vault
        let vault: Vault = serde_json::from_slice(&migrated_bytes).unwrap();
        assert_eq!(vault.version, 4);
        assert_eq!(vault.hosts.len(), 2);

        // Verify new v3 fields are populated
        assert_eq!(vault.revision, 1);
        assert!(!vault.device_id.is_empty());
        assert!(vault.modified_at > 0);
        assert!(!vault.hosts[0].id.is_empty());
        assert!(!vault.hosts[1].id.is_empty());

        // Host 1: all fields preserved
        assert_eq!(vault.hosts[0].name, "production-web");
        assert_eq!(vault.hosts[0].host, "10.0.1.50");
        assert_eq!(vault.hosts[0].user, "deploy");
        assert_eq!(vault.hosts[0].port, 22);
        assert_eq!(vault.hosts[0].tags, vec!["prod"]);
        assert_eq!(vault.hosts[0].group.as_deref(), Some("servers"));
        assert_eq!(vault.hosts[0].color.as_deref(), Some("#ff6b6b"));
        assert_eq!(
            vault.hosts[0].notes.as_deref(),
            Some("Production web server")
        );

        // Host 2: password migrated (empty, needs re-entry)
        assert_eq!(vault.hosts[1].name, "staging-db");
        match &vault.hosts[1].auth {
            AuthMethod::Password { password } => {
                assert_eq!(password.expose(), "", "migrated password should be empty");
            }
            other => panic!("expected Password variant, got {:?}", other),
        }

        // Settings preserved
        assert_eq!(
            vault.settings.s3_endpoint.as_deref(),
            Some("https://s3.example.com")
        );
        assert_eq!(vault.settings.s3_bucket.as_deref(), Some("my-vault"));
        assert_eq!(vault.settings.scrollback_lines, 10000);
    }

    /// Missing version field → treated as v1 → migrated to v2.
    #[test]
    fn missing_version_treated_as_v1() {
        let no_version_json = r#"{
  "hosts": [
    {
      "name": "old-host",
      "host": "1.2.3.4",
      "user": "root",
      "port": 22,
      "tags": [],
      "group": null,
      "color": null,
      "auth": "Password",
      "notes": null
    }
  ],
  "settings": {
    "s3_endpoint": null,
    "s3_bucket": null,
    "s3_access_key": null,
    "s3_secret_key": null,
    "scrollback_lines": 5000
  }
}"#;

        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.json");
        let migrated = migrate_vault_json(no_version_json.as_bytes(), &backup).unwrap();
        let vault: Vault = serde_json::from_slice(&migrated).unwrap();
        assert_eq!(vault.version, 4);
        assert_eq!(vault.hosts[0].name, "old-host");
        assert!(!vault.hosts[0].id.is_empty());
    }

    /// Migrate from v3 to v4: added sync_local_path to Settings.
    #[test]
    fn migrate_v3_to_v4_adds_sync_local_path() {
        let v3_json = r##"{
  "version": 3,
  "revision": 5,
  "device_id": "test-device-id",
  "modified_at": 1700000000,
  "hosts": [
    {
      "id": "host-uuid-1",
      "name": "my-server",
      "host": "10.0.0.1",
      "user": "root",
      "port": 22,
      "tags": ["prod"],
      "group": "servers",
      "color": "#ff0000",
      "auth": { "Password": { "password": "secret123" } },
      "notes": "Production"
    }
  ],
  "settings": {
    "s3_endpoint": "https://s3.example.com",
    "s3_bucket": "my-bucket",
    "s3_access_key": "AKID",
    "s3_secret_key": "SECRET",
    "scrollback_lines": 5000
  }
}"##;

        let dir = tempfile::tempdir().unwrap();
        let backup_path = dir.path().join("backup.json");
        let migrated_bytes = migrate_vault_json(v3_json.as_bytes(), &backup_path).unwrap();

        // Backup should contain original v3 data
        assert!(backup_path.exists());
        let backup = std::fs::read_to_string(&backup_path).unwrap();
        assert!(backup.contains("\"version\": 3"), "backup must preserve v3");

        // Migrated should be v4 with sync_local_path
        let vault: Vault = serde_json::from_slice(&migrated_bytes).unwrap();
        assert_eq!(vault.version, 4);
        assert!(
            vault.settings.sync_local_path.is_none(),
            "sync_local_path defaults to null"
        );
        assert_eq!(vault.settings.scrollback_lines, 5000);

        // Existing v3 fields preserved
        assert_eq!(vault.hosts.len(), 1);
        assert_eq!(vault.hosts[0].name, "my-server");
        assert_eq!(vault.revision, 5);
        assert_eq!(vault.device_id, "test-device-id");
        assert_eq!(
            vault.settings.s3_endpoint.as_deref(),
            Some("https://s3.example.com")
        );
    }

    /// Snapshot test: lock current version ↔ structure correspondence.
    /// If this fails, someone changed the struct without bumping version
    /// and adding migration (see AGENTS.md rule).
    #[test]
    fn version_snapshot() {
        let vault = Vault {
            version: CURRENT_VAULT_VERSION,
            revision: 1,
            device_id: uuid::Uuid::new_v4().to_string(),
            modified_at: 0,
            hosts: vec![HostEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: "snapshot-host".into(),
                host: "192.168.1.1".into(),
                user: "root".into(),
                port: 22,
                tags: vec!["tag1".into(), "tag2".into()],
                group: Some("group1".into()),
                color: Some("#aabbcc".into()),
                auth: AuthMethod::Password {
                    password: SecureString::new("snapshot-pass".into()),
                },
                notes: Some("Snapshot test host".into()),
            }],
            settings: Settings {
                s3_endpoint: Some("https://s3.example.com".into()),
                s3_bucket: Some("bucket".into()),
                s3_access_key: Some("AKID".into()),
                s3_secret_key: Some(SecureString::new("SECRET".into())),
                sync_local_path: Some("/Users/test/VidaSync".into()),
                scrollback_lines: 8192,
            },
        };

        let json = serde_json::to_string_pretty(&vault).unwrap();

        // Verify key structural markers
        assert!(json.contains("\"version\": 4"), "version must be 4");
        assert!(json.contains("\"revision\""), "must have revision field");
        assert!(json.contains("\"device_id\""), "must have device_id field");
        assert!(
            json.contains("\"modified_at\""),
            "must have modified_at field"
        );
        assert!(json.contains("\"id\""), "HostEntry must have id field");
        assert!(json.contains("\"Password\""), "auth variant name");
        assert!(json.contains("\"password\""), "password field in auth");
        assert!(
            json.contains("\"sync_local_path\""),
            "settings must have sync_local_path"
        );
        assert!(json.contains("\"s3_secret_key\""), "settings field");

        // Verify it roundtrips
        let decoded: Vault = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.version, CURRENT_VAULT_VERSION);
        assert_eq!(decoded.hosts[0].name, "snapshot-host");
    }
}
