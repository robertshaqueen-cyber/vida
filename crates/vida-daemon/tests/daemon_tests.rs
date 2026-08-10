use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use vida_core::sync::{LocalPathBackend, SyncCoordinator, SyncResult};
use vida_daemon::protocol::{ConflictChoice, HostAuthRequest, HostRequest};
use vida_daemon::state::DaemonState;

/// Serialize tests that modify VIDA_CONFIG_DIR env var
/// to prevent parallel test interference.
static ENV_MUTEX: StdMutex<()> = StdMutex::new(());

fn test_state(token: &str) -> (DaemonState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let vault_path = dir.path().join("vault.age");
    let state = DaemonState {
        token: token.to_string(),
        vault: None,
        passphrase: None,
        vault_path,
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    (state, dir)
}

fn add_test_host(state: &mut DaemonState, name: &str) {
    state
        .update_host(HostRequest {
            id: None,
            name: name.to_string(),
            host: "10.0.0.1".to_string(),
            user: "root".to_string(),
            port: 22,
            tags: vec![],
            group: None,
            color: None,
            password: Some("secret-password-123".to_string()),
            auth: None,
            notes: None,
        })
        .unwrap();
}

/// Start the real daemon server, return (addr, token, tempdir).
async fn start_daemon() -> (std::net::SocketAddr, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let vault_path = dir.path().join("vault.age");
    let token = "test-token-abc123".to_string();

    let state = DaemonState {
        token: token.clone(),
        vault: None,
        passphrase: None,
        vault_path,
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };

    let state = Arc::new(Mutex::new(state));
    let addr = vida_daemon::ws_server::start(state).await.unwrap();
    (addr, token, dir)
}

async fn connect(
    addr: std::net::SocketAddr,
) -> (
    futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
        .await
        .unwrap();
    ws.split()
}

async fn send_recv(
    ws: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    reader: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    msg: &str,
) -> serde_json::Value {
    use tokio_tungstenite::tungstenite::Message;
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .unwrap();
    recv_text(reader).await
}

async fn recv_text(
    reader: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) -> serde_json::Value {
    // 跳过二进制推送帧，返回第一个文本响应或事件。
    loop {
        match reader.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).unwrap();
            }
            Some(Ok(Message::Binary(_))) => continue, // 跳过推送帧
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("read error: {:?}", e),
            None => panic!("connection closed"),
        }
    }
}

// -----------------------------------------------------------------------
// WebSocket auth behavior tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn ws_no_auth_then_list_hosts_rejected() {
    let (addr, _token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    let resp = send_recv(&mut ws, &mut reader, r#"{"method":"ListHosts","id":1}"#).await;

    // ResponsePayload::Error with #[serde(tag="type")] flattens to:
    // {"id":1,"type":"Error","code":-2,"message":"未认证：首条消息必须是 auth"}
    assert_eq!(
        resp["code"], -2,
        "should return unauthenticated error: {}",
        resp
    );
    let resp_str = resp.to_string();
    assert!(
        !resp_str.contains("10.0.0.1"),
        "must not leak host data: {}",
        resp_str
    );
}

#[tokio::test]
async fn ws_wrong_token_rejected() {
    let (addr, _token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    let resp = send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"Auth","params":{"token":"wrong-token"},"id":1}"#,
    )
    .await;

    assert_eq!(resp["code"], -3, "should return token mismatch: {}", resp);
}

#[tokio::test]
async fn ws_correct_token_allows_list_hosts() {
    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    let auth_msg = format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
        token
    );
    let resp = send_recv(&mut ws, &mut reader, &auth_msg).await;
    assert!(resp["result"]["authenticated"].as_bool().unwrap());

    // Create vault so list_hosts won't fail with "vault locked"
    let resp = send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"CreateVault","params":{"passphrase":"test-pass"},"id":2}"#,
    )
    .await;
    assert!(
        resp["result"].is_object(),
        "CreateVault should succeed: {}",
        resp
    );

    let resp = send_recv(&mut ws, &mut reader, r#"{"method":"ListHosts","id":3}"#).await;
    assert!(
        resp["result"].is_array(),
        "should return host list: {}",
        resp
    );
}

#[tokio::test]
async fn ws_origin_header_rejected() {
    let (addr, token, _dir) = start_daemon().await;

    // Case 1: Origin + correct token (as query param) → handshake must fail
    // This proves Origin blocks at HTTP level BEFORE any WebSocket message (auth) is sent
    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(format!("ws://{}?token={}", addr, token))
        .header("Origin", "http://evil.com")
        .body(())
        .unwrap();
    let result = tokio_tungstenite::connect_async(request).await;
    assert!(
        result.is_err(),
        "handshake with Origin + correct token must be rejected at HTTP level, got: {:?}",
        result.ok()
    );

    // Case 2: Origin alone (no token) → same rejection
    let request2 = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(format!("ws://{}", addr))
        .header("Origin", "http://evil.com")
        .body(())
        .unwrap();
    let result2 = tokio_tungstenite::connect_async(request2).await;
    assert!(
        result2.is_err(),
        "handshake with Origin alone must also be rejected"
    );
}

// -----------------------------------------------------------------------
// ConflictFilesDetected integration test
// -----------------------------------------------------------------------

#[test]
fn conflict_files_detected_on_sync() {
    let (mut state, dir) = test_state("tok");
    state.create_vault("pass").unwrap();

    // First sync to establish state
    let ct = vida_core::vault::encrypt(state.vault.as_ref().unwrap(), "pass").unwrap();
    let local = vida_core::sync::LocalVaultInfo {
        ciphertext: ct,
        revision: 1,
        device_id: "test".to_string(),
    };
    let backend = Box::new(LocalPathBackend::new(dir.path().to_path_buf()));
    let mut sync = SyncCoordinator::with_state(backend, state.vault_path.clone(), None);
    tokio_test::block_on(sync.sync(&local)).unwrap();

    // Create a Dropbox-style conflict file
    let conflict_path = dir.path().join("vault 2.age");
    std::fs::write(&conflict_path, b"fake-conflict-data").unwrap();

    // Get current state and create new coordinator
    let saved_state = sync.state().cloned();
    let backend2 = Box::new(LocalPathBackend::new(dir.path().to_path_buf()));
    let mut sync2 = SyncCoordinator::with_state(backend2, state.vault_path.clone(), saved_state);
    let ct2 = vida_core::vault::encrypt(state.vault.as_ref().unwrap(), "pass").unwrap();
    let local2 = vida_core::sync::LocalVaultInfo {
        ciphertext: ct2,
        revision: 1,
        device_id: "test".to_string(),
    };
    let result = tokio_test::block_on(sync2.sync(&local2)).unwrap();

    match result {
        SyncResult::ConflictFilesDetected { files } => {
            assert!(files.iter().any(|f| f.path.contains("vault 2.age")));
        }
        other => panic!("expected ConflictFilesDetected, got {:?}", other),
    }
}

#[test]
fn no_false_positive_on_user_backup_file() {
    let (mut state, dir) = test_state("tok");
    state.create_vault("pass").unwrap();

    let backup_path = dir.path().join("vault backup.age");
    std::fs::write(&backup_path, b"user-backup").unwrap();

    let backend = Box::new(LocalPathBackend::new(dir.path().to_path_buf()));
    let mut sync = SyncCoordinator::with_state(backend, state.vault_path.clone(), None);
    let ct = vida_core::vault::encrypt(state.vault.as_ref().unwrap(), "pass").unwrap();
    let local = vida_core::sync::LocalVaultInfo {
        ciphertext: ct,
        revision: 1,
        device_id: "test".to_string(),
    };
    let result = tokio_test::block_on(sync.sync(&local)).unwrap();

    assert!(
        !matches!(result, SyncResult::ConflictFilesDetected { .. }),
        "should not trigger on user backup file"
    );
}

// -----------------------------------------------------------------------
// RemoteMissing integration test
// -----------------------------------------------------------------------

#[test]
fn remote_missing_after_delete() {
    let dir = tempfile::tempdir().unwrap();

    let mut state = DaemonState {
        token: "t".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state.vault = Some(vida_core::vault::Vault::default());
    state.passphrase = Some("pass".to_string());
    let backend = Box::new(LocalPathBackend::new(dir.path().to_path_buf()));
    state.sync = Some(SyncCoordinator::with_state(
        backend,
        state.vault_path.clone(),
        None,
    ));

    // Add a host directly in memory (no disk write)
    {
        let v = state.vault.as_mut().unwrap();
        v.hosts.push(vida_core::vault::HostEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name: "host-a".to_string(),
            host: "10.0.0.1".to_string(),
            user: "root".to_string(),
            port: 22,
            tags: vec![],
            group: None,
            color: None,
            auth: vida_core::vault::AuthMethod::Password {
                password: vida_core::vault::SecureString::new("pass".to_string()),
            },
            notes: None,
            agent_trust: vida_core::agent_policy::AgentTrust::Ask,
        });
        v.revision += 1;
    }

    // First sync: no remote file → Uploaded
    let (result, _) = tokio_test::block_on(state.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::Uploaded { .. }),
        "expected Uploaded, got {:?}",
        result
    );

    // Delete the vault file (simulate remote deletion)
    std::fs::remove_file(&state.vault_path).unwrap();

    // Don't change local vault — only remote is missing
    // Sync → should detect remote missing
    let (result, _) = tokio_test::block_on(state.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::RemoteMissing),
        "expected RemoteMissing, got {:?}",
        result
    );
}

