use crate::config;
use crate::persist;
use crate::vault::Vault;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::LazyLock;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Regex patterns for conflict file detection (compiled once)
// ---------------------------------------------------------------------------

static RE_DROPBOX_VERSION: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^vault \d+\.age$").unwrap());

static RE_GENERIC_CONFLICT: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^vault.*-conflict-.*\.age$").unwrap());

fn is_dropbox_version(name: &str) -> bool {
    RE_DROPBOX_VERSION.is_match(name)
}

fn is_generic_conflict(name: &str) -> bool {
    RE_GENERIC_CONFLICT.is_match(name)
}

fn is_dropbox_copy(name: &str) -> bool {
    name.contains("conflicted copy") || name.contains("\u{51b2}\u{7a81}\u{526f}\u{672c}")
}

fn is_syncthing_conflict(name: &str) -> bool {
    name.contains(".sync-conflict-")
}

// ---------------------------------------------------------------------------
// RemoteMeta — observable info only (no decryption needed)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteMeta {
    /// SHA-256 hex of ciphertext bytes
    pub bytes_hash: String,
    pub size: u64,
    pub modified_at: Option<i64>,
    /// S3 ETag; LocalPath is None
    pub etag: Option<String>,
}

// ---------------------------------------------------------------------------
// ConflictFile — detected cloud-service conflict files
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictFile {
    pub path: String,
    pub pattern: ConflictPattern,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConflictPattern {
    DropboxCopy,
    DropboxVersion,
    Syncthing,
    Generic,
    IcloudPlaceholder,
}

// ---------------------------------------------------------------------------
// ConflictInfo — shown to user in conflict dialog
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub remote_host_count: usize,
    pub remote_modified_at: Option<i64>,
    pub remote_device_id: String,
    pub remote_host_names: Vec<String>,
    pub local_host_count: usize,
    pub local_modified_at: Option<i64>,
    pub local_device_id: String,
    pub local_host_names: Vec<String>,
    pub only_local: Vec<String>,
    pub only_remote: Vec<String>,
    pub both: Vec<String>,
}

// ---------------------------------------------------------------------------
// SyncState — persisted to config dir, not in vault
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncState {
    pub last_synced_revision: u64,
    pub last_synced_hash: String,
    pub last_sync_time: i64,
}

fn sync_state_path() -> Result<PathBuf> {
    Ok(config::config_dir()?.join("sync_state.json"))
}

pub fn load_sync_state() -> Result<Option<SyncState>> {
    let path = sync_state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read(&path).context("Failed to read sync state")?;
    let state: SyncState = serde_json::from_slice(&data).context("Failed to parse sync state")?;
    Ok(Some(state))
}

pub fn save_sync_state(state: &SyncState) -> Result<()> {
    let path = sync_state_path()?;
    let data = serde_json::to_vec_pretty(state).context("Failed to serialize sync state")?;
    persist::write_atomic(&path, &data)
}

// ---------------------------------------------------------------------------
// LocalVaultInfo — passed to sync() to avoid unbounded parameter growth
// ---------------------------------------------------------------------------

pub struct LocalVaultInfo {
    pub ciphertext: Vec<u8>,
    pub revision: u64,
    pub device_id: String,
}

// ---------------------------------------------------------------------------
// SyncBackend trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait SyncBackend: Send + Sync {
    fn name(&self) -> &str;

    /// Upload ciphertext. If `expected` is Some, backend must verify
    /// remote still matches before writing (optimistic concurrency).
    /// Returns Conflict error if mismatch.
    async fn upload(
        &self,
        data: &[u8],
        path: &str,
        expected: Option<&RemoteMeta>,
    ) -> Result<RemoteMeta>;

    /// Download ciphertext + observable metadata. Does NOT decrypt.
    async fn download(&self, path: &str) -> Result<(Vec<u8>, RemoteMeta)>;

    /// Check if remote file exists (lightweight, no content read).
    async fn exists(&self, path: &str) -> Result<bool>;

    /// Scan for cloud-service conflict files in the same directory.
    async fn scan_conflict_files(&self, dir: &str) -> Result<Vec<ConflictFile>>;
}

