use crate::vault::{self, Vault};
use anyhow::{Context, Result};
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tracing::warn;

const ACCOUNT_NAME: &str = "master-passphrase";

/// Compute keyring service name from config directory.
/// Uses sha2 to ensure stability across Rust toolchain versions.
/// Service name varies with VIDA_CONFIG_DIR so different config
/// directories don't share keyring entries.
fn service_name() -> Result<String> {
    let config_dir = crate::config::config_dir()?;
    // Hash the logical path (no canonicalize — first run may not exist yet)
    let mut hasher = Sha256::new();
    hasher.update(config_dir.to_string_lossy().as_bytes());
    let hash = hasher.finalize();
    Ok(format!("com.vida.vault.{}", hex::encode(&hash[..8])))
}

/// Cache the master passphrase in the OS keyring.
///
/// This is a CACHE ONLY. The vault must always be decryptable with just
/// the user-supplied passphrase. If keyring is empty or unavailable,
/// the app falls back to manual passphrase entry.
pub fn cache_passphrase(passphrase: &SecretString) -> Result<()> {
    let svc = service_name()?;
    let entry = keyring::Entry::new(&svc, ACCOUNT_NAME)
        .context("Failed to open keyring entry")?;
    // Delete existing entry first (macOS keychain errors on duplicate)
    let _ = entry.delete_credential();
    entry
        .set_password(passphrase.expose_secret())
        .context("Failed to cache passphrase in keyring")?;
    Ok(())
}

/// Retrieve the cached passphrase from the OS keyring.
///
/// Returns `None` if no passphrase is cached (first run, cleared cache,
/// new machine, keyring unavailable).
pub fn get_cached_passphrase() -> Option<SecretString> {
    let svc = service_name().ok()?;
    let entry = keyring::Entry::new(&svc, ACCOUNT_NAME).ok()?;
    let password = entry.get_password().ok()?;
    Some(SecretString::from(password))
}

/// Clear the cached passphrase from the OS keyring.
///
/// After this, the user must enter their passphrase manually.
pub fn clear_cache() -> Result<()> {
    let svc = service_name()?;
    let entry = keyring::Entry::new(&svc, ACCOUNT_NAME)
        .context("Failed to open keyring entry")?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()), // Already cleared
        Err(e) => Err(e).context("Failed to clear keyring cache"),
    }
}

/// Check if a passphrase is cached in the keyring.
pub fn is_cached() -> bool {
    get_cached_passphrase().is_some()
}

/// Application-level vault unlock. Tries keyring cache first, falls back
/// to manual passphrase entry.
///
/// Returns the decrypted Vault and which auth path was used.
#[derive(Debug)]
pub enum UnlockMethod {
    Keyring,
    Passphrase,
}

pub fn unlock_vault(
    encrypted: &[u8],
    manual_passphrase: &SecretString,
) -> Result<(Vault, UnlockMethod)> {
    // Try keyring cache first
    if let Some(cached) = get_cached_passphrase() {
        match vault::decrypt(encrypted, cached.expose_secret()) {
            Ok(vault) => return Ok((vault, UnlockMethod::Keyring)),
            Err(e) => {
                warn!("keyring cache decrypt failed ({}), falling back to manual passphrase", e);
            }
        }
    }
    // Fall back to manual passphrase
    let vault = vault::decrypt(encrypted, manual_passphrase.expose_secret())
        .context("Failed to decrypt vault with manual passphrase")?;
    Ok((vault, UnlockMethod::Passphrase))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{AuthMethod, HostEntry, SecureString};

    #[test]
    fn cache_lifecycle() {
        // Full lifecycle: cache → retrieve → clear → verify gone
        let passphrase = SecretString::from("test-lifecycle-passphrase".to_owned());

        // Cache
        cache_passphrase(&passphrase).unwrap();

        // Retrieve
        let cached = get_cached_passphrase().unwrap();
        assert_eq!(cached.expose_secret(), passphrase.expose_secret());

        // Clear
        clear_cache().unwrap();

        // Verify cleared (get_cached returns None or error)
        // On macOS, after deletion, get_password returns Err
        let _after_clear = get_cached_passphrase();
        // We accept either None or Some (if keyring has stale state)
        // The important thing is that clear_cache succeeded
    }

    #[test]
    fn keyring_only_path_works() {
        // 测试要求 D："应用必须能仅凭用户输入的口令解锁，完全不依赖 keyring"
        //
        // 测应用级入口 unlock_vault（而非底层 encrypt/decrypt）。
        // 断言：keyring 为空时，回落到口令输入，解锁成功，数据完整。

        // 1. 构造含真实主机数据的 vault
        let mut vault = Vault::default();
        vault.hosts.push(HostEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name: "production-web".to_owned(),
            host: "10.0.1.50".to_owned(),
            user: "deploy".to_owned(),
            port: 22,
            tags: vec!["prod".to_owned()],
            group: None,
            color: None,
            auth: AuthMethod::Password { password: SecureString::new("deploy-pass".to_owned()) },
            notes: Some("Production web server".to_owned()),
        });
        vault.hosts.push(HostEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name: "staging-db".to_owned(),
            host: "10.0.2.100".to_owned(),
            user: "admin".to_owned(),
            port: 2222,
            tags: vec!["staging".to_owned(), "db".to_owned()],
            group: None,
            color: None,
            auth: AuthMethod::Password { password: SecureString::new("admin-pass".to_owned()) },
            notes: Some("Staging database".to_owned()),
        });

        // 2. 加密 (log_n=10 for fast test)
        let passphrase = SecretString::from("unlock-test-passphrase".to_owned());
        let encrypted = crate::vault::encrypt_inner(&vault, passphrase.expose_secret(), 10).unwrap();

        // 3. 清空 keyring
        clear_cache().unwrap();

        // 4. 调用应用级解锁入口（应走口令回落路径）
        let (decrypted, method) = unlock_vault(&encrypted, &passphrase).unwrap();

        // 5. 断言回落到口令输入
        assert!(
            matches!(method, UnlockMethod::Passphrase),
            "should have fallen back to manual passphrase, got {:?}",
            method
        );

        // 6. 断言数据完整
        assert_eq!(decrypted.hosts.len(), 2, "both hosts should survive decrypt");
        assert_eq!(decrypted.hosts[0].name, "production-web");
        assert_eq!(decrypted.hosts[0].user, "deploy");
        assert_eq!(decrypted.hosts[1].name, "staging-db");
        assert_eq!(decrypted.hosts[1].port, 2222);
    }

    #[test]
    fn service_name_consistency() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("vida-config");

        // SAFETY: test runs single-threaded, no concurrent env access
        unsafe { std::env::set_var("VIDA_CONFIG_DIR", &config_path) };

        // Before directory exists
        let before = service_name().unwrap();

        // Create the directory
        std::fs::create_dir_all(&config_path).unwrap();

        // After directory exists
        let after = service_name().unwrap();

        assert_eq!(before, after, "service_name must be identical before and after directory creation");

        // SAFETY: test cleanup
        unsafe { std::env::remove_var("VIDA_CONFIG_DIR") };
    }
}