#[test]
fn remote_missing_clear_state_removes_sync_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = DaemonState {
        token: "t".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    // Set up without writing file
    state.vault = Some(vida_core::vault::Vault::default());
    state.passphrase = Some("pass".to_string());
    let backend = Box::new(LocalPathBackend::new(dir.path().to_path_buf()));
    state.sync = Some(SyncCoordinator::with_state(
        backend,
        state.vault_path.clone(),
        None,
    ));

    // Sync to establish state (uploads to non-existent remote)
    let (result, _) = tokio_test::block_on(state.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }));

    let state_path = vida_core::config::config_dir()
        .unwrap()
        .join("sync_state.json");
    assert!(state_path.exists(), "sync state should exist");

    state.handle_remote_missing("clear_state").unwrap();

    assert!(
        !state_path.exists(),
        "sync state should be deleted after clear_state"
    );
}

// -----------------------------------------------------------------------
// Write failure → SyncState not updated
// -----------------------------------------------------------------------

#[test]
fn write_failure_does_not_update_sync_state() {
    let dir = tempfile::tempdir().unwrap();
    let vault_path = dir.path().join("vault.age");

    let mut state = DaemonState {
        token: "t".to_string(),
        vault: None,
        passphrase: None,
        vault_path: vault_path.clone(),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state.create_vault("pass").unwrap();

    let backend = Box::new(LocalPathBackend::new(dir.path().to_path_buf()));
    let mut sync = SyncCoordinator::with_state(backend, vault_path.clone(), None);
    let ct = vida_core::vault::encrypt(state.vault.as_ref().unwrap(), "pass").unwrap();
    let local = vida_core::sync::LocalVaultInfo {
        ciphertext: ct,
        revision: 1,
        device_id: "test".to_string(),
    };
    tokio_test::block_on(sync.sync(&local)).unwrap();

    // Record sync state hash from the coordinator (not from global config dir)
    let hash_before = sync.state().map(|s| s.last_synced_hash.clone());

    // Make vault.age read-only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&vault_path, std::fs::Permissions::from_mode(0o444)).unwrap();
    }

    // NoChange sync should not touch SyncState
    let ct2 = vida_core::vault::encrypt(state.vault.as_ref().unwrap(), "pass").unwrap();
    let local2 = vida_core::sync::LocalVaultInfo {
        ciphertext: ct2,
        revision: 1,
        device_id: "test".to_string(),
    };
    let _ = tokio_test::block_on(sync.sync(&local2));

    let hash_after = sync.state().map(|s| s.last_synced_hash.clone());
    assert_eq!(
        hash_before, hash_after,
        "SyncState must not change on NoChange"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&vault_path, std::fs::Permissions::from_mode(0o644));
    }
}

// -----------------------------------------------------------------------
// Original tests (kept from previous round)
// -----------------------------------------------------------------------

#[test]
fn token_generation_and_persistence() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("VIDA_CONFIG_DIR", dir.path()) };
    let token1 = vida_daemon::ws_server::load_or_create_token().unwrap();
    let token2 = vida_daemon::ws_server::load_or_create_token().unwrap();
    assert_eq!(token1, token2);
    assert_eq!(token1.len(), 64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(dir.path().join("daemon.token"))
            .unwrap()
            .permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }
    unsafe { std::env::remove_var("VIDA_CONFIG_DIR") };
}

#[test]
fn token_cleanup_on_exit() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("VIDA_CONFIG_DIR", dir.path()) };
    let _token = vida_daemon::ws_server::load_or_create_token().unwrap();
    assert!(dir.path().join("daemon.token").exists());
    vida_daemon::ws_server::cleanup_token();
    assert!(!dir.path().join("daemon.token").exists());
    unsafe { std::env::remove_var("VIDA_CONFIG_DIR") };
}

#[test]
fn lock_returns_unlocked_error() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    state.lock();
    let result = state.list_hosts();
    assert!(result.is_err());
    assert!(format!("{}", result.unwrap_err()).contains("锁定"));
}

#[test]
fn lock_clears_memory() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    state.lock();
    assert!(state.vault.is_none());
    assert!(state.passphrase.is_none());
    assert!(state.sync.is_none());
}

#[test]
fn create_unlock_lock_cycle() {
    let (mut state, _dir) = test_state("tok");
    let info = state.create_vault("mypass").unwrap();
    assert!(!info.locked);
    state.lock();
    assert!(state.vault_status().locked);
    let info = state.unlock("mypass", None).unwrap();
    assert!(!info.locked);
}

#[test]
fn unlock_wrong_passphrase_fails() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("correct").unwrap();
    state.lock();
    assert!(state.unlock("wrong", None).is_err());
}

#[test]
fn unexpired_keyring_cache_unlocks_restarted_daemon() {
    let _env_guard = ENV_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("VIDA_CONFIG_DIR", dir.path()) };

    let vault_path = dir.path().join("vault.age");
    let mut first = DaemonState {
        token: "first".into(),
        vault: None,
        passphrase: None,
        vault_path: vault_path.clone(),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    first.create_vault("cached-passphrase").unwrap();
    first.unlock("cached-passphrase", Some(60)).unwrap();

    let mut restarted = DaemonState {
        token: "restarted".into(),
        vault: None,
        passphrase: None,
        vault_path,
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    assert!(restarted.try_unlock_cached().unwrap());
    assert!(!restarted.vault_status().locked);

    restarted.lock();
    assert!(vida_core::keyring_cache::get_cached_passphrase().is_none());
    unsafe { std::env::remove_var("VIDA_CONFIG_DIR") };
}

#[test]
fn create_duplicate_vault_fails() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    state.lock();
    assert!(state.create_vault("pass").is_err());
}

#[test]
fn export_backup_produces_valid_ciphertext() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    add_test_host(&mut state, "myhost");
    let backup = state.export_backup(None).unwrap();
    assert!(!backup.is_empty());
    let decrypted = vida_core::vault::decrypt(&backup, "pass").unwrap();
    assert_eq!(decrypted.hosts.len(), 1);
    assert_eq!(decrypted.hosts[0].name, "myhost");
}

#[test]
fn restore_backup_validates_before_replacing_and_rotates_current_vault() {
    let _guard = ENV_MUTEX.lock().unwrap();
    let (mut state, dir) = test_state("tok");
    unsafe { std::env::set_var("VIDA_CONFIG_DIR", dir.path()) };

    state.create_vault("current-pass").unwrap();
    add_test_host(&mut state, "from-backup");
    let backup = state.export_backup(Some("backup-pass")).unwrap();
    add_test_host(&mut state, "current-only");

    assert!(state.preview_backup(&backup, "wrong-pass").is_err());
    assert_eq!(state.list_hosts().unwrap().len(), 2);

    let (host_count, _, host_names) = state.preview_backup(&backup, "backup-pass").unwrap();
    assert_eq!(host_count, 1);
    assert_eq!(host_names, vec!["from-backup"]);

    let (restored_hosts, warning) = state.restore_backup(&backup, "backup-pass").unwrap();
    assert_eq!(restored_hosts.len(), 1);
    assert_eq!(restored_hosts[0].name, "from-backup");
    assert!(warning.is_none());

    state.lock();
    state.unlock("backup-pass", None).unwrap();
    assert_eq!(state.list_hosts().unwrap().len(), 1);

    let rotated = std::fs::read(dir.path().join("vault.1.age")).unwrap();
    let previous = vida_core::vault::decrypt(&rotated, "current-pass").unwrap();
    assert_eq!(previous.hosts.len(), 2);
    assert!(
        previous
            .hosts
            .iter()
            .any(|host| host.name == "current-only")
    );

    unsafe { std::env::remove_var("VIDA_CONFIG_DIR") };
}

#[test]
fn list_hosts_no_plaintext_credentials() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    add_test_host(&mut state, "server1");
    let hosts = state.list_hosts().unwrap();
    let json = serde_json::to_string(&hosts).unwrap();
    assert!(!json.contains("secret-password-123"));
}

#[test]
fn reveal_credential_returns_plaintext() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    add_test_host(&mut state, "server1");
    let host_id = state.list_hosts().unwrap()[0].id.clone();
    assert_eq!(
        state.reveal_credential(&host_id).unwrap(),
        "secret-password-123"
    );
}

#[test]
fn add_host_with_password() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    let summary = state
        .update_host(HostRequest {
            id: None,
            name: "web".to_string(),
            host: "1.2.3.4".to_string(),
            user: "admin".to_string(),
            port: 22,
            tags: vec!["prod".to_string()],
            group: Some("servers".to_string()),
            color: Some("#ff0000".to_string()),
            password: Some("mypassword".to_string()),
            auth: None,
            notes: Some("notes".to_string()),
        })
        .unwrap();
    assert_eq!(summary.name, "web");
}

#[test]
fn add_hosts_with_key_file_and_imported_key() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();

    let key_file = state
        .update_host(HostRequest {
            id: None,
            name: "key-file".to_string(),
            host: "server.example".to_string(),
            user: "deploy".to_string(),
            port: 2222,
            tags: vec![],
            group: None,
            color: None,
            password: None,
            auth: Some(HostAuthRequest::Key {
                private_key_path: "/secure/id_ed25519".to_string(),
                passphrase: Some("key-passphrase".to_string()),
            }),
            notes: None,
        })
        .unwrap();
    assert_eq!(key_file.auth_kind, "key");
    assert_eq!(
        state.reveal_credential(&key_file.id).unwrap(),
        "key:/secure/id_ed25519"
    );

    let inline = state
        .update_host(HostRequest {
            id: None,
            name: "imported-key".to_string(),
            host: "server.example".to_string(),
            user: "deploy".to_string(),
            port: 22,
            tags: vec![],
            group: None,
            color: None,
            password: None,
            auth: Some(HostAuthRequest::KeyInline {
                private_key: "PRIVATE KEY CONTENT".to_string(),
                passphrase: None,
            }),
            notes: None,
        })
        .unwrap();
    assert_eq!(inline.auth_kind, "key_inline");
    assert_eq!(
        state.reveal_credential(&inline.id).unwrap(),
        "PRIVATE KEY CONTENT"
    );
}