// ---------------------------------------------------------------------------
// SyncResult
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SyncResult {
    /// Remote doesn't exist, uploaded local.
    ///
    /// # 调用方契约（daemon 层必须按顺序执行）
    ///
    /// 1. `new_meta` 已由 `sync()` 内部写入 SyncState，无需调用方操作
    /// 2. 通知 GUI 同步成功（可选）
    Uploaded { new_meta: RemoteMeta },

    /// Downloaded remote ciphertext.
    ///
    /// # 调用方契约（daemon 层必须按顺序执行，缺一不可）
    ///
    /// 1. 将 `ciphertext` 写入本地 `vault.age`（走原子写入路径）
    /// 2. 解密 `ciphertext` 得到 `Vault`
    /// 3. 替换 daemon 内存中的 Vault
    /// 4. 用 `meta` 更新 SyncState（`sync()` 未自动执行此步）
    /// 5. 通知 GUI 刷新主机列表
    ///
    /// 常见场景：在另一台设备编辑后回到本机同步。
    /// 若不写文件 → 下次同步重复下载；若不更新 SyncState → 状态永久不一致；
    /// 若不替换内存 → 界面显示旧数据，下次保存把旧数据写回覆盖远端。
    Downloaded {
        ciphertext: Vec<u8>,
        meta: RemoteMeta,
    },

    /// Both sides changed, or no state + remote exists.
    ///
    /// # 调用方契约（daemon 层必须按顺序执行）
    ///
    /// 1. 向 GUI 展示冲突解决界面，提供「使用本地 / 使用远端」选项
    /// 2. 用户选择后调用 `resolve_conflict_local()` 或 `resolve_conflict_remote()`
    /// 3. `resolve_conflict_remote` 的返回值（`Vault`）必须用于：
    ///    a. 替换 daemon 内存中的 Vault
    ///    b. 通知 GUI 刷新主机列表
    /// 4. `resolve_conflict_local` 无需额外内存操作（本地版本保留）
    ///
    /// `remote_ciphertext` 为 `None` 时，调用方必须拒绝解决并提示用户。
    Conflict {
        remote_meta: RemoteMeta,
        remote_ciphertext: Option<Vec<u8>>,
    },

    /// No changes on either side. 调用方无需操作。
    NoChange,

    /// Remote deleted / path changed, but local has sync record.
    ///
    /// # 调用方契约
    ///
    /// 向 UI 提供两个选项：
    /// 1. 「重新上传」— 用本地 vault 重新上传并更新 SyncState
    /// 2. 「清除同步状态」— 删除本地 SyncState，下次同步重新开始
    RemoteMissing,

    /// Cloud-service conflict files detected.
    ///
    /// # 调用方契约
    ///
    /// 1. 向 UI 展示冲突文件列表（路径 + 模式）
    /// 2. 提供「查看文件」按钮（调用系统文件管理器打开目录）
    /// 3. 提供「忽略」按钮（清除 SyncState，下次同步重新检测）
    ConflictFilesDetected { files: Vec<ConflictFile> },

    /// Sync not configured — `Settings.sync_local_path` is `None`.
    ///
    /// # 调用方契约
    ///
    /// 向 UI 返回未配置状态，不执行任何文件操作。
    /// GUI 应将同步按钮置为灰色，点击后跳转到设置的同步页。
    SyncNotConfigured,
}

// ---------------------------------------------------------------------------
// LocalPathBackend
// ---------------------------------------------------------------------------

pub struct LocalPathBackend {
    base_path: PathBuf,
}

impl LocalPathBackend {
    pub fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    fn vault_file(&self) -> PathBuf {
        self.base_path.join("vault.age")
    }
}

#[async_trait]
impl SyncBackend for LocalPathBackend {
    fn name(&self) -> &str {
        "local-path"
    }

    async fn upload(
        &self,
        data: &[u8],
        _path: &str,
        expected: Option<&RemoteMeta>,
    ) -> Result<RemoteMeta> {
        let target = self.vault_file();

        // Optimistic concurrency: check current state matches expected
        if let Some(exp) = expected
            && target.exists()
        {
            let current = std::fs::read(&target)
                .context("Failed to read remote file for concurrency check")?;
            let current_hash = sha256_hex(&current);
            if current_hash != exp.bytes_hash {
                anyhow::bail!(
                    "Conflict: remote file changed since last sync \
                         (expected hash {}, got {})",
                    exp.bytes_hash,
                    current_hash
                );
            }
        }

        // Atomic write: temp → full_fsync → rename → fsync parent
        let dir = self.base_path.clone();
        std::fs::create_dir_all(&dir)?;

        let temp_name = format!(".vault-sync-{}.tmp", uuid::Uuid::new_v4());
        let temp_path = dir.join(&temp_name);

        std::fs::write(&temp_path, data).context("Failed to write temp sync file")?;

        let f = std::fs::File::open(&temp_path)?;
        persist::full_fsync(&f)?;
        drop(f);

        std::fs::rename(&temp_path, &target).context("Failed to rename sync temp file")?;

        let dir_file = std::fs::File::open(&dir)?;
        persist::full_fsync(&dir_file)?;
        drop(dir_file);

        let hash = sha256_hex(data);
        let size = data.len() as u64;
        let modified_at = std::fs::metadata(&target)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);

        info!("Sync upload OK: {} ({} bytes)", target.display(), size);

