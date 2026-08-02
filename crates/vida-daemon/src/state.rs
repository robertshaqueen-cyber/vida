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
        let backend = Box::new(LocalPathBackend::new(
            self.vault_path.parent().unwrap().to_path_buf(),
        ));
        self.sync = Some(SyncCoordinator::new(backend, self.vault_path.clone())?);
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
        vault.settings = settings;
        vault.modified_at = chrono::Utc::now().timestamp();
        vault.revision += 1;
        let ct = vida_core::vault::encrypt(vault, &passphrase)?;
        vida_core::persist::write_atomic(&vault_path, &ct)?;
        info!("Settings updated");
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

        let sync = self
            .sync
            .as_mut()
            .context(self.i18n.tr("daemon_sync_not_initialized"))?;
        let result = sync.sync(&local).await?;

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