#[test]
fn update_host_keep_credential() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    add_test_host(&mut state, "original");
    let host_id = state.list_hosts().unwrap()[0].id.clone();
    state
        .update_host(HostRequest {
            id: Some(host_id.clone()),
            name: "updated".to_string(),
            host: "10.0.0.1".to_string(),
            user: "root".to_string(),
            port: 22,
            tags: vec![],
            group: None,
            color: None,
            password: None,
            auth: None,
            notes: None,
        })
        .unwrap();
    assert_eq!(
        state.reveal_credential(&host_id).unwrap(),
        "secret-password-123"
    );
}

#[test]
fn delete_host() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    add_test_host(&mut state, "to-delete");
    let host_id = state.list_hosts().unwrap()[0].id.clone();
    state.delete_host(&host_id).unwrap();
    assert!(state.list_hosts().unwrap().is_empty());
}

#[test]
fn update_settings_persists() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    let settings = vida_core::vault::Settings {
        scrollback_lines: 10000,
        terminal_font_family: "SF Mono".into(),
        terminal_font_size: 16.0,
        terminal_cursor_blink: false,
        ..Default::default()
    };
    state.update_settings(settings).unwrap();
    state.lock();
    let ct = std::fs::read(&state.vault_path).unwrap();
    let vault = vida_core::vault::decrypt(&ct, "pass").unwrap();
    assert_eq!(vault.settings.scrollback_lines, 10000);
    assert_eq!(vault.settings.terminal_font_family, "SF Mono");
    assert_eq!(vault.settings.terminal_font_size, 16.0);
    assert!(!vault.settings.terminal_cursor_blink);
}

#[test]
fn read_conflict_file_decrypts() {
    let (mut state, dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    add_test_host(&mut state, "conflict-host");
    let vault = state.vault.as_ref().unwrap();
    let ct = vida_core::vault::encrypt(vault, "pass").unwrap();
    let conflict_path = dir.path().join("vault (conflicted copy 2026-08-01).age");
    std::fs::write(&conflict_path, &ct).unwrap();
    let hosts = state
        .read_conflict_file(conflict_path.to_str().unwrap())
        .unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].name, "conflict-host");
}

#[test]
fn adopt_conflict_file_replaces_vault() {
    let (mut state, dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    add_test_host(&mut state, "local-host");

    let mut conflict_vault = vida_core::vault::Vault::default();
    conflict_vault.hosts.push(vida_core::vault::HostEntry {
        id: uuid::Uuid::new_v4().to_string(),
        name: "conflict-host".to_string(),
        host: "192.168.1.1".to_string(),
        user: "admin".to_string(),
        port: 22,
        tags: vec![],
        group: None,
        color: None,
        auth: vida_core::vault::AuthMethod::Password {
            password: vida_core::vault::SecureString::new("conflict-pass".to_string()),
        },
        notes: None,
        agent_trust: vida_core::agent_policy::AgentTrust::Ask,
    });
    let ct = vida_core::vault::encrypt(&conflict_vault, "pass").unwrap();
    let conflict_path = dir.path().join("vault (conflicted copy).age");
    std::fs::write(&conflict_path, &ct).unwrap();

    let hosts = state
        .adopt_conflict_file(conflict_path.to_str().unwrap())
        .unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].name, "conflict-host");
    assert!(!conflict_path.exists());
    assert!(
        dir.path()
            .join("vault (conflicted copy).age.reviewed")
            .exists()
    );
}

#[test]
fn ignore_conflict_file_renames() {
    let (mut state, dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    let conflict_path = dir.path().join("vault (conflicted copy).age");
    std::fs::write(&conflict_path, b"fake").unwrap();
    state
        .ignore_conflict_file(conflict_path.to_str().unwrap())
        .unwrap();
    assert!(!conflict_path.exists());
    assert!(
        dir.path()
            .join("vault (conflicted copy).age.reviewed")
            .exists()
    );
}

#[test]
fn downloaded_writes_file_and_updates_vault() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    // Device A: create vault with host
    let mut state_a = DaemonState {
        token: "a".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir_a.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state_a.create_vault("pass").unwrap();
    add_test_host(&mut state_a, "host-from-a");

    // Device B: set up without writing file
    let mut state_b = DaemonState {
        token: "b".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir_b.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state_b.vault = Some(vida_core::vault::Vault::default());
    state_b.passphrase = Some("pass".to_string());
    let backend_b = Box::new(LocalPathBackend::new(dir_b.path().to_path_buf()));
    state_b.sync = Some(SyncCoordinator::with_state(
        backend_b,
        state_b.vault_path.clone(),
        None,
    ));
    assert_eq!(state_b.list_hosts().unwrap().len(), 0);

    // B uploads empty vault to establish state
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }));

    // A's file overwrites B's
    let ct_a = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a).unwrap();

    // B syncs → Downloaded
    let (result, hosts) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::Downloaded { .. }),
        "expected Downloaded, got {:?}",
        result
    );
    assert!(hosts.is_some());
    assert_eq!(hosts.unwrap().len(), 1);
    assert_eq!(state_b.list_hosts().unwrap()[0].name, "host-from-a");

    // Second sync → NoChange
    let (result2, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result2, SyncResult::NoChange));
}

#[test]
fn downloaded_write_failure_preserves_sync_state() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    // Device A: create vault with host
    let mut state_a = DaemonState {
        token: "a".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir_a.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state_a.create_vault("pass").unwrap();
    add_test_host(&mut state_a, "host-from-a");

    // Device B: set up without writing file
    let mut state_b = DaemonState {
        token: "b".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir_b.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state_b.vault = Some(vida_core::vault::Vault::default());
    state_b.passphrase = Some("pass".to_string());
    let backend_b = Box::new(LocalPathBackend::new(dir_b.path().to_path_buf()));
    state_b.sync = Some(SyncCoordinator::with_state(
        backend_b,
        state_b.vault_path.clone(),
        None,
    ));

    // B uploads empty vault to establish state
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }));

    // A's file overwrites B's → B syncs → Downloaded (establishes sync state)
    let ct_a = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a).unwrap();
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Downloaded { .. }));

    // Record SyncState before the failed sync
    let hash_before = state_b
        .sync
        .as_ref()
        .unwrap()
        .state()
        .map(|s| s.last_synced_hash.clone());
    let rev_before = state_b
        .sync
        .as_ref()
        .unwrap()
        .state()
        .map(|s| s.last_synced_revision);

    // A adds another host and creates new ciphertext — write to B's dir while still writable
    add_test_host(&mut state_a, "host-from-a2");
    let ct_a2 = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a2).unwrap();

    // NOW make dir read-only: download (read) still works, but write_atomic (create temp) fails
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir_b.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    }

    // B syncs → coordinator reads file OK (Downloaded), daemon's write_atomic fails
    let result = tokio_test::block_on(state_b.sync());
    assert!(
        result.is_err(),
        "sync should fail when directory is read-only, got: {:?}",
        result.ok()
    );

    // SyncState must be unchanged — system still knows it hasn't synced
    let hash_after = state_b
        .sync
        .as_ref()
        .unwrap()
        .state()
        .map(|s| s.last_synced_hash.clone());
    let rev_after = state_b
        .sync
        .as_ref()
        .unwrap()
        .state()
        .map(|s| s.last_synced_revision);
    assert_eq!(
        hash_before, hash_after,
        "SyncState hash must not change on write failure"
    );
    assert_eq!(
        rev_before, rev_after,
        "SyncState revision must not change on write failure"
    );

    // Fix permissions and sync again → should still return Downloaded (retry succeeds)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir_b.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (result, hosts) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::Downloaded { .. }),
        "after fixing permissions, sync should succeed with Downloaded, got {:?}",
        result
    );
    assert!(hosts.is_some());
    assert!(hosts.unwrap().iter().any(|h| h.name == "host-from-a2"));
}

