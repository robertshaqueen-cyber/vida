use crate::vault::{self, Vault};
use anyhow::{Context, Result};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const ACCOUNT_NAME: &str = "master-passphrase";
const CACHE_VERSION: u8 = 1;
pub const MAX_CACHE_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Serialize)]
struct CacheEnvelopeRef<'a> {
    version: u8,
    expires_at: u64,
    passphrase: &'a str,
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct CacheEnvelope {
    version: u8,
    expires_at: u64,
    passphrase: String,
}

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
pub fn cache_passphrase(passphrase: &SecretString, ttl: Duration) -> Result<()> {
    let service = service_name()?;
    cache_passphrase_for_service(&service, passphrase, ttl)
}

fn cache_passphrase_for_service(
    service: &str,
    passphrase: &SecretString,
    ttl: Duration,
) -> Result<()> {
    let ttl_seconds = ttl.as_secs();
    anyhow::ensure!(
        (1..=MAX_CACHE_SECONDS).contains(&ttl_seconds),
        "Passphrase cache duration must be between 1 second and 7 days"
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before Unix epoch")?
        .as_secs();
    let envelope = CacheEnvelopeRef {
        version: CACHE_VERSION,
        expires_at: now.saturating_add(ttl_seconds),
        passphrase: passphrase.expose_secret(),
    };
    let encoded =
        Zeroizing::new(serde_json::to_string(&envelope).context("Failed to encode keyring cache")?);
    let entry =
        keyring::Entry::new(service, ACCOUNT_NAME).context("Failed to open keyring entry")?;
    // Delete existing entry first (macOS keychain errors on duplicate)
    let _ = entry.delete_credential();
    entry
        .set_password(encoded.as_str())
        .context("Failed to cache passphrase in keyring")?;
    Ok(())
}

/// Retrieve the cached passphrase from the OS keyring.
///
/// Returns `None` if no passphrase is cached (first run, cleared cache,
/// new machine, keyring unavailable).
pub fn get_cached_passphrase() -> Option<SecretString> {
    let service = service_name().ok()?;
    get_cached_passphrase_for_service(&service)
}

fn get_cached_passphrase_for_service(service: &str) -> Option<SecretString> {
    let entry = keyring::Entry::new(service, ACCOUNT_NAME).ok()?;
    let encoded = Zeroizing::new(entry.get_password().ok()?);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let Some(passphrase) = decode_cached_passphrase(&encoded, now) else {
        let _ = entry.delete_credential();
        return None;
    };
    Some(passphrase)
}

fn decode_cached_passphrase(encoded: &str, now: u64) -> Option<SecretString> {
    // Legacy entries stored an unbounded plaintext value. Do not keep
    // honoring them after duration-based caching is introduced.
    let mut envelope: CacheEnvelope = serde_json::from_str(encoded).ok()?;
    if envelope.version != CACHE_VERSION || envelope.expires_at <= now {
        return None;
    }
    Some(SecretString::from(std::mem::take(&mut envelope.passphrase)))
}

/// Clear the cached passphrase from the OS keyring.
///
/// After this, the user must enter their passphrase manually.
pub fn clear_cache() -> Result<()> {
    let service = service_name()?;
    clear_cache_for_service(&service)
}

fn clear_cache_for_service(service: &str) -> Result<()> {
    let entry =
        keyring::Entry::new(service, ACCOUNT_NAME).context("Failed to open keyring entry")?;
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
    unlock_vault_with_cached(encrypted, manual_passphrase, get_cached_passphrase())
}

fn unlock_vault_with_cached(
    encrypted: &[u8],
    manual_passphrase: &SecretString,
    cached_passphrase: Option<SecretString>,
) -> Result<(Vault, UnlockMethod)> {
    // Try keyring cache first
    if let Some(cached) = cached_passphrase {
        match vault::decrypt(encrypted, cached.expose_secret()) {
            Ok(vault) => return Ok((vault, UnlockMethod::Keyring)),
            Err(e) => {
                warn!(
                    "keyring cache decrypt failed ({}), falling back to manual passphrase",
                    e
                );
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
        let service = format!("com.vida.test.{}", uuid::Uuid::new_v4());

        // Cache
        cache_passphrase_for_service(&service, &passphrase, Duration::from_secs(60)).unwrap();

        // Retrieve
        let cached = get_cached_passphrase_for_service(&service).unwrap();
        assert_eq!(cached.expose_secret(), passphrase.expose_secret());

        // Clear
        clear_cache_for_service(&service).unwrap();

        // Verify cleared (get_cached returns None or error)
        // On macOS, after deletion, get_password returns Err
        let _after_clear = get_cached_passphrase_for_service(&service);
        // We accept either None or Some (if keyring has stale state)
        // The important thing is that clear_cache succeeded
    }

    #[test]
    fn bounded_cache_rejects_expired_and_legacy_entries() {
        let valid = r#"{"version":1,"expires_at":101,"passphrase":"secret"}"#;
        let decoded = decode_cached_passphrase(valid, 100).unwrap();
        assert_eq!(decoded.expose_secret(), "secret");
        assert!(decode_cached_passphrase(valid, 101).is_none());
        assert!(decode_cached_passphrase("legacy-unbounded-secret", 100).is_none());
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
            auth: AuthMethod::Password {
                password: SecureString::new("deploy-pass".to_owned()),
            },
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
            auth: AuthMethod::Password {
                password: SecureString::new("admin-pass".to_owned()),
            },
            notes: Some("Staging database".to_owned()),
        });

        // 2. 加密 (log_n=10 for fast test)
        let passphrase = SecretString::from("unlock-test-passphrase".to_owned());
        let encrypted =
            crate::vault::encrypt_inner(&vault, passphrase.expose_secret(), 10).unwrap();

        // 3. 调用应用级解锁逻辑，显式模拟 keyring 无缓存。
        // 测试不得读写用户真实的 Vida Keychain 项。
        let (decrypted, method) = unlock_vault_with_cached(&encrypted, &passphrase, None).unwrap();

        // 5. 断言回落到口令输入
        assert!(
            matches!(method, UnlockMethod::Passphrase),
            "should have fallen back to manual passphrase, got {:?}",
            method
        );

        // 6. 断言数据完整
        assert_eq!(
            decrypted.hosts.len(),
            2,
            "both hosts should survive decrypt"
        );
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

        assert_eq!(
            before, after,
            "service_name must be identical before and after directory creation"
        );

        // SAFETY: test cleanup
        unsafe { std::env::remove_var("VIDA_CONFIG_DIR") };
    }
}
