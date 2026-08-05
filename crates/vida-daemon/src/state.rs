use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::info;
use vida_core::config;
use vida_core::sync::{LocalPathBackend, SyncCoordinator, SyncResult};
use vida_core::vault::{AuthMethod, Settings, Vault};

use crate::protocol::{ConflictChoice, HostRequest, HostSummary, VaultStatusInfo};

// ---------------------------------------------------------------------------
// DaemonState — shared across all WebSocket connections
// ---------------------------------------------------------------------------

pub struct DaemonState {
    pub token: String,
    pub vault: Option<Vault>,
    pub passphrase: Option<String>,
    pub vault_path: PathBuf,
    pub sync: Option<SyncCoordinator>,
    pub i18n: vida_core::i18n::I18n,
}

impl DaemonState {
    pub fn new(token: String) -> Result<Self> {
        let vault_path = config::vault_path()?;
        Ok(Self {
            token,
            vault: None,
            passphrase: None,
            vault_path,
            sync: None,
            i18n: vida_core::i18n::I18n::new(vida_core::i18n::detect_lang()),
        })
    }

    // Vault -----------------------------------------------------------------

    pub fn create_vault(&mut self, passphrase: &str) -> Result<VaultStatusInfo> {
        if self.vault_path.exists() {
            anyhow::bail!("{}", self.i18n.tr("daemon_vault_exists"));
        }
        let vault = Vault::default();
        let ct = vida_core::vault::encrypt(&vault, passphrase)?;
        vida_core::persist::write_atomic(&self.vault_path, &ct)?;
        self.vault = Some(vault);
        self.passphrase = Some(passphrase.to_string());
        self.init_sync()?;
        info!("Vault created");
        Ok(self.vault_status())
    }

    pub fn unlock(&mut self, passphrase: &str, remember: bool) -> Result<VaultStatusInfo> {
        if !self.vault_path.exists() {
            anyhow::bail!("{}", self.i18n.tr("daemon_vault_missing"));
        }
        let ct = std::fs::read(&self.vault_path).context("Failed to read vault file")?;
        let vault = vida_core::vault::decrypt(&ct, passphrase)?;
        if remember {
            let sec = secrecy::SecretString::from(passphrase.to_string());
            vida_core::keyring_cache::cache_passphrase(&sec)?;
            info!("Passphrase cached in keyring");
        }
        self.vault = Some(vault);
        self.passphrase = Some(passphrase.to_string());
        self.init_sync()?;
        info!("Vault unlocked");
        Ok(self.vault_status())
    }

    pub fn lock(&mut self) {
        self.vault = None;
        self.passphrase = None;
        self.sync = None;
        info!("Vault locked");
    }

    pub fn vault_status(&self) -> VaultStatusInfo {
        match &self.vault {
            Some(v) => VaultStatusInfo {
                locked: false,
                host_count: v.hosts.len(),
                revision: v.revision,
                device_id: v.device_id.clone(),
                vault_exists: true,
            },
            None => VaultStatusInfo {
                locked: true,
                host_count: 0,
                revision: 0,
                device_id: String::new(),
                vault_exists: self.vault_path.exists(),
            },
        }
    }

    pub fn export_backup(&self, passphrase: Option<&str>) -> Result<Vec<u8>> {
        let vault = self.ensure_unlocked()?;
        let p = match passphrase {
            Some(p) => p.to_string(),
            None => self.ensure_passphrase()?.to_string(),
        };
        vida_core::vault::encrypt(vault, &p)
    }

    fn init_sync(&mut self) -> Result<()> {
        let sync_path = self
            .vault
            .as_ref()
            .and_then(|v| v.settings.sync_local_path.as_deref());
        match sync_path {
            Some(path) if !path.is_empty() => {
                let base = std::path::PathBuf::from(path);
                let backend = Box::new(LocalPathBackend::new(base));
                self.sync = Some(SyncCoordinator::new(backend, self.vault_path.clone())?);
                info!("Sync initialized with path: {}", path);
            }
            _ => {
                self.sync = None;
                info!("Sync not configured (sync_local_path is empty)");
            }
        }
        Ok(())
    }