#[test]
fn conflict_resolve_remote_replaces_vault() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    // Device A: create vault with host
    let mut state_a = DaemonState {
        token: "a".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir_a.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state_a.create_vault("pass").unwrap();
    add_test_host(&mut state_a, "host-a");

    // Device B: set up without writing file
    let mut state_b = DaemonState {
        token: "b".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir_b.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state_b.vault = Some(vida_core::vault::Vault::default());
    state_b.passphrase = Some("pass".to_string());
    let backend_b = Box::new(LocalPathBackend::new(dir_b.path().to_path_buf()));
    state_b.sync = Some(SyncCoordinator::with_state(
        backend_b,
        state_b.vault_path.clone(),
        None,
    ));

    // B uploads empty vault to establish state
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }));

    // A's file overwrites B's
    let ct_a = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a).unwrap();

    // B syncs → Downloaded
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::Downloaded { .. }),
        "expected Downloaded, got {:?}",
        result
    );

    // Both sides change
    add_test_host(&mut state_a, "host-a2");
    add_test_host(&mut state_b, "host-b");
    let ct_a2 = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a2).unwrap();

    // B syncs → Conflict
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::Conflict { .. }),
        "expected Conflict, got {:?}",
        result
    );

    // Resolve choosing remote
    let (hosts,) = tokio_test::block_on(state_b.resolve_conflict(ConflictChoice::Remote)).unwrap();
    assert!(
        hosts
            .iter()
            .any(|h| h.name == "host-a" || h.name == "host-a2")
    );

    // After resolving to remote, the local state must equal the remote:
    // a follow-up sync must report NoChange (not re-upload).
    // Regression guard: a hardcoded remote revision of 0 would make
    // last_synced_revision < local revision and trigger a false re-upload.
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::NoChange),
        "sync after resolve-to-remote must be NoChange, got {:?}",
        result
    );

    // Verify no write-back
    add_test_host(&mut state_b, "new-after-resolve");
    let ct_check = std::fs::read(&state_b.vault_path).unwrap();
    let vault_check = vida_core::vault::decrypt(&ct_check, "pass").unwrap();
    assert!(
        !vault_check.hosts.iter().any(|h| h.name == "host-b"),
        "old local host-b must not be written back"
    );
}

// -----------------------------------------------------------------------
// GetSettings IPC test
// -----------------------------------------------------------------------

#[tokio::test]
async fn get_settings_returns_vault_settings() {
    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    // Auth
    send_recv(
        &mut ws,
        &mut reader,
        &format!(
            r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
            token
        ),
    )
    .await;

    // Create vault
    send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"CreateVault","params":{"passphrase":"test123"},"id":2}"#,
    )
    .await;

    // Get settings — should return defaults
    let resp = send_recv(&mut ws, &mut reader, r#"{"method":"GetSettings","id":3}"#).await;
    assert_eq!(resp["type"], "Ok", "GetSettings should succeed: {}", resp);
    let result = &resp["result"];
    assert_eq!(result["scrollback_lines"], 3000, "default scrollback_lines");
    assert!(
        result["sync_local_path"].is_null(),
        "default sync_local_path is null"
    );
    assert_eq!(result["terminal_font_family"], "JetBrains Mono");
    assert_eq!(result["terminal_font_size"], 13.0);
    assert_eq!(result["terminal_cursor_blink"], true);
}

#[tokio::test]
async fn sync_response_includes_new_fields() {
    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    // Auth
    send_recv(
        &mut ws,
        &mut reader,
        &format!(
            r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
            token
        ),
    )
    .await;

    // Create vault
    send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"CreateVault","params":{"passphrase":"test123"},"id":2}"#,
    )
    .await;

    // Sync — should return a valid SyncResponse with new fields
    let resp = send_recv(&mut ws, &mut reader, r#"{"method":"Sync","id":3}"#).await;
    assert_eq!(resp["type"], "Ok", "Sync should succeed: {}", resp);
    let result = &resp["result"];
    assert!(result["status"].is_string(), "status must be present");
    // files: null or array of ConflictFile objects
    if let Some(files) = result.get("files") {
        assert!(files.is_array(), "files must be array when present");
    }
    // remote_hosts: null or array of HostSummary objects
    if let Some(rh) = result.get("remote_hosts") {
        assert!(rh.is_array(), "remote_hosts must be array when present");
    }
}

// -----------------------------------------------------------------------
// Conflict branch: remote_hosts populated from remote ciphertext
// -----------------------------------------------------------------------

#[test]
fn conflict_sync_returns_remote_hosts() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    // Device A: create vault + host (writes to disk, won't sync until later)
    let mut state_a = DaemonState {
        token: "a".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir_a.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state_a.create_vault("pass").unwrap();
    add_test_host(&mut state_a, "host-a");

    // Device B: vault in memory only (no file on disk)
    let mut state_b = DaemonState {
        token: "b".to_string(),
        vault: None,
        passphrase: None,
        vault_path: dir_b.path().join("vault.age"),
        sync: None,
        i18n: vida_core::i18n::I18n::default(),
        pty: std::sync::Arc::new(std::sync::RwLock::new(
            vida_daemon::pty::PtyManager::default(),
        )),
    };
    state_b.vault = Some(vida_core::vault::Vault::default());
    state_b.passphrase = Some("pass".to_string());
    let backend_b = Box::new(LocalPathBackend::new(dir_b.path().to_path_buf()));
    state_b.sync = Some(SyncCoordinator::with_state(
        backend_b,
        state_b.vault_path.clone(),
        None,
    ));

    // B1: upload empty vault to establish state (no file on disk → upload)
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::Uploaded { .. }),
        "B1 should be Uploaded, got: {:?}",
        result
    );

    // A's file overwrites B's → B2: sync → Downloaded (B's local unchanged)
    let ct_a = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a).unwrap();
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::Downloaded { .. }),
        "B2 should be Downloaded, got: {:?}",
        result
    );

    // NOW both sides change → B3: sync → Conflict
    add_test_host(&mut state_a, "host-a2");
    add_test_host(&mut state_b, "host-b");
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    match &result {
        SyncResult::Conflict {
            remote_ciphertext, ..
        } => {
            assert!(
                remote_ciphertext.is_some(),
                "Conflict must have remote_ciphertext"
            );
            let remote = state_b
                .decrypt_remote_hosts(remote_ciphertext.as_ref().unwrap())
                .unwrap();
            assert!(!remote.is_empty(), "remote_hosts must not be empty");
            assert!(
                remote
                    .iter()
                    .any(|h| h.name == "host-a" || h.name == "host-a2"),
                "remote_hosts should contain A's hosts: {:?}",
                remote
            );
            for host in &remote {
                assert!(
                    host.auth_kind == "password"
                        || host.auth_kind == "key"
                        || host.auth_kind == "key_inline",
                    "auth_kind must be sanitized, got: {}",
                    host.auth_kind
                );
            }
        }
        other => panic!("expected Conflict, got {:?}", other),
    }
}

#[test]
fn sync_end_to_end_config_to_upload() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();

    // Configure sync via the real settings path (triggers init_sync)
    let sync_dir = tempfile::tempdir().unwrap();
    let settings = vida_core::vault::Settings {
        sync_local_path: Some(sync_dir.path().to_str().unwrap().to_string()),
        ..Default::default()
    };
    state.update_settings(settings).unwrap();
    assert!(
        state.sync.is_some(),
        "sync must be initialized after update_settings with a path"
    );

    // Add a host through the real host path
    add_test_host(&mut state, "e2e-host");

    // Sync through the real daemon sync() entry point
    let (result, hosts) = tokio_test::block_on(state.sync()).unwrap();
    assert!(
        matches!(result, SyncResult::Uploaded { .. }),
        "expected Uploaded, got: {:?}",
        result
    );
    assert!(
        hosts.is_none(),
        "Uploaded should not return hosts (nothing downloaded)"
    );

    // Remote file must exist and contain the host after decryption
    let remote_path = sync_dir.path().join("vault.age");
    assert!(
        remote_path.exists(),
        "remote vault.age must exist after sync"
    );
    let remote_ct = std::fs::read(&remote_path).unwrap();
    let remote_vault = vida_core::vault::decrypt(&remote_ct, "pass").unwrap();
    assert!(
        remote_vault.hosts.iter().any(|h| h.name == "e2e-host"),
        "remote vault must contain the uploaded host"
    );

    // Second sync: no changes → NoChange
    let (result2, _) = tokio_test::block_on(state.sync()).unwrap();
    assert!(
        matches!(result2, SyncResult::NoChange),
        "expected NoChange on second sync, got: {:?}",
        result2
    );
}

// -----------------------------------------------------------------------
// PTY session tests (M2a-2)
// -----------------------------------------------------------------------