        Ok(RemoteMeta {
            bytes_hash: hash,
            size,
            modified_at,
            etag: None,
        })
    }

    async fn download(&self, _path: &str) -> Result<(Vec<u8>, RemoteMeta)> {
        let target = self.vault_file();

        // iCloud placeholder check
        check_icloud_status(&self.base_path)?;

        if !target.exists() {
            anyhow::bail!(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Remote file not found: {}", target.display())
            ));
        }

        let data = std::fs::read(&target).context("Failed to read remote vault file")?;

        // Truncation check: age files have a minimum reasonable size
        if data.len() < 200 {
            anyhow::bail!(
                "Remote file appears truncated ({} bytes). \
                 File may be corrupted or still being written.",
                data.len()
            );
        }

        let hash = sha256_hex(&data);
        let size = data.len() as u64;
        let modified_at = std::fs::metadata(&target)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);

        info!("Sync download OK: {} ({} bytes)", target.display(), size);

        Ok((
            data,
            RemoteMeta {
                bytes_hash: hash,
                size,
                modified_at,
                etag: None,
            },
        ))
    }

    async fn exists(&self, _path: &str) -> Result<bool> {
        Ok(self.vault_file().exists())
    }

    async fn scan_conflict_files(&self, _dir: &str) -> Result<Vec<ConflictFile>> {
        let mut results = vec![];

        let read_dir =
            std::fs::read_dir(&self.base_path).context("Failed to scan for conflict files")?;

        for entry in read_dir {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();

            if is_dropbox_copy(&name) {
                results.push(ConflictFile {
                    path: entry.path().to_string_lossy().to_string(),
                    pattern: ConflictPattern::DropboxCopy,
                });
            } else if is_dropbox_version(&name) {
                results.push(ConflictFile {
                    path: entry.path().to_string_lossy().to_string(),
                    pattern: ConflictPattern::DropboxVersion,
                });
            } else if is_syncthing_conflict(&name) {
                results.push(ConflictFile {
                    path: entry.path().to_string_lossy().to_string(),
                    pattern: ConflictPattern::Syncthing,
                });
            } else if is_generic_conflict(&name) {
                results.push(ConflictFile {
                    path: entry.path().to_string_lossy().to_string(),
                    pattern: ConflictPattern::Generic,
                });
            }
        }

        // iCloud: .vault.age.icloud sibling
        let icloud_marker = self.base_path.join(".vault.age.icloud");
        if icloud_marker.exists() {
            results.push(ConflictFile {
                path: icloud_marker.to_string_lossy().to_string(),
                pattern: ConflictPattern::IcloudPlaceholder,
            });
        }

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// SyncCoordinator
// ---------------------------------------------------------------------------

pub struct SyncCoordinator {
    backend: Box<dyn SyncBackend>,
    state: Option<SyncState>,
    vault_path: PathBuf,
}

impl SyncCoordinator {
    pub fn new(backend: Box<dyn SyncBackend>, vault_path: PathBuf) -> Result<Self> {
        let state = load_sync_state()?;
        Ok(Self {
            backend,
            state,
            vault_path,
        })
    }

    /// Construct with explicit state (for testing).
    pub fn with_state(
        backend: Box<dyn SyncBackend>,
        vault_path: PathBuf,
        state: Option<SyncState>,
    ) -> Self {
        Self {
            backend,
            state,
            vault_path,
        }
    }

    /// Get the current sync state (for testing).
    pub fn state(&self) -> Option<&SyncState> {
        self.state.as_ref()
    }

    /// Run a sync cycle. `local` contains the current local vault info.
    pub async fn sync(&mut self, local: &LocalVaultInfo) -> Result<SyncResult> {
        // Scan for cloud-service conflict files first
        let dir = self
            .vault_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let conflicts = self
            .backend
            .scan_conflict_files(&dir.to_string_lossy())
            .await?;
        if !conflicts.is_empty() {
            warn!("Cloud-service conflict files detected: {}", conflicts.len());
            return Ok(SyncResult::ConflictFilesDetected { files: conflicts });
        }

        let vault_str = self.vault_path.to_string_lossy().to_string();

        // Download remote (if exists)
        let remote = match self.backend.download(&vault_str).await {
            Ok((data, meta)) => Some((data, meta)),
            Err(e) if is_not_found(&e) => None,
            Err(e) => return Err(e),
        };

        let remote_exists = remote.is_some();

        let remote_changed = match (&remote, &self.state) {
            (Some((_, meta)), Some(state)) => meta.bytes_hash != state.last_synced_hash,
            (Some(_), None) => true,
            (None, _) => false,
        };

        let local_changed = match &self.state {
            Some(state) => local.revision > state.last_synced_revision,
            None => true, // no state → treat as changed
        };

        // Decision table
        debug!(
            "Sync decision: remote_exists={} remote_changed={} local_changed={}",
            remote_exists, remote_changed, local_changed
        );
        match (remote_exists, remote_changed, local_changed) {
            // Remote doesn't exist + local changed → upload
            (false, _, true) => {
                let meta = self
                    .backend
                    .upload(&local.ciphertext, &vault_str, None)
                    .await?;
                self.update_state(local.revision, &meta.bytes_hash);
                Ok(SyncResult::Uploaded { new_meta: meta })
            }

            // Remote doesn't exist + local not changed + has state
            // → remote was deleted or path changed
            (false, _, false) => {
                // SAFETY: local_changed is true when state is None,
                // so reaching here means state.is_some()
                Ok(SyncResult::RemoteMissing)
            }

            // Remote unchanged + local changed → upload (with optimistic concurrency)
            (true, false, true) => {
                let expected = remote.map(|(_, m)| m);
                let meta = self
                    .backend
                    .upload(&local.ciphertext, &vault_str, expected.as_ref())
                    .await?;
                self.update_state(local.revision, &meta.bytes_hash);
                Ok(SyncResult::Uploaded { new_meta: meta })
            }

            // Remote unchanged + local unchanged → nothing to do
            (true, false, false) => Ok(SyncResult::NoChange),

            // Remote changed + local unchanged → download
            (true, true, false) => {
                let (data, meta) = remote.unwrap();
                Ok(SyncResult::Downloaded {
                    ciphertext: data,
                    meta,
                })
            }

            // Remote changed + local changed → conflict
            (true, true, true) => {
                let (data, meta) = remote.unwrap();
                Ok(SyncResult::Conflict {
                    remote_meta: meta,
                    remote_ciphertext: Some(data),
                })
            }
        }
    }

    /// Resolve conflict by choosing "remote" (discard local).
    ///
    /// Steps:
    /// 1. Backup local ciphertext to conflicts/vault.local-{ts}-rev{n}.age
    /// 2. full_fsync backup file + parent dir (fail = abort, no overwrite)
    /// 3. Write remote ciphertext to local vault.age (atomic write path)
    /// 4. Decrypt and replace in-memory Vault
    /// 5. Update SyncState from remote_meta
    pub async fn resolve_conflict_remote(
        &mut self,
        remote_ciphertext: &[u8],
        remote_meta: &RemoteMeta,
        local_ciphertext: &[u8],
        local_rev: u64,
        remote_rev: u64,
        passphrase: &str,
    ) -> Result<Vault> {
        let backup_dir = config::config_dir()?.join("conflicts");
        std::fs::create_dir_all(&backup_dir)?;

        // Step 1: Backup local ciphertext
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let backup_path = backup_dir.join(format!("vault.local-{}-rev{}.age", ts, local_rev));
        std::fs::write(&backup_path, local_ciphertext)
            .context("Failed to write conflict backup")?;

        // Step 2: full_fsync backup (fail = abort, no overwrite)
        {
            let f = std::fs::File::open(&backup_path)?;
            persist::full_fsync(&f)?;
            drop(f);
            let d = std::fs::File::open(&backup_dir)?;
            persist::full_fsync(&d)?;
            drop(d);
        }

        // Step 3: Write remote ciphertext to local vault.age (atomic)
        persist::write_atomic(&self.vault_path, remote_ciphertext)?;

        // Step 4: Decrypt and return
        let vault = crate::vault::decrypt(remote_ciphertext, passphrase)?;

        // Step 5: Update SyncState
        let new_state = SyncState {
            last_synced_revision: remote_rev,
            last_synced_hash: remote_meta.bytes_hash.clone(),
            last_sync_time: now_unix(),
        };
        save_sync_state(&new_state)?;
        self.state = Some(new_state);

        Ok(vault)
    }

    /// Resolve conflict by choosing "local" (discard remote).
    ///
    /// Steps:
    /// 1. Backup remote ciphertext to conflicts/vault.remote-{ts}.age
    /// 2. full_fsync backup
    /// 3. Upload local with optimistic concurrency (expected = remote_meta)
    /// 4. Update SyncState
    pub async fn resolve_conflict_local(
        &mut self,
        local_ciphertext: &[u8],
        local_rev: u64,
        remote_ciphertext: &[u8],
        remote_meta: &RemoteMeta,
    ) -> Result<()> {
        let backup_dir = config::config_dir()?.join("conflicts");
        std::fs::create_dir_all(&backup_dir)?;

        // Step 1: Backup remote ciphertext
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let backup_path = backup_dir.join(format!("vault.remote-{}.age", ts));
        std::fs::write(&backup_path, remote_ciphertext)
            .context("Failed to write conflict backup")?;

        // Step 2: full_fsync backup
        {
            let f = std::fs::File::open(&backup_path)?;
            persist::full_fsync(&f)?;
            drop(f);
            let d = std::fs::File::open(&backup_dir)?;
            persist::full_fsync(&d)?;
            drop(d);
        }

        // Step 3: Upload local with optimistic concurrency
        let vault_str = self.vault_path.to_string_lossy().to_string();
        let new_meta = self
            .backend
            .upload(local_ciphertext, &vault_str, Some(remote_meta))
            .await?;

        // Step 4: Update SyncState
        let new_state = SyncState {
            last_synced_revision: local_rev,
            last_synced_hash: new_meta.bytes_hash.clone(),
            last_sync_time: now_unix(),
        };
        save_sync_state(&new_state)?;
        self.state = Some(new_state);

        Ok(())
    }

    /// Update sync state after a successful download.
    /// Must be called by daemon after writing downloaded ciphertext to disk.
    pub fn update_state_after_download(&mut self, meta: &RemoteMeta, new_revision: u64) {
        self.state = Some(SyncState {
            last_synced_revision: new_revision,
            last_synced_hash: meta.bytes_hash.clone(),
            last_sync_time: now_unix(),
        });
        let _ = save_sync_state(self.state.as_ref().unwrap());
    }

    /// Clear sync state (used when conflict files detected or remote missing).
    pub fn clear_state(&mut self) -> Result<()> {
        self.state = None;
        let path = sync_state_path()?;
        if path.exists() {
            std::fs::remove_file(&path).context("Failed to remove sync state file")?;
        }
        Ok(())
    }

    fn update_state(&mut self, revision: u64, hash: &str) {
        self.state = Some(SyncState {
            last_synced_revision: revision,
            last_synced_hash: hash.to_owned(),
            last_sync_time: now_unix(),
        });
        let _ = save_sync_state(self.state.as_ref().unwrap());
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn check_icloud_status(dir: &std::path::Path) -> Result<()> {
    let icloud_marker = dir.join(".vault.age.icloud");
    if icloud_marker.exists() {
        #[cfg(target_os = "macos")]
        {
            // Trigger iCloud download
            use std::process::Command;
            let _ = Command::new("brctl")
                .args(["download", &dir.to_string_lossy()])
                .output();

            // Wait with tokio (async-safe)
            let rt = tokio::runtime::Handle::current();
            for _ in 0..30 {
                rt.block_on(tokio::time::sleep(std::time::Duration::from_secs(1)));
                if !icloud_marker.exists() {
                    return Ok(());
                }
            }
            anyhow::bail!(
                "iCloud 文件尚未下载完成。请检查网络连接后重试。\
                 如果问题持续，请在 Finder 中右键点击文件选择「立即下载」。"
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            anyhow::bail!(
                "发现 iCloud 占位文件 (.vault.age.icloud)，\
                 文件尚未从 iCloud 下载完成。请等待或手动触发下载后重试。"
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Conflict info builder
// ---------------------------------------------------------------------------

pub fn build_conflict_info(
    local: &Vault,
    remote: &Vault,
    local_meta: &RemoteMeta,
    remote_meta: &RemoteMeta,
) -> ConflictInfo {
    let local_names: Vec<String> = local.hosts.iter().map(|h| h.name.clone()).collect();
    let remote_names: Vec<String> = remote.hosts.iter().map(|h| h.name.clone()).collect();

    let only_local: Vec<String> = local_names
        .iter()
        .filter(|n| !remote_names.contains(n))
        .cloned()
        .collect();
    let only_remote: Vec<String> = remote_names
        .iter()
        .filter(|n| !local_names.contains(n))
        .cloned()
        .collect();
    let both: Vec<String> = local_names
        .iter()
        .filter(|n| remote_names.contains(n))
        .cloned()
        .collect();

    ConflictInfo {
        remote_host_count: remote.hosts.len(),
        remote_modified_at: remote_meta.modified_at,
        remote_device_id: remote.device_id.clone(),
        remote_host_names: remote_names,
        local_host_count: local.hosts.len(),
        local_modified_at: local_meta.modified_at,
        local_device_id: local.device_id.clone(),
        local_host_names: local_names,
        only_local,
        only_remote,
        both,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{AuthMethod, CURRENT_VAULT_VERSION, HostEntry, SecureString, Settings};

    fn test_vault(name: &str, host_count: usize) -> Vault {
        let hosts = (0..host_count)
            .map(|i| HostEntry {
                id: uuid::Uuid::new_v4().to_string(),
                name: format!("{}-{}", name, i),
                host: format!("10.0.0.{}", i),
                user: "root".into(),
                port: 22,
                tags: vec![],
                group: None,
                color: None,
                auth: AuthMethod::Password {
                    password: SecureString::new(format!("pass-{}", i)),
                },
                notes: None,
            })
            .collect();
        Vault {
            version: CURRENT_VAULT_VERSION,
            revision: 1,
            device_id: uuid::Uuid::new_v4().to_string(),
            modified_at: now_unix(),
            hosts,
            settings: Settings::default(),
        }
    }

    fn encrypt_vault(vault: &Vault, passphrase: &str) -> Vec<u8> {
        crate::vault::encrypt(vault, passphrase).unwrap()
    }

    // -----------------------------------------------------------------------
    // Pattern tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_dropbox_version_pattern() {
        assert!(is_dropbox_version("vault 2.age"));
        assert!(is_dropbox_version("vault 10.age"));
        assert!(!is_dropbox_version("vault backup.age"));
        assert!(!is_dropbox_version("vault old.age"));
        assert!(!is_dropbox_version("vault.age"));
        assert!(!is_dropbox_version("Vault 2.age"));
    }

    #[test]
    fn test_generic_conflict_pattern() {
        assert!(is_generic_conflict("vault-abcd-conflict-1234.age"));
        assert!(is_generic_conflict("vault-conflict-20260801.age"));
        assert!(!is_generic_conflict("my-server-conflict-backup.age"));
        assert!(!is_generic_conflict("vault.age"));
        assert!(!is_generic_conflict("vault 2.age"));
    }

    #[test]
    fn test_dropbox_copy_pattern() {
        // Standard Dropbox conflicted copy
        assert!(is_dropbox_copy("vault (conflicted copy 2026-08-01).age"));
        // With username
        assert!(is_dropbox_copy(
            "vault (Jane's conflicted copy 2026-08-01).age"
        ));
        // Chinese macOS
        assert!(is_dropbox_copy(
            "vault (MacBook Pro \u{7684}\u{51b2}\u{7a81}\u{526f}\u{672c} 2026-08-01).age"
        ));
        // Should not match
        assert!(!is_dropbox_copy("vault.age"));
        assert!(!is_dropbox_copy("vault-conflict-notes.age"));
    }

    #[test]
    fn test_syncthing_pattern() {
        assert!(is_syncthing_conflict(
            "vault.age.sync-conflict-20260801-123456"
        ));
        assert!(!is_syncthing_conflict("vault.age"));
    }

    // -----------------------------------------------------------------------
    // Sync decision table tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_upload_new_remote() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalPathBackend::new(dir.path().to_path_buf());
        let vault_path = dir.path().join("vault.age");

        // Local has content, remote empty
        let vault = test_vault("upload-new", 2);
        let ciphertext = encrypt_vault(&vault, "pass");

        let local = LocalVaultInfo {
            ciphertext: ciphertext.clone(),
            revision: 1,
            device_id: "local-device".into(),
        };

        let mut coord = SyncCoordinator::with_state(Box::new(backend), vault_path.clone(), None);

        let result = tokio_test::block_on(coord.sync(&local)).unwrap();
        assert!(matches!(result, SyncResult::Uploaded { .. }));
        assert!(vault_path.exists());
        assert_eq!(std::fs::read(&vault_path).unwrap(), ciphertext);
    }

    #[test]
    fn test_no_change() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalPathBackend::new(dir.path().to_path_buf());
        let vault_path = dir.path().join("vault.age");

        let vault = test_vault("no-change", 1);
        let ciphertext = encrypt_vault(&vault, "pass");

        // First upload
        let local = LocalVaultInfo {
            ciphertext: ciphertext.clone(),
            revision: 1,
            device_id: "local-device".into(),
        };
        let mut coord = SyncCoordinator::with_state(Box::new(backend), vault_path.clone(), None);
        tokio_test::block_on(coord.sync(&local)).unwrap();

        // Same revision again
        let local2 = LocalVaultInfo {
            ciphertext: ciphertext.clone(),
            revision: 1,
            device_id: "local-device".into(),
        };
        let result = tokio_test::block_on(coord.sync(&local2)).unwrap();
        assert!(matches!(result, SyncResult::NoChange));
    }

    #[test]
    fn test_download_remote_change() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalPathBackend::new(dir.path().to_path_buf());
        let vault_path = dir.path().join("vault.age");

        // Upload v1
        let vault1 = test_vault("v1", 1);
        let ct1 = encrypt_vault(&vault1, "pass");
        let local1 = LocalVaultInfo {
            ciphertext: ct1.clone(),
            revision: 1,
            device_id: "device-a".into(),
        };
        let mut coord = SyncCoordinator::with_state(Box::new(backend), vault_path.clone(), None);
        tokio_test::block_on(coord.sync(&local1)).unwrap();

        // Manually overwrite remote with v2 (simulating another device)
        let vault2 = test_vault("v2", 2);
        let ct2 = encrypt_vault(&vault2, "pass");
        std::fs::write(&vault_path, &ct2).unwrap();

        // Sync with same local revision → should download
        let local2 = LocalVaultInfo {
            ciphertext: ct1.clone(),
            revision: 1,
            device_id: "device-a".into(),
        };
        let result = tokio_test::block_on(coord.sync(&local2)).unwrap();
        match result {
            SyncResult::Downloaded { ciphertext, .. } => {
                assert_eq!(ciphertext, ct2);
            }
            other => panic!("expected Downloaded, got {:?}", other),
        }
    }

    #[test]
    fn test_conflict_both_changed() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalPathBackend::new(dir.path().to_path_buf());
        let vault_path = dir.path().join("vault.age");

        // Upload v1
        let vault1 = test_vault("v1", 1);
        let ct1 = encrypt_vault(&vault1, "pass");
        let local1 = LocalVaultInfo {
            ciphertext: ct1.clone(),
            revision: 1,
            device_id: "device-a".into(),
        };
        let mut coord = SyncCoordinator::with_state(Box::new(backend), vault_path.clone(), None);
        tokio_test::block_on(coord.sync(&local1)).unwrap();

        // Remote changed
        let vault2r = test_vault("remote-v2", 3);
        let ct2r = encrypt_vault(&vault2r, "pass");
        std::fs::write(&vault_path, &ct2r).unwrap();

        // Local also changed
        let vault2l = test_vault("local-v2", 2);
        let ct2l = encrypt_vault(&vault2l, "pass");
        let local2 = LocalVaultInfo {
            ciphertext: ct2l,
            revision: 2,
            device_id: "device-a".into(),
        };

        let result = tokio_test::block_on(coord.sync(&local2)).unwrap();
        match result {
            SyncResult::Conflict {
                remote_ciphertext, ..
            } => {
                assert!(remote_ciphertext.is_some());
                assert_eq!(remote_ciphertext.unwrap(), ct2r);
            }
            other => panic!("expected Conflict, got {:?}", other),
        }
    }

    #[test]
    fn test_remote_missing() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalPathBackend::new(dir.path().to_path_buf());
        let vault_path = dir.path().join("vault.age");

        // Upload v1
        let vault1 = test_vault("v1", 1);
        let ct1 = encrypt_vault(&vault1, "pass");
        let local1 = LocalVaultInfo {
            ciphertext: ct1.clone(),
            revision: 1,
            device_id: "device-a".into(),
        };
        let mut coord = SyncCoordinator::with_state(Box::new(backend), vault_path.clone(), None);
        tokio_test::block_on(coord.sync(&local1)).unwrap();

        // Delete remote
        std::fs::remove_file(&vault_path).unwrap();

        // Sync same revision → RemoteMissing
        let local2 = LocalVaultInfo {
            ciphertext: ct1,
            revision: 1,
            device_id: "device-a".into(),
        };
        let result = tokio_test::block_on(coord.sync(&local2)).unwrap();
        assert!(matches!(result, SyncResult::RemoteMissing));
    }

    #[test]
    fn test_conflict_no_state_remote_exists() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalPathBackend::new(dir.path().to_path_buf());
        let vault_path = dir.path().join("vault.age");

        // Remote has a file but no sync state
        let vault = test_vault("remote", 2);
        let ct = encrypt_vault(&vault, "pass");
        std::fs::write(&vault_path, &ct).unwrap();

        let local = LocalVaultInfo {
            ciphertext: ct.clone(),
            revision: 1,
            device_id: "local-device".into(),
        };
        let mut coord = SyncCoordinator::with_state(Box::new(backend), vault_path.clone(), None);

        let result = tokio_test::block_on(coord.sync(&local)).unwrap();
        assert!(matches!(result, SyncResult::Conflict { .. }));
    }

    // -----------------------------------------------------------------------
    // Resolve conflict tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_remote_replaces_local() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalPathBackend::new(dir.path().to_path_buf());
        let vault_path = dir.path().join("vault.age");

        // Setup: upload local v1
        let vault_local = test_vault("local", 2);
        let ct_local = encrypt_vault(&vault_local, "pass");
        let local = LocalVaultInfo {
            ciphertext: ct_local.clone(),
            revision: 1,
            device_id: "device-local".into(),
        };
        let mut coord = SyncCoordinator::with_state(Box::new(backend), vault_path.clone(), None);
        tokio_test::block_on(coord.sync(&local)).unwrap();

        // Simulate remote v2
        let vault_remote = test_vault("remote", 3);
        let ct_remote = encrypt_vault(&vault_remote, "pass");
        let remote_meta = RemoteMeta {
            bytes_hash: sha256_hex(&ct_remote),
            size: ct_remote.len() as u64,
            modified_at: Some(now_unix()),
            etag: None,
        };

        // Resolve choosing remote
        let result = coord.resolve_conflict_remote(
            &ct_remote,
            &remote_meta,
            &ct_local,
            1,
            2, // remote_rev
            "pass",
        );
        let returned_vault = tokio_test::block_on(result).unwrap();

        // Assert 1: local file = remote ciphertext
        let local_file = std::fs::read(&vault_path).unwrap();
        assert_eq!(local_file, ct_remote);

        // Assert 2: backup exists and is decryptable
        let _conflicts_dir = dir.path().parent().unwrap().join("conflicts");
        // The config dir is derived from config::config_dir() which uses
        // VIDA_CONFIG_DIR env var in tests, so we check there
        let backup_dir = config::config_dir().unwrap().join("conflicts");
        let mut entries: Vec<_> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("vault.local-"))
            .collect();
        // Sort by modification time descending to get the most recent backup
        entries.sort_by(|a, b| {
            let t_a = a
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let t_b = b
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            t_b.cmp(&t_a)
        });
        assert!(!entries.is_empty(), "local backup should exist");

        let backup_data = std::fs::read(entries[0].path()).unwrap();
        let decrypted = crate::vault::decrypt(&backup_data, "pass").unwrap();
        assert_eq!(decrypted.hosts.len(), 2);
        assert_eq!(decrypted.hosts[0].name, "local-0");

        // Assert 3: returned vault matches remote
        assert_eq!(returned_vault.hosts.len(), 3);
        assert_eq!(returned_vault.hosts[0].name, "remote-0");

        // Assert 4: subsequent sync returns NoChange
        let local2 = LocalVaultInfo {
            ciphertext: ct_remote.clone(),
            revision: 2, // remote_meta.revision
            device_id: "device-remote".into(),
        };
        let result2 = tokio_test::block_on(coord.sync(&local2)).unwrap();
        assert!(
            matches!(result2, SyncResult::NoChange),
            "subsequent sync should be NoChange, got {:?}",
            result2
        );
    }

    #[test]
    fn test_resolve_local_uploads_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalPathBackend::new(dir.path().to_path_buf());
        let vault_path = dir.path().join("vault.age");

        // Setup: upload v1
        let vault_v1 = test_vault("v1", 1);
        let ct_v1 = encrypt_vault(&vault_v1, "pass");
        let local = LocalVaultInfo {
            ciphertext: ct_v1.clone(),
            revision: 1,
            device_id: "device-a".into(),
        };
        let mut coord = SyncCoordinator::with_state(Box::new(backend), vault_path.clone(), None);
        tokio_test::block_on(coord.sync(&local)).unwrap();

        // Remote changed
        let vault_remote = test_vault("remote", 3);
        let ct_remote = encrypt_vault(&vault_remote, "pass");
        std::fs::write(&vault_path, &ct_remote).unwrap();
        let remote_meta = RemoteMeta {
            bytes_hash: sha256_hex(&ct_remote),
            size: ct_remote.len() as u64,
            modified_at: Some(now_unix()),
            etag: None,
        };

        // Local also changed
        let vault_local2 = test_vault("local-v2", 2);
        let ct_local2 = encrypt_vault(&vault_local2, "pass");

        // Resolve choosing local
        tokio_test::block_on(coord.resolve_conflict_local(&ct_local2, 2, &ct_remote, &remote_meta))
            .unwrap();

        // Assert: local file = local v2
        let local_file = std::fs::read(&vault_path).unwrap();
        assert_eq!(local_file, ct_local2);

        // Assert: remote backup exists
        let backup_dir = config::config_dir().unwrap().join("conflicts");
        let entries: Vec<_> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("vault.remote-"))
            .collect();
        assert!(!entries.is_empty(), "remote backup should exist");
    }

    // -----------------------------------------------------------------------
    // Conflict file scanning tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_scan_conflict_files() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalPathBackend::new(dir.path().to_path_buf());

        // Create various conflict files
        std::fs::write(
            dir.path().join("vault (conflicted copy 2026-08-01).age"),
            b"",
        )
        .unwrap();
        std::fs::write(dir.path().join("vault 2.age"), b"").unwrap();
        std::fs::write(
            dir.path().join("vault.age.sync-conflict-20260801-123456"),
            b"",
        )
        .unwrap();
        std::fs::write(dir.path().join("vault-abcd-conflict-1234.age"), b"").unwrap();

        // Non-conflict files
        std::fs::write(dir.path().join("vault.age"), b"main").unwrap();
        std::fs::write(dir.path().join("vault backup.age"), b"").unwrap();

        let conflicts =
            tokio_test::block_on(backend.scan_conflict_files(&dir.path().to_string_lossy()))
                .unwrap();

        assert_eq!(conflicts.len(), 4, "should find 4 conflict files");

        let patterns: Vec<_> = conflicts.iter().map(|c| &c.pattern).collect();
        assert!(
            patterns
                .iter()
                .any(|p| matches!(p, ConflictPattern::DropboxCopy))
        );
        assert!(
            patterns
                .iter()
                .any(|p| matches!(p, ConflictPattern::DropboxVersion))
        );
        assert!(
            patterns
                .iter()
                .any(|p| matches!(p, ConflictPattern::Syncthing))
        );
        assert!(
            patterns
                .iter()
                .any(|p| matches!(p, ConflictPattern::Generic))
        );
    }
}