    fn ensure_unlocked(&self) -> Result<&Vault> {
        self.vault
            .as_ref()
            .context(self.i18n.tr("daemon_vault_locked"))
    }

    fn ensure_unlocked_mut(&mut self) -> Result<&mut Vault> {
        self.vault
            .as_mut()
            .context(self.i18n.tr("daemon_vault_locked"))
    }

    fn ensure_passphrase(&self) -> Result<&str> {
        self.passphrase
            .as_deref()
            .context(self.i18n.tr("daemon_passphrase_not_cached"))
    }

    // Settings --------------------------------------------------------------

    pub fn get_settings(&self) -> Result<Settings> {
        let vault = self.ensure_unlocked()?;
        Ok(vault.settings.clone())
    }

    pub fn update_settings(&mut self, settings: Settings) -> Result<()> {
        let passphrase = self.ensure_passphrase()?.to_string();
        let vault_path = self.vault_path.clone();
        let vault = self.ensure_unlocked_mut()?;

        // --- 必改1: 检测 sync_local_path 是否变化，变化时清除旧 SyncState ---
        let old_path = vault.settings.sync_local_path.clone();
        let new_path = settings.sync_local_path.clone();
        let path_changed = old_path != new_path;

        // --- 必改2: 校验新路径 ---
        if let Some(ref p) = new_path
            && !p.is_empty()
        {
            let new_dir = std::path::PathBuf::from(p);

            // 1. 路径必须存在
            if !new_dir.exists() {
                anyhow::bail!("同步文件夹不存在：{}", p);
            }
            // 2. 必须是目录
            if !new_dir.is_dir() {
                anyhow::bail!("路径不是文件夹：{}", p);
            }
            // 3. 必须可写（UUID 文件名避免 iCloud/Dropbox 同步痕迹）
            let probe_name = format!(".vida-write-test-{}", uuid::Uuid::new_v4());
            let probe = new_dir.join(&probe_name);
            match std::fs::write(&probe, b"test") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                }
                Err(e) => {
                    anyhow::bail!("同步文件夹不可写：{} — {}", p, e);
                }
            }
            // 4. 不能是金库自身目录（canonicalize 在存在性检查之后，必然成功）
            let vault_parent = vault_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .canonicalize()?;
            let new_canonical = new_dir.canonicalize()?;
            if new_canonical == vault_parent {
                anyhow::bail!(
                    "同步文件夹不能是金库所在目录（{}），请选择其他位置",
                    vault_parent.display()
                );
            }
        }

        vault.settings = settings;
        vault.modified_at = chrono::Utc::now().timestamp();
        vault.revision += 1;
        let ct = vida_core::vault::encrypt(vault, &passphrase)?;
        vida_core::persist::write_atomic(&vault_path, &ct)?;
        info!("Settings updated");

        // --- 必改1: 路径变化时清除旧 SyncState，防止静默覆盖 ---
        if path_changed {
            let state_path = vida_core::config::config_dir()?.join("sync_state.json");
            if state_path.exists() {
                std::fs::remove_file(&state_path).context("Failed to remove old sync state")?;
                info!("Cleared old SyncState (sync path changed)");
            }
        }

        // Re-initialize sync based on new settings
        self.init_sync()?;
        Ok(())
    }

    // Hosts -----------------------------------------------------------------

    pub fn list_hosts(&self) -> Result<Vec<HostSummary>> {
        let vault = self.ensure_unlocked()?;
        Ok(vault.hosts.iter().map(host_to_summary).collect())
    }

    pub fn reveal_credential(&self, host_id: &str) -> Result<String> {
        let vault = self.ensure_unlocked()?;
        let host = vault
            .hosts
            .iter()
            .find(|h| h.id == host_id)
            .context(self.i18n.tr("daemon_host_not_found"))?;
        match &host.auth {
            AuthMethod::Password { password } => Ok(password.expose().to_string()),
            AuthMethod::Key {
                private_key_path, ..
            } => Ok(format!("key:{}", private_key_path)),
            AuthMethod::KeyInline { private_key, .. } => Ok(private_key.expose().to_string()),
        }
    }

    pub fn update_host(&mut self, req: HostRequest) -> Result<HostSummary> {
        let passphrase = self.ensure_passphrase()?.to_string();
        let vault_path = self.vault_path.clone();
        let i18n = self.i18n.clone();
        let vault = self.ensure_unlocked_mut()?;
        let now = chrono::Utc::now().timestamp();

        if let Some(ref id) = req.id {
            let host = vault
                .hosts
                .iter_mut()
                .find(|h| h.id == *id)
                .context(i18n.tr("daemon_host_not_found"))?;
            host.name = req.name;
            host.host = req.host;
            host.user = req.user;
            host.port = req.port;
            host.tags = req.tags;
            host.group = req.group;
            host.color = req.color;
            host.notes = req.notes;
            if let Some(password) = req.password {
                host.auth = AuthMethod::Password {
                    password: vida_core::vault::SecureString::new(password),
                };
            }
        } else {
            let auth = match req.password {
                Some(p) => AuthMethod::Password {
                    password: vida_core::vault::SecureString::new(p),
                },
                None => anyhow::bail!("{}", i18n.tr("daemon_host_need_password")),
            };
            vault.hosts.push(vida_core::vault::HostEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: req.name,
                host: req.host,
                user: req.user,
                port: req.port,
                tags: req.tags,
                group: req.group,
                color: req.color,
                auth,
                notes: req.notes,
            });
        }
        vault.modified_at = now;
        vault.revision += 1;
        let ct = vida_core::vault::encrypt(vault, &passphrase)?;
        vida_core::persist::write_atomic(&vault_path, &ct)?;
        Ok(host_to_summary(vault.hosts.last().unwrap()))
    }

    pub fn delete_host(&mut self, host_id: &str) -> Result<()> {
        let passphrase = self.ensure_passphrase()?.to_string();
        let vault_path = self.vault_path.clone();
        let vault = self.ensure_unlocked_mut()?;
        let len_before = vault.hosts.len();
        vault.hosts.retain(|h| h.id != host_id);
        if vault.hosts.len() == len_before {
            anyhow::bail!("{}", self.i18n.trf("daemon_host_not_found_id", &[host_id]));
        }
        vault.modified_at = chrono::Utc::now().timestamp();
        vault.revision += 1;
        let ct = vida_core::vault::encrypt(vault, &passphrase)?;
        vida_core::persist::write_atomic(&vault_path, &ct)?;
        info!("Host deleted: {}", host_id);
        Ok(())
    }

    // Sync ------------------------------------------------------------------

    pub async fn sync(&mut self) -> Result<(SyncResult, Option<Vec<HostSummary>>)> {
        // Check if sync is configured before borrowing self.sync mutably
        if self.sync.is_none() {
            return Ok((SyncResult::SyncNotConfigured, None));
        }

        let passphrase = self.ensure_passphrase()?.to_string();
        let vault = self.ensure_unlocked()?;
        let ct = vida_core::vault::encrypt(vault, &passphrase)?;
        let revision = vault.revision;
        let device_id = vault.device_id.clone();
        let local = vida_core::sync::LocalVaultInfo {
            ciphertext: ct,
            revision,
            device_id,
        };

        let result = self.sync.as_mut().unwrap().sync(&local).await?;

        let mut hosts_updated = false;
        match &result {
            SyncResult::Downloaded {
                ciphertext: dl_ct,
                meta,
            } => {
                info!("Downloaded remote vault ({} bytes)", dl_ct.len());
                vida_core::persist::write_atomic(&self.vault_path, dl_ct)?;
                let new_vault = vida_core::vault::decrypt(dl_ct, &passphrase)?;
                let new_revision = new_vault.revision;
                self.vault = Some(new_vault);
                self.sync
                    .as_mut()
                    .unwrap()
                    .update_state_after_download(meta, new_revision);
                hosts_updated = true;
            }
            SyncResult::Conflict { .. } => {
                info!("Sync conflict detected, awaiting user resolution")
            }
            SyncResult::ConflictFilesDetected { files } => {
                info!("{} conflict files detected", files.len())
            }
            SyncResult::RemoteMissing => info!("Remote vault missing, awaiting user action"),
            SyncResult::Uploaded { .. } | SyncResult::NoChange => {}
            SyncResult::SyncNotConfigured => {
                tracing::warn!(
                    "SyncNotConfigured reached post-sync match (should have been caught earlier)"
                );
            }
        }
        let hosts = if hosts_updated {
            Some(self.list_hosts()?)
        } else {
            None
        };
        Ok((result, hosts))
    }

    pub async fn resolve_conflict(
        &mut self,
        choice: ConflictChoice,
    ) -> Result<(Vec<HostSummary>,)> {
        let passphrase = self.ensure_passphrase()?.to_string();
        let vault = self.ensure_unlocked()?;
        let ct = vida_core::vault::encrypt(vault, &passphrase)?;
        let revision = vault.revision;
        let local = vida_core::sync::LocalVaultInfo {
            ciphertext: ct.clone(),
            revision,
            device_id: vault.device_id.clone(),
        };

        let sync = self
            .sync
            .as_mut()
            .context(self.i18n.tr("daemon_sync_not_initialized"))?;
        let result = sync.sync(&local).await?;

        match result {
            SyncResult::Conflict {
                remote_meta,
                remote_ciphertext,
            } => {
                let remote_ct =
                    remote_ciphertext.context(self.i18n.tr("daemon_conflict_no_remote"))?;
                match choice {
                    ConflictChoice::Remote => {
                        // Real remote revision from the decrypted remote vault;
                        // a hardcoded 0 would make the next sync think the local
                        // side changed and re-upload.
                        let remote_vault = vida_core::vault::decrypt(&remote_ct, &passphrase)?;
                        let remote_revision = remote_vault.revision;
                        let new_vault = self
                            .sync
                            .as_mut()
                            .unwrap()
                            .resolve_conflict_remote(
                                &remote_ct,
                                &remote_meta,
                                &ct,
                                revision,
                                remote_revision,
                                &passphrase,
                            )
                            .await?;
                        self.vault = Some(new_vault);
                    }
                    ConflictChoice::Local => {
                        self.sync
                            .as_mut()
                            .unwrap()
                            .resolve_conflict_local(&ct, revision, &remote_ct, &remote_meta)
                            .await?;
                    }
                }
            }
            other => anyhow::bail!(
                "{}",
                self.i18n
                    .trf("daemon_conflict_none", &[&format!("{:?}", other)])
            ),
        }
        Ok((self.list_hosts()?,))
    }

    // Conflict files --------------------------------------------------------

    pub fn read_conflict_file(&self, path: &str) -> Result<Vec<HostSummary>> {
        let passphrase = self.ensure_passphrase()?;
        let data = std::fs::read(path).context("Failed to read conflict file")?;
        let vault = vida_core::vault::decrypt(&data, passphrase)?;
        Ok(vault.hosts.iter().map(host_to_summary).collect())
    }

    pub fn adopt_conflict_file(&mut self, path: &str) -> Result<Vec<HostSummary>> {
        let passphrase = self.ensure_passphrase()?.to_string();
        let vault_path = self.vault_path.clone();

        // 1. Read conflict file first (fail fast before any mutation)
        let conflict_data = std::fs::read(path).context("Failed to read conflict file")?;
        // Validate it's decryptable before proceeding
        vida_core::vault::decrypt(&conflict_data, &passphrase)?;

        // 2. Backup current vault to conflicts/ (must succeed before replace)
        let backup_dir = config::config_dir()?.join("conflicts");
        std::fs::create_dir_all(&backup_dir)?;
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let backup_path = backup_dir.join(format!("vault.adopted-{}.age", ts));
        if vault_path.exists() {
            let current = std::fs::read(&vault_path)?;
            std::fs::write(&backup_path, &current)?;
            let f = std::fs::File::open(&backup_path)?;
            vida_core::persist::full_fsync(&f)?;
        }

        // 3. Replace vault.age with conflict data (atomic)
        vida_core::persist::write_atomic(&vault_path, &conflict_data)?;

        // 4. Replace in-memory vault
        let new_vault = vida_core::vault::decrypt(&conflict_data, &passphrase)?;
        let hosts = new_vault.hosts.iter().map(host_to_summary).collect();
        self.vault = Some(new_vault);

        // 5. Rename conflict file to .reviewed
        let reviewed_path = format!("{}.reviewed", path);
        std::fs::rename(path, &reviewed_path)?;

        info!("Conflict file adopted: {}", path);
        Ok(hosts)
    }

    pub fn ignore_conflict_file(&self, path: &str) -> Result<()> {
        let reviewed_path = format!("{}.reviewed", path);
        std::fs::rename(path, &reviewed_path)?;
        info!("Conflict file ignored: {}", path);
        Ok(())
    }

    // Remote missing --------------------------------------------------------

    /// Decrypt remote ciphertext and return desensitized host summaries.
    /// Used by Sync handler to populate remote_hosts for Conflict branch.
    pub fn decrypt_remote_hosts(&self, remote_ct: &[u8]) -> Result<Vec<HostSummary>> {
        let passphrase = self.ensure_passphrase()?;
        let vault = vida_core::vault::decrypt(remote_ct, passphrase)?;
        Ok(vault.hosts.iter().map(host_to_summary).collect())
    }

    pub fn handle_remote_missing(&mut self, action: &str) -> Result<()> {
        match action {
            "reupload" => info!("Will reupload on next sync"),
            "clear_state" => {
                self.sync
                    .as_mut()
                    .context(self.i18n.tr("daemon_sync_not_initialized"))?
                    .clear_state()?;
                info!("Sync state cleared due to missing remote");
            }
            _ => anyhow::bail!("{}", self.i18n.trf("daemon_unknown_action", &[action])),
        }
        Ok(())
    }
}