/// Agent token cannot bypass the daemon gate through human SessionInput.
/// Safe commands execute; dangerous commands remain unsent until owner action.
#[tokio::test]
async fn agent_role_is_policy_gated_and_owner_can_deny() {
    use sha2::{Digest, Sha256};

    let (addr, token, _dir) = start_daemon().await;
    let (mut owner, mut owner_reader) = connect(addr).await;
    let owner_auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}","role":"owner"}},"id":1}}"#,
        token
    );
    assert_eq!(
        send_recv(&mut owner, &mut owner_reader, &owner_auth).await["type"],
        "Ok"
    );
    let opened = send_recv(
        &mut owner,
        &mut owner_reader,
        r#"{"method":"OpenLocalSession","params":{"cols":80,"rows":24},"id":2}"#,
    )
    .await;
    let session_id = opened["result"]["session_id"].as_str().unwrap();

    let agent_token = hex::encode(Sha256::digest(format!("vida-agent:{token}").as_bytes()));
    let (mut agent, mut agent_reader) = connect(addr).await;
    let agent_auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}","role":"agent"}},"id":1}}"#,
        agent_token
    );
    let response = send_recv(&mut agent, &mut agent_reader, &agent_auth).await;
    assert_eq!(response["result"]["role"], "agent");

    let raw_input = format!(
        r#"{{"method":"SessionInput","params":{{"session_id":"{}","data":[101,99,104,111,13]}},"id":2}}"#,
        session_id
    );
    let response = send_recv(&mut agent, &mut agent_reader, &raw_input).await;
    assert_eq!(response["type"], "Error");
    assert_eq!(response["category"], "forbidden");

    let safe = format!(
        r#"{{"method":"AgentExec","params":{{"session_id":"{}","command":"echo VIDA_AGENT_SAFE"}},"id":3}}"#,
        session_id
    );
    let response = send_recv(&mut agent, &mut agent_reader, &safe).await;
    assert_eq!(response["result"]["status"], "sent");

    let dangerous = format!(
        r#"{{"method":"AgentExec","params":{{"session_id":"{}","command":"rm -rf /tmp/vida-never-approved"}},"id":4}}"#,
        session_id
    );
    let response = send_recv(&mut agent, &mut agent_reader, &dangerous).await;
    assert_eq!(response["result"]["status"], "needs_approval");
    let approval_id = response["result"]["approval_id"].as_str().unwrap();

    let requested = recv_text(&mut owner_reader).await;
    assert_eq!(requested["type"], "Event");
    assert_eq!(requested["event"], "agent_approval");
    assert_eq!(requested["data"]["kind"], "approval_requested");
    assert_eq!(requested["data"]["approval"]["approval_id"], approval_id);
    assert_eq!(
        requested["data"]["approval"]["command"],
        "rm -rf /tmp/vida-never-approved"
    );

    let deny = format!(
        r#"{{"method":"DenyAgentAction","params":{{"approval_id":"{}"}},"id":3}}"#,
        approval_id
    );
    let response = send_recv(&mut owner, &mut owner_reader, &deny).await;
    assert_eq!(response["result"]["status"], "denied");
    let resolved = recv_text(&mut owner_reader).await;
    assert_eq!(resolved["data"]["kind"], "approval_resolved");
    assert_eq!(resolved["data"]["approval_id"], approval_id);
    assert_eq!(resolved["data"]["status"], "denied");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let screen = format!(
        r#"{{"method":"ReadScreen","params":{{"session_id":"{}"}},"id":4}}"#,
        session_id
    );
    let response = send_recv(&mut owner, &mut owner_reader, &screen).await;
    let text = response["result"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|line| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("VIDA_AGENT_SAFE"));
    assert!(!text.contains("vida-never-approved"));

    let audit = send_recv(
        &mut owner,
        &mut owner_reader,
        r#"{"method":"ReadAgentAudit","params":{"limit":10},"id":5}"#,
    )
    .await;
    let outcomes: Vec<_> = audit["result"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["outcome"].as_str())
        .collect();
    assert!(outcomes.contains(&"allowed"));
    assert!(outcomes.contains(&"needs_approval"));
    assert!(outcomes.contains(&"denied"));
}

#[tokio::test]
async fn agent_opens_one_configured_ssh_session_without_exposing_credentials() {
    use sha2::{Digest, Sha256};

    let (addr, token, _dir) = start_daemon().await;
    let (mut owner, mut owner_reader) = connect(addr).await;
    let owner_auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}","role":"owner"}},"id":1}}"#,
        token
    );
    assert_eq!(
        send_recv(&mut owner, &mut owner_reader, &owner_auth).await["type"],
        "Ok"
    );
    assert_eq!(
        send_recv(
            &mut owner,
            &mut owner_reader,
            r#"{"method":"CreateVault","params":{"passphrase":"test-passphrase"},"id":2}"#,
        )
        .await["type"],
        "Ok"
    );
    let added = send_recv(
        &mut owner,
        &mut owner_reader,
        r#"{"method":"UpdateHost","params":{"host":{"id":null,"name":"Agent SSH","host":"10.0.0.1","user":"root","port":22,"tags":[],"group":null,"color":null,"password":"credential-must-not-leak","notes":null}},"id":3}"#,
    )
    .await;
    let host_id = added["result"]["id"].as_str().unwrap().to_string();

    let agent_token = hex::encode(Sha256::digest(format!("vida-agent:{token}").as_bytes()));
    let (mut agent, mut agent_reader) = connect(addr).await;
    let agent_auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}","role":"agent"}},"id":1}}"#,
        agent_token
    );
    assert_eq!(
        send_recv(&mut agent, &mut agent_reader, &agent_auth).await["result"]["role"],
        "agent"
    );
    let (mut concurrent_agent, mut concurrent_agent_reader) = connect(addr).await;
    assert_eq!(
        send_recv(
            &mut concurrent_agent,
            &mut concurrent_agent_reader,
            &agent_auth,
        )
        .await["result"]["role"],
        "agent"
    );

    let open = format!(
        r#"{{"method":"AgentOpenSshSession","params":{{"host_id":"{}"}},"id":2}}"#,
        host_id
    );
    let (first, concurrent) = tokio::join!(
        send_recv(&mut agent, &mut agent_reader, &open),
        send_recv(&mut concurrent_agent, &mut concurrent_agent_reader, &open,)
    );
    for response in [&first, &concurrent] {
        assert_eq!(response["type"], "Ok", "open failed: {response}");
        assert_eq!(response["result"]["host_id"], host_id);
        assert_eq!(response["result"]["title"], "Agent SSH");
        assert!(!response.to_string().contains("credential-must-not-leak"));
    }
    assert_ne!(first["result"]["reused"], concurrent["result"]["reused"]);
    assert_eq!(
        first["result"]["session_id"],
        concurrent["result"]["session_id"]
    );
    let session_id = first["result"]["session_id"].as_str().unwrap().to_string();

    let event = recv_text(&mut owner_reader).await;
    assert_eq!(event["event"], "agent_approval");
    assert_eq!(event["data"]["kind"], "session_opened");
    assert_eq!(event["data"]["session_id"], session_id);
    assert_eq!(event["data"]["host_id"], host_id);
    assert!(!event.to_string().contains("credential-must-not-leak"));

    let second = send_recv(&mut agent, &mut agent_reader, &open).await;
    assert_eq!(second["result"]["session_id"], session_id);
    assert_eq!(second["result"]["reused"], true);

    let audit = send_recv(
        &mut owner,
        &mut owner_reader,
        r#"{"method":"ReadAgentAudit","params":{"limit":10},"id":4}"#,
    )
    .await;
    assert!(audit["result"].as_array().unwrap().iter().any(|entry| {
        entry["tool"] == "session_open"
            && entry["session_id"] == session_id
            && entry["host_id"] == host_id
    }));
    assert!(!audit.to_string().contains("credential-must-not-leak"));

    let close = format!(
        r#"{{"method":"CloseSession","params":{{"session_id":"{}"}},"id":5}}"#,
        session_id
    );
    assert_eq!(
        send_recv(&mut owner, &mut owner_reader, &close).await["type"],
        "Ok"
    );
}

#[tokio::test]
async fn agent_prepares_non_secret_host_draft_without_writing_vault() {
    use sha2::{Digest, Sha256};

    let (addr, token, _dir) = start_daemon().await;
    let (mut owner, mut owner_reader) = connect(addr).await;
    let owner_auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}","role":"owner"}},"id":1}}"#,
        token
    );
    assert_eq!(
        send_recv(&mut owner, &mut owner_reader, &owner_auth).await["type"],
        "Ok"
    );
    assert_eq!(
        send_recv(
            &mut owner,
            &mut owner_reader,
            r#"{"method":"CreateVault","params":{"passphrase":"test-passphrase"},"id":2}"#,
        )
        .await["type"],
        "Ok"
    );
    let owner_attempt = send_recv(
        &mut owner,
        &mut owner_reader,
        r#"{"method":"AgentPrepareHost","params":{"draft":{"name":"Owner path","host":"example.com","user":"root","port":22}},"id":99}"#,
    )
    .await;
    assert_eq!(owner_attempt["type"], "Error");

    let agent_token = hex::encode(Sha256::digest(format!("vida-agent:{token}").as_bytes()));
    let (mut agent, mut agent_reader) = connect(addr).await;
    let agent_auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}","role":"agent"}},"id":1}}"#,
        agent_token
    );
    assert_eq!(
        send_recv(&mut agent, &mut agent_reader, &agent_auth).await["result"]["role"],
        "agent"
    );

    let prepared = send_recv(
        &mut agent,
        &mut agent_reader,
        r#"{"method":"AgentPrepareHost","params":{"draft":{"name":"  New server  ","host":"example.com","user":"deploy","port":2222,"tags":["prod","prod"],"group":"  Edge  ","notes":"review me"}},"id":2}"#,
    )
    .await;
    assert_eq!(prepared["type"], "Ok", "prepare failed: {prepared}");
    assert_eq!(prepared["result"]["status"], "awaiting_human");
    assert!(prepared["result"]["draft_id"].as_str().is_some());

    let event = recv_text(&mut owner_reader).await;
    assert_eq!(event["data"]["kind"], "host_draft_prepared");
    assert_eq!(event["data"]["draft"]["name"], "New server");
    assert_eq!(event["data"]["draft"]["group"], "Edge");
    assert_eq!(event["data"]["draft"]["tags"], serde_json::json!(["prod"]));
    assert!(!event.to_string().contains("password"));
    assert!(!event.to_string().contains("private_key"));

    let hosts = send_recv(
        &mut owner,
        &mut owner_reader,
        r#"{"method":"ListHosts","id":3}"#,
    )
    .await;
    assert_eq!(hosts["result"], serde_json::json!([]));

    let audit = send_recv(
        &mut owner,
        &mut owner_reader,
        r#"{"method":"ReadAgentAudit","params":{"limit":10},"id":4}"#,
    )
    .await;
    let audit_text = audit.to_string();
    assert!(audit_text.contains("host_prepare"));
    assert!(!audit_text.contains("example.com"));
    assert!(!audit_text.contains("New server"));
    assert!(!audit_text.contains("review me"));

    let credential_injection = send_recv(
        &mut agent,
        &mut agent_reader,
        r#"{"method":"AgentPrepareHost","params":{"draft":{"name":"Rejected","host":"example.com","user":"root","port":22,"password":"must-not-enter"}},"id":3}"#,
    )
    .await;
    assert_eq!(credential_injection["type"], "Error");
    assert!(!credential_injection.to_string().contains("must-not-enter"));
}

#[tokio::test]
async fn agent_host_draft_requires_unlocked_vault() {
    use sha2::{Digest, Sha256};

    let (addr, token, _dir) = start_daemon().await;
    let agent_token = hex::encode(Sha256::digest(format!("vida-agent:{token}").as_bytes()));
    let (mut agent, mut agent_reader) = connect(addr).await;
    let agent_auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}","role":"agent"}},"id":1}}"#,
        agent_token
    );
    assert_eq!(
        send_recv(&mut agent, &mut agent_reader, &agent_auth).await["result"]["role"],
        "agent"
    );
    let response = send_recv(
        &mut agent,
        &mut agent_reader,
        r#"{"method":"AgentPrepareHost","params":{"draft":{"name":"Locked","host":"example.com","user":"root","port":22}},"id":2}"#,
    )
    .await;
    assert_eq!(response["type"], "Error");
    assert!(response["message"].as_str().unwrap().contains("解锁"));
}

/// 打开会话 → 输入 → 读屏幕 → resize → 列出 → 关闭。
/// 覆盖规格 5.2 全部 6 个 IPC 方法。
#[tokio::test]
async fn pty_session_full_cycle() {
    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;
    let auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
        token
    );
    let resp = send_recv(&mut ws, &mut reader, &auth).await;
    assert_eq!(resp["type"], "Ok", "auth failed: {}", resp);

    // 1. OpenLocalSession
    let resp = send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"OpenLocalSession","params":{"cols":80,"rows":24},"id":2}"#,
    )
    .await;
    assert_eq!(resp["type"], "Ok", "open failed: {}", resp);
    let session_id = resp["result"]["session_id"].as_str().unwrap().to_string();

    // 2. SessionInput（data 是字节数组）
    let input_data = serde_json::to_string(&b"echo hello\r\n".to_vec()).unwrap();
    let input = format!(
        r#"{{"method":"SessionInput","params":{{"session_id":"{}","data":{}}},"id":3}}"#,
        session_id, input_data
    );
    let resp = send_recv(&mut ws, &mut reader, &input).await;
    assert_eq!(resp["type"], "Ok", "input failed: {}", resp);
    assert_eq!(resp["result"]["ok"], true);

    // 3. ReadScreen（等 shell 输出到达）
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let screen = format!(
        r#"{{"method":"ReadScreen","params":{{"session_id":"{}"}},"id":4}}"#,
        session_id
    );
    let resp = send_recv(&mut ws, &mut reader, &screen).await;
    assert_eq!(resp["type"], "Ok", "read_screen failed: {}", resp);
    let lines: Vec<String> = resp["result"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let joined = lines.join("\n");
    assert!(
        joined.contains("hello"),
        "screen should contain hello: {}",
        joined
    );
    assert!(
        resp["result"]["cursor"]["row"].is_number(),
        "cursor row missing"
    );
    assert!(
        resp["result"]["cursor"]["col"].is_number(),
        "cursor col missing"
    );

    // 4. ResizeSession
    let resize = format!(
        r#"{{"method":"ResizeSession","params":{{"session_id":"{}","cols":40,"rows":12}},"id":5}}"#,
        session_id
    );
    let resp = send_recv(&mut ws, &mut reader, &resize).await;
    assert_eq!(resp["type"], "Ok", "resize failed: {}", resp);
    assert_eq!(resp["result"]["ok"], true);

    // 5. ListSessions — 应含该会话且尺寸已更新
    let resp = send_recv(&mut ws, &mut reader, r#"{"method":"ListSessions","id":6}"#).await;
    assert_eq!(resp["type"], "Ok", "list failed: {}", resp);
    let sessions = resp["result"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "should have 1 session: {}", resp);
    assert_eq!(sessions[0]["cols"], 40, "cols should be resized");
    assert_eq!(sessions[0]["rows"], 12, "rows should be resized");
    assert_eq!(sessions[0]["alive"], true);
    assert_eq!(sessions[0]["target_kind"], "local");
    assert!(
        sessions[0]["title"]
            .as_str()
            .is_some_and(|title| !title.is_empty()),
        "session title should identify the target: {resp}"
    );
    assert!(sessions[0]["host_id"].is_null());
    assert!(sessions[0]["host_name"].is_null());

    // 6. CloseSession
    let close = format!(
        r#"{{"method":"CloseSession","params":{{"session_id":"{}"}},"id":7}}"#,
        session_id
    );
    let resp = send_recv(&mut ws, &mut reader, &close).await;
    assert_eq!(resp["type"], "Ok", "close failed: {}", resp);
    assert_eq!(resp["result"]["ok"], true);

    // 关闭后 ListSessions 应为空
    let resp = send_recv(&mut ws, &mut reader, r#"{"method":"ListSessions","id":8}"#).await;
    let sessions = resp["result"].as_array().unwrap();
    assert_eq!(sessions.len(), 0, "sessions should be empty after close");
}

/// 尺寸校验：0 和超大值必须被拒绝（规格约束 4）。
#[tokio::test]
async fn pty_session_size_validation() {
    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;
    let auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
        token
    );
    let resp = send_recv(&mut ws, &mut reader, &auth).await;
    assert_eq!(resp["type"], "Ok");

    // cols=0
    let resp = send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"OpenLocalSession","params":{"cols":0,"rows":24},"id":2}"#,
    )
    .await;
    assert_eq!(resp["type"], "Error", "cols=0 should be rejected: {}", resp);
    assert!(resp["message"].as_str().unwrap().contains("不能为 0"));

    // rows=0
    let resp = send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"OpenLocalSession","params":{"cols":80,"rows":0},"id":3}"#,
    )
    .await;
    assert_eq!(resp["type"], "Error", "rows=0 should be rejected: {}", resp);

    // 超大 1001×24
    let resp = send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"OpenLocalSession","params":{"cols":1001,"rows":24},"id":4}"#,
    )
    .await;
    assert_eq!(
        resp["type"], "Error",
        "huge cols should be rejected: {}",
        resp
    );
    assert!(resp["message"].as_str().unwrap().contains("超限"));

    // 超大 80×1001
    let resp = send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"OpenLocalSession","params":{"cols":80,"rows":1001},"id":5}"#,
    )
    .await;
    assert_eq!(
        resp["type"], "Error",
        "huge rows should be rejected: {}",
        resp
    );

    // 非法会话 ID
    let resp = send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"SessionInput","params":{"session_id":"nonexistent","data":"x"},"id":6}"#,
    )
    .await;
    assert_eq!(
        resp["type"], "Error",
        "nonexistent session should error: {}",
        resp
    );
    let msg = resp["message"].as_str().unwrap();
    assert!(
        !msg.is_empty(),
        "error message should explain the failure: {}",
        resp
    );
}

// -----------------------------------------------------------------------
// PTY push protocol tests (M2a-2-2)
// -----------------------------------------------------------------------

/// yes 场景：持续高吞吐，验证推送带宽有界。
///
/// 注意：SessionInput.data 是 Vec<u8>，JSON 中必须编码为字节数组。
/// yes → [121,101,115,13,10]（y e s \r \n）
/// Ctrl+C → [3]
#[tokio::test]
async fn pty_yos_bandwidth_bounded() {
    use tokio_tungstenite::tungstenite::Message;

    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    macro_rules! call_ok {
        ($w:expr, $r:expr, $json:expr) => {{
            let resp = send_recv($w, $r, $json).await;
            assert_eq!(resp["type"], "Ok", "IPC 失败: {} → {}", $json, resp);
            resp
        }};
    }

    let auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
        token
    );
    call_ok!(&mut ws, &mut reader, &auth);

    let resp = call_ok!(
        &mut ws,
        &mut reader,
        r#"{"method":"OpenLocalSession","params":{"cols":200,"rows":50},"id":2}"#
    );
    let sid = resp["result"]["session_id"].as_str().unwrap().to_string();

    let sub = format!(
        r#"{{"method":"SubscribeSession","params":{{"session_id":"{}"}},"id":3}}"#,
        sid
    );
    call_ok!(&mut ws, &mut reader, &sub);

    // 启动 yes（data 为字节数组）。不等待文本响应，避免与二进制帧竞争。
    let yes_data = format!(
        r#"{{"method":"SessionInput","params":{{"session_id":"{}","data":[121,101,115,13,10]}},"id":4}}"#,
        sid
    );
    ws.send(Message::Text(yes_data.into())).await.unwrap();

    // 收集 5 秒内的推送帧
    let start = std::time::Instant::now();
    let mut total_bytes: usize = 0;
    let mut frame_count: usize = 0;

    while start.elapsed() < std::time::Duration::from_secs(5) {
        tokio::select! {
            msg = reader.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        total_bytes += bytes.len();
                        frame_count += 1;
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Err(_)) => break,
                    None => break,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }

    let elapsed_secs = start.elapsed().as_secs_f64();
    let bandwidth_mbps = if elapsed_secs > 0.0 {
        (total_bytes as f64 / elapsed_secs) / 1_000_000.0
    } else {
        0.0
    };

    eprintln!(
        "yes 场景: {} 帧, {} bytes, {:.2} MB/s",
        frame_count, total_bytes, bandwidth_mbps
    );

    // 停止 yes（Ctrl+C = 字节 [3]）
    let stop_data = format!(
        r#"{{"method":"SessionInput","params":{{"session_id":"{}","data":[3]}},"id":5}}"#,
        sid
    );
    ws.send(Message::Text(stop_data.into())).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 关闭会话
    let close = format!(
        r#"{{"method":"CloseSession","params":{{"session_id":"{}"}},"id":6}}"#,
        sid
    );
    call_ok!(&mut ws, &mut reader, &close);

    assert!(frame_count > 0, "应收到至少一帧推送");
    assert!(
        bandwidth_mbps < 1.0,
        "带宽过高: {:.2} MB/s（目标 < 1 MB/s）",
        bandwidth_mbps
    );
    assert!(
        total_bytes > 1000,
        "收到的数据太少: {} bytes（yes 应产生大量输出）",
        total_bytes
    );
}