fn host_to_summary(h: &vida_core::vault::HostEntry) -> HostSummary {
    HostSummary {
        id: h.id.clone(),
        name: h.name.clone(),
        host: h.host.clone(),
        user: h.user.clone(),
        port: h.port,
        tags: h.tags.clone(),
        group: h.group.clone(),
        color: h.color.clone(),
        auth_kind: match &h.auth {
            AuthMethod::Password { .. } => "password".to_string(),
            AuthMethod::Key { .. } => "key".to_string(),
            AuthMethod::KeyInline { .. } => "key_inline".to_string(),
        },
        notes: h.notes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sync_not_configured_returns_sync_not_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.age");

        let mut state = DaemonState {
            token: "test".into(),
            vault: None,
            passphrase: None,
            vault_path,
            sync: None,
            i18n: vida_core::i18n::I18n::new(vida_core::i18n::Lang::ZhCn),
        };

        let passphrase = "test-passphrase";
        state.create_vault(passphrase).unwrap();
        state.lock();
        state.unlock(passphrase, false).unwrap();

        assert!(state.sync.is_none());

        let (result, hosts) = state.sync().await.unwrap();
        assert!(matches!(result, SyncResult::SyncNotConfigured));
        assert!(hosts.is_none());
    }

    #[tokio::test]
    async fn path_change_clears_sync_state() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.age");

        // Create sync_state.json to simulate an established sync
        let config_dir = vida_core::config::config_dir().unwrap();
        let sync_state_file = config_dir.join("sync_state.json");
        let fake_state = vida_core::sync::SyncState {
            last_synced_revision: 5,
            last_synced_hash: "old-hash".into(),
            last_sync_time: 0,
        };
        std::fs::write(&sync_state_file, serde_json::to_vec(&fake_state).unwrap()).unwrap();
        assert!(sync_state_file.exists());

        let mut state = DaemonState {
            token: "test".into(),
            vault: None,
            passphrase: None,
            vault_path,
            sync: None,
            i18n: vida_core::i18n::I18n::new(vida_core::i18n::Lang::ZhCn),
        };
        state.create_vault("pass").unwrap();
        state.lock();
        state.unlock("pass", false).unwrap();

        // Set initial sync path
        let path_a = tmp.path().join("path_a");
        std::fs::create_dir_all(&path_a).unwrap();
        let mut settings = Settings {
            sync_local_path: Some(path_a.to_str().unwrap().to_string()),
            ..Default::default()
        };
        state.update_settings(settings.clone()).unwrap();

        // Verify sync_state.json was cleared when switching paths
        assert!(
            !sync_state_file.exists(),
            "sync_state.json should be deleted after path change"
        );

        // Switch to a new path — should also clear state
        let path_b = tmp.path().join("path_b");
        std::fs::create_dir_all(&path_b).unwrap();
        settings.sync_local_path = Some(path_b.to_str().unwrap().to_string());
        state.update_settings(settings).unwrap();
        assert!(
            !sync_state_file.exists(),
            "sync_state.json should still not exist after second path change"
        );
    }

    #[test]
    fn path_change_to_vault_dir_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.age");

        let mut state = DaemonState {
            token: "test".into(),
            vault: None,
            passphrase: None,
            vault_path,
            sync: None,
            i18n: vida_core::i18n::I18n::new(vida_core::i18n::Lang::ZhCn),
        };
        state.create_vault("pass").unwrap();
        state.lock();
        state.unlock("pass", false).unwrap();

        // Set sync_local_path to the vault directory itself
        let settings = Settings {
            sync_local_path: Some(tmp.path().to_str().unwrap().to_string()),
            ..Default::default()
        };

        let err = state.update_settings(settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("不能是金库所在目录"),
            "Should reject vault directory, got: {}",
            msg
        );
    }

    #[test]
    fn nonexistent_sync_path_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.age");

        let mut state = DaemonState {
            token: "test".into(),
            vault: None,
            passphrase: None,
            vault_path,
            sync: None,
            i18n: vida_core::i18n::I18n::new(vida_core::i18n::Lang::ZhCn),
        };
        state.create_vault("pass").unwrap();
        state.lock();
        state.unlock("pass", false).unwrap();

        let settings = Settings {
            sync_local_path: Some("/nonexistent/path/abc123".into()),
            ..Default::default()
        };

        let err = state.update_settings(settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("不存在"), "Should say '不存在', got: {}", msg);
        // 关键：不能误报为「不能是金库所在目录」
        assert!(
            !msg.contains("不能是金库所在目录"),
            "Must NOT say '不能是金库所在目录' for nonexistent path, got: {}",
            msg
        );
    }

    #[test]
    fn file_as_sync_path_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.age");
        let file_path = tmp.path().join("not_a_dir.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let mut state = DaemonState {
            token: "test".into(),
            vault: None,
            passphrase: None,
            vault_path,
            sync: None,
            i18n: vida_core::i18n::I18n::new(vida_core::i18n::Lang::ZhCn),
        };
        state.create_vault("pass").unwrap();
        state.lock();
        state.unlock("pass", false).unwrap();

        let settings = Settings {
            sync_local_path: Some(file_path.to_str().unwrap().to_string()),
            ..Default::default()
        };

        let err = state.update_settings(settings).unwrap_err();
        assert!(format!("{}", err).contains("不是文件夹"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_vault_dir_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let vault_path = tmp.path().join("vault.age");

        let mut state = DaemonState {
            token: "test".into(),
            vault: None,
            passphrase: None,
            vault_path,
            sync: None,
            i18n: vida_core::i18n::I18n::new(vida_core::i18n::Lang::ZhCn),
        };
        state.create_vault("pass").unwrap();
        state.lock();
        state.unlock("pass", false).unwrap();

        // Create a symlink pointing to the vault directory
        let symlink_path = tmp.path().join("sync_link");
        std::os::unix::fs::symlink(tmp.path(), &symlink_path).unwrap();

        let settings = Settings {
            sync_local_path: Some(symlink_path.to_str().unwrap().to_string()),
            ..Default::default()
        };

        let err = state.update_settings(settings).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("不能是金库所在目录"),
            "Symlink to vault dir should be rejected, got: {}",
            msg
        );
    }
}