// -----------------------------------------------------------------------
// 单连接多订阅（M2b-1）：同一 WS 连接订阅两个会话，帧带各自 session_id
// -----------------------------------------------------------------------

/// 解帧头：返回 (session_id, 剩余 payload)。
fn decode_frame_header(bytes: &[u8]) -> (String, &[u8]) {
    assert_eq!(bytes[0], 0x01, "帧头魔法字节");
    let id_len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    let id = String::from_utf8(bytes[3..3 + id_len].to_vec()).unwrap();
    (id, &bytes[3 + id_len..])
}

/// 解码帧 payload 为行文本（测试用，宽松解析：任何越界即停止）。
/// 格式：[seq:8][cursor_row:2][cursor_col:2][cursor_visible:1][line_count:2]
/// 每行：[row:2][start:2][end:2][run_count:2]
/// 每 run：[len:2][flags:1][fg tag:1(+payload)][bg tag:1(+payload)][char_len:1][chars]
fn decode_frame_lines(payload: &[u8]) -> Vec<String> {
    // seq(8) + cursor(4) + visible(1) + viewport_start(8) + line_count(2)
    let mut pos = 23usize;
    if payload.len() < pos {
        return Vec::new();
    }
    let line_count = u16::from_be_bytes([payload[21], payload[22]]) as usize;
    let mut rows: std::collections::BTreeMap<u16, String> = std::collections::BTreeMap::new();
    for _ in 0..line_count {
        if pos + 8 > payload.len() {
            break;
        }
        let row = u16::from_be_bytes([payload[pos], payload[pos + 1]]);
        pos += 8; // row + start + end + run_count
        let run_count = u16::from_be_bytes([payload[pos - 2], payload[pos - 1]]) as usize;
        let mut line = String::new();
        for _ in 0..run_count {
            if pos + 3 > payload.len() {
                break;
            }
            // run_len = 该 run 覆盖的列数：字符需重复 run_len 次渲染
            let run_len = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
            pos += 3; // len(2) + flags(1)
            // fg tag
            let fg_tag = payload[pos];
            pos += 1;
            match fg_tag {
                0x00 => {}
                0x01 => pos += 1,
                0x02 => pos += 3,
                _ => return Vec::new(),
            }
            // bg tag
            let bg_tag = payload[pos];
            pos += 1;
            match bg_tag {
                0x00 => {}
                0x01 => pos += 1,
                0x02 => pos += 3,
                _ => return Vec::new(),
            }
            if pos + 1 > payload.len() {
                break;
            }
            let char_len = payload[pos] as usize;
            pos += 1;
            if pos + char_len > payload.len() {
                break;
            }
            if let Ok(s) = std::str::from_utf8(&payload[pos..pos + char_len]) {
                for _ in 0..run_len {
                    line.push_str(s);
                }
            }
            pos += char_len;
        }
        rows.insert(row, line);
    }
    rows.into_values().collect()
}

/// 同一连接订阅两个会话：两个会话的帧都能收到，session_id 各自正确；
/// Unsubscribe 其中一个后，另一个仍正常推送。
#[tokio::test]
async fn single_connection_multi_subscribe() {
    use tokio_tungstenite::tungstenite::Message;

    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    let auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
        token
    );
    let resp = send_recv(&mut ws, &mut reader, &auth).await;
    assert_eq!(resp["type"], "Ok");

    // 打开两个会话
    let mut sids = Vec::new();
    for i in 0..2 {
        let open = format!(
            r#"{{"method":"OpenLocalSession","params":{{"cols":80,"rows":24}},"id":{}}}"#,
            2 + i
        );
        let resp = send_recv(&mut ws, &mut reader, &open).await;
        assert_eq!(resp["type"], "Ok");
        sids.push(resp["result"]["session_id"].as_str().unwrap().to_string());
    }

    // 同一连接订阅两个会话
    for (i, sid) in sids.iter().enumerate() {
        let sub = format!(
            r#"{{"method":"SubscribeSession","params":{{"session_id":"{}"}},"id":{}}}"#,
            sid,
            4 + i
        );
        let resp = send_recv(&mut ws, &mut reader, &sub).await;
        assert_eq!(resp["type"], "Ok", "订阅失败: {}", resp);
    }

    // 向两个会话各发一条 echo，触发增量帧
    for (i, sid) in sids.iter().enumerate() {
        // "echo subN\r" = e c h o _ s u b N
        let mut data = b"echo sub".to_vec();
        data.push(b'0' + i as u8);
        data.push(0x0D);
        let input = format!(
            r#"{{"method":"SessionInput","params":{{"session_id":"{}","data":{:?}}},
               "id":{}}}"#,
            sid,
            data,
            6 + i
        );
        // 不等待响应（二进制帧会抢先），直接发送
        ws.send(Message::Text(input.into())).await.unwrap();
    }

    // 收集 3 秒内两个会话的帧，验证 session_id 各自正确
    let start = std::time::Instant::now();
    let mut received_a = 0usize;
    let mut received_b = 0usize;
    let mut wrong_id = 0usize;
    while start.elapsed() < std::time::Duration::from_secs(3) {
        tokio::select! {
            msg = reader.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        let (sid, _) = decode_frame_header(&bytes);
                        if sid == sids[0] {
                            received_a += 1;
                        } else if sid == sids[1] {
                            received_b += 1;
                        } else {
                            wrong_id += 1;
                        }
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Err(_)) => break,
                    None => break,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }

    assert_eq!(wrong_id, 0, "收到未知 session_id 的帧");
    assert!(
        received_a > 0 && received_b > 0,
        "两个会话都应收到帧: a={} b={}",
        received_a,
        received_b
    );

    // Unsubscribe 会话 A，会话 B 应继续推送
    let unsub_a = format!(
        r#"{{"method":"UnsubscribeSession","params":{{"session_id":"{}"}},"id":20}}"#,
        sids[0]
    );
    let resp = send_recv(&mut ws, &mut reader, &unsub_a).await;
    assert_eq!(resp["type"], "Ok", "取消订阅失败: {}", resp);

    // 会话 B 继续产生输出（echo hello\r）
    let input_b = format!(
        r#"{{"method":"SessionInput","params":{{"session_id":"{}","data":[101,99,104,111,32,104,105,13]}},
           "id":21}}"#,
        sids[1]
    );
    ws.send(Message::Text(input_b.into())).await.unwrap();

    let start2 = std::time::Instant::now();
    let mut b_after = 0usize;
    let mut a_after = 0usize;
    while start2.elapsed() < std::time::Duration::from_secs(2) {
        tokio::select! {
            msg = reader.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        let (sid, _) = decode_frame_header(&bytes);
                        if sid == sids[1] {
                            b_after += 1;
                        } else if sid == sids[0] {
                            a_after += 1;
                        }
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Err(_)) => break,
                    None => break,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
        }
    }

    assert_eq!(a_after, 0, "取消订阅后会话 A 不应再收到帧");
    assert!(b_after > 0, "取消订阅 A 后会话 B 应继续推送");
}

/// 断开重连后重新订阅必须拿全量帧且画面恢复到断线前（规格 4.3）：
/// 1. 打开会话，发送 'echo RECONNECT_MARKER\n' 并等待输出
/// 2. 断开连接（会话属于 daemon，保留）
/// 3. 新连接，用【同一个 session_id】SubscribeSession
/// 4. 断言全量帧中包含 RECONNECT_MARKER（画面恢复到断线前）
/// 5. 断言 ListSessions 数量未增加（没有偷偷新开会话）
#[tokio::test]
async fn reconnect_resubscribe_gets_full_frame() {
    use tokio_tungstenite::tungstenite::Message;

    let (addr, token, _dir) = start_daemon().await;
    // 连接 A
    let (mut ws_a, mut reader_a) = connect(addr).await;
    let auth_a = format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
        token
    );
    let resp = send_recv(&mut ws_a, &mut reader_a, &auth_a).await;
    assert_eq!(resp["type"], "Ok");

    let resp = send_recv(
        &mut ws_a,
        &mut reader_a,
        r#"{"method":"OpenLocalSession","params":{"cols":80,"rows":24},"id":2}"#,
    )
    .await;
    let sid = resp["result"]["session_id"].as_str().unwrap().to_string();

    let sub_a = format!(
        r#"{{"method":"SubscribeSession","params":{{"session_id":"{}"}},"id":3}}"#,
        sid
    );
    let resp = send_recv(&mut ws_a, &mut reader_a, &sub_a).await;
    assert_eq!(resp["type"], "Ok");

    // 在会话中留下可见标记（画面恢复的判据）
    let mut data: Vec<u8> = b"echo RECONNECT_MARKER".to_vec();
    data.push(0x0D);
    let input = format!(
        r#"{{"method":"SessionInput","params":{{"session_id":"{}","data":{:?}}},"id":4}}"#,
        sid, data
    );
    ws_a.send(Message::Text(input.into())).await.unwrap();

    // 等 shell 完全就绪再发送（排除输入过早进入 PTY 队列的时序问题）
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // 等待标记确实出现在连接 A 的帧里（确保 echo 已执行且未滚动出屏）
    let deadline_a = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut marker_on_a = false;
    while tokio::time::Instant::now() < deadline_a {
        match reader_a.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                let (_, payload) = decode_frame_header(&bytes);
                let lines = decode_frame_lines(payload);
                let text: String = lines.join("\n");
                if text.contains("RECONNECT_MARKER") {
                    marker_on_a = true;
                    break;
                }
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("read error: {:?}", e),
            None => panic!("connection closed while waiting for marker"),
        }
    }
    assert!(marker_on_a, "断线前 marker 应出现在连接 A 的帧中");

    // 断开连接 A（会话属于 daemon，保留）
    drop(ws_a);
    drop(reader_a);

    // 连接 B：同一会话重新订阅
    let (mut ws_b, mut reader_b) = connect(addr).await;
    let auth_b = format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
        token
    );
    let resp = send_recv(&mut ws_b, &mut reader_b, &auth_b).await;
    assert_eq!(resp["type"], "Ok");

    // 记录断线前的会话数量
    let resp = send_recv(
        &mut ws_b,
        &mut reader_b,
        r#"{"method":"ListSessions","params":{},"id":2}"#,
    )
    .await;
    let sessions_before = resp["result"].as_array().map(|a| a.len()).unwrap_or(0);

    let sub_b = format!(
        r#"{{"method":"SubscribeSession","params":{{"session_id":"{}"}},"id":3}}"#,
        sid
    );
    let resp = send_recv(&mut ws_b, &mut reader_b, &sub_b).await;
    assert_eq!(resp["type"], "Ok", "重新订阅同一会话失败: {}", resp);

    // 断言收到的帧包含 RECONNECT_MARKER（画面恢复到断线前）
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut marker_found = false;
    while tokio::time::Instant::now() < deadline {
        match reader_b.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                let (sid_b, payload) = decode_frame_header(&bytes);
                assert_eq!(sid_b, sid, "帧的 session_id 应正确");
                let lines = decode_frame_lines(payload);
                let text: String = lines.join("\n");
                if text.contains("RECONNECT_MARKER") {
                    marker_found = true;
                    break;
                }
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("read error: {:?}", e),
            None => panic!("connection closed before full frame"),
        }
    }
    assert!(
        marker_found,
        "重新订阅后的全量帧应包含断线前的输出（RECONNECT_MARKER）"
    );

    // 断言没有偷偷新开会话
    let resp = send_recv(
        &mut ws_b,
        &mut reader_b,
        r#"{"method":"ListSessions","params":{},"id":4}"#,
    )
    .await;
    let sessions_after = resp["result"].as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(
        sessions_after, sessions_before,
        "重连不应新增会话（旧会话应被复用）"
    );
}

#[cfg(unix)]
fn spawn_production_daemon(config_dir: &std::path::Path) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_vida-daemon"))
        .env("VIDA_CONFIG_DIR", config_dir)
        .env("RUST_LOG", "error")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[cfg(unix)]
async fn wait_for_production_daemon(
    config_dir: &std::path::Path,
) -> (std::net::SocketAddr, String) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let port = std::fs::read_to_string(config_dir.join("daemon.port"))
            .ok()
            .and_then(|value| value.trim().parse::<u16>().ok());
        let token = std::fs::read_to_string(config_dir.join("daemon.token")).ok();
        if let (Some(port), Some(token)) = (port, token) {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return (addr, token);
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "production daemon did not become ready"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// M4：真正杀死 daemon 进程后，分离的会话宿主仍保留同一个 shell。
/// 新 daemon 必须列出原 session_id，并保留 shell 的环境变量与工作目录。
#[cfg(unix)]
#[tokio::test]
async fn production_daemon_restart_preserves_shell_process() {
    let directory = tempfile::tempdir().unwrap();
    let mut first_daemon = spawn_production_daemon(directory.path());
    let (first_addr, token) = wait_for_production_daemon(directory.path()).await;
    let (mut ws, mut reader) = connect(first_addr).await;
    let auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#,
        token.trim()
    );
    assert_eq!(send_recv(&mut ws, &mut reader, &auth).await["type"], "Ok");
    let opened = send_recv(
        &mut ws,
        &mut reader,
        r#"{"method":"OpenLocalSession","params":{"cols":100,"rows":30},"id":2}"#,
    )
    .await;
    let session_id = opened["result"]["session_id"].as_str().unwrap().to_string();
    let command = b"export VIDA_M4_PROCESS_MARK=preserved; cd /tmp; echo before-restart\r";
    let input = format!(
        r#"{{"method":"SessionInput","params":{{"session_id":"{}","data":{}}},"id":3}}"#,
        session_id,
        serde_json::to_string(command.as_slice()).unwrap()
    );
    assert_eq!(send_recv(&mut ws, &mut reader, &input).await["type"], "Ok");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    drop(ws);
    drop(reader);

    first_daemon.kill().unwrap();
    first_daemon.wait().unwrap();
    std::fs::remove_file(directory.path().join("daemon.port")).unwrap();

    let mut second_daemon = spawn_production_daemon(directory.path());
    let (second_addr, token) = wait_for_production_daemon(directory.path()).await;
    let (mut ws, mut reader) = connect(second_addr).await;
    let auth = format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":4}}"#,
        token.trim()
    );
    assert_eq!(send_recv(&mut ws, &mut reader, &auth).await["type"], "Ok");

    let listed = send_recv(&mut ws, &mut reader, r#"{"method":"ListSessions","id":5}"#).await;
    assert_eq!(listed["type"], "Ok", "list after restart failed: {listed}");
    let sessions = listed["result"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "original session should survive: {listed}"
    );
    assert_eq!(sessions[0]["session_id"], session_id);

    let command = b"printf '%s:%s\\n' \"$VIDA_M4_PROCESS_MARK\" \"$PWD\"\r";
    let input = format!(
        r#"{{"method":"SessionInput","params":{{"session_id":"{}","data":{}}},"id":6}}"#,
        session_id,
        serde_json::to_string(command.as_slice()).unwrap()
    );
    assert_eq!(send_recv(&mut ws, &mut reader, &input).await["type"], "Ok");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let request = format!(
            r#"{{"method":"ReadScreen","params":{{"session_id":"{}"}},"id":7}}"#,
            session_id
        );
        let screen = send_recv(&mut ws, &mut reader, &request).await;
        let text = screen["result"]["lines"]
            .as_array()
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(|line| line.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if text.contains("preserved:/tmp") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "shell state was not preserved: {text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let close = format!(
        r#"{{"method":"CloseSession","params":{{"session_id":"{}"}},"id":8}}"#,
        session_id
    );
    assert_eq!(send_recv(&mut ws, &mut reader, &close).await["type"], "Ok");
    drop(ws);
    drop(reader);
    unsafe {
        libc::kill(second_daemon.id() as i32, libc::SIGINT);
    }
    second_daemon.wait().unwrap();

    let socket = directory.path().join("session-host.sock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(
        !socket.exists(),
        "empty detached host should exit with daemon"
    );
}

// -----------------------------------------------------------------------
// 配置目录（首次启动）：不存在时自动创建；不可写时给人话错误（不 panic）
// -----------------------------------------------------------------------

/// VIDA_CONFIG_DIR 指向不存在的路径 → 启动成功且目录被创建。
#[test]
fn config_dir_created_if_missing() {
    // poison 容忍：并行测试中其他测试失败会 poison 锁，不应连带失败
    let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let base = tempfile::tempdir().unwrap();
    let missing = base.path().join("not-yet-created");
    assert!(!missing.exists(), "前置条件：目录不应存在");
    unsafe { std::env::set_var("VIDA_CONFIG_DIR", &missing) };

    // 启动路径（与 main.rs 一致）：ensure_dirs 后再写 token
    vida_core::config::ensure_dirs().unwrap();
    let token = vida_daemon::ws_server::load_or_create_token().unwrap();
    assert_eq!(token.len(), 64);
    assert!(
        missing.join("daemon.token").exists(),
        "token 应写入新创建的目录"
    );
    assert!(missing.join("backups").is_dir(), "backups 子目录应被创建");

    unsafe { std::env::remove_var("VIDA_CONFIG_DIR") };
}

/// VIDA_CONFIG_DIR 指向不可写路径 → 报出可读错误信息，不 panic。
#[cfg(unix)]
#[test]
fn config_dir_unwritable_gives_clear_error() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let base = tempfile::tempdir().unwrap();
    let readonly = base.path().join("readonly");
    std::fs::create_dir(&readonly).unwrap();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555)).unwrap();
    unsafe { std::env::set_var("VIDA_CONFIG_DIR", &readonly) };

    let err = vida_core::config::ensure_dirs().expect_err("应返回错误");
    let msg = format!("{:#}", err);
    assert!(
        (msg.contains("无法创建配置目录") || msg.contains("无法创建备份目录"))
            && msg.contains("请检查权限"),
        "错误应是人话（含目录与权限提示），实际: {}",
        msg
    );
    assert!(!msg.contains("os error"), "不应暴露原始 os error: {}", msg);

    // 恢复权限便于 tempdir 清理（即使上面的断言失败也要执行）
    let _ = std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o755));
    unsafe { std::env::remove_var("VIDA_CONFIG_DIR") };
}
