use vida_daemon::protocol::{HostRequest, ConflictChoice};
use vida_daemon::state::DaemonState;
use vida_core::sync::{LocalPathBackend, SyncCoordinator, SyncResult};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

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
    };
    (state, dir)
}

fn add_test_host(state: &mut DaemonState, name: &str) {
    state.update_host(HostRequest {
        id: None,
        name: name.to_string(),
        host: "10.0.0.1".to_string(),
        user: "root".to_string(),
        port: 22,
        tags: vec![],
        group: None,
        color: None,
        password: Some("secret-password-123".to_string()),
        notes: None,
    }).unwrap();
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
    };

    let state = Arc::new(Mutex::new(state));
    let addr = vida_daemon::ws_server::start(state).await.unwrap();
    (addr, token, dir)
}

async fn connect(addr: std::net::SocketAddr) -> (
    futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
    futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
) {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr)).await.unwrap();
    ws.split()
}

async fn send_recv(
    ws: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
    reader: &mut futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    msg: &str,
) -> serde_json::Value {
    ws.send(Message::Text(msg.to_string().into())).await.unwrap();
    match reader.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected Text, got {:?}", other),
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
    assert_eq!(resp["code"], -2, "should return unauthenticated error: {}", resp);
    let resp_str = resp.to_string();
    assert!(!resp_str.contains("10.0.0.1"), "must not leak host data: {}", resp_str);
}

#[tokio::test]
async fn ws_wrong_token_rejected() {
    let (addr, _token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    let resp = send_recv(
        &mut ws, &mut reader,
        r#"{"method":"Auth","params":{"token":"wrong-token"},"id":1}"#,
    ).await;

    assert_eq!(resp["code"], -3, "should return token mismatch: {}", resp);
}

#[tokio::test]
async fn ws_correct_token_allows_list_hosts() {
    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    let auth_msg = format!(r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#, token);
    let resp = send_recv(&mut ws, &mut reader, &auth_msg).await;
    assert!(resp["result"]["authenticated"].as_bool().unwrap());

    // Create vault so list_hosts won't fail with "vault locked"
    let resp = send_recv(&mut ws, &mut reader, r#"{"method":"CreateVault","params":{"passphrase":"test-pass"},"id":2}"#).await;
    assert!(resp["result"].is_object(), "CreateVault should succeed: {}", resp);

    let resp = send_recv(&mut ws, &mut reader, r#"{"method":"ListHosts","id":3}"#).await;
    assert!(resp["result"].is_array(), "should return host list: {}", resp);
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
    assert!(result.is_err(),
        "handshake with Origin + correct token must be rejected at HTTP level, got: {:?}",
        result.ok());

    // Case 2: Origin alone (no token) → same rejection
    let request2 = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(format!("ws://{}", addr))
        .header("Origin", "http://evil.com")
        .body(())
        .unwrap();
    let result2 = tokio_tungstenite::connect_async(request2).await;
    assert!(result2.is_err(), "handshake with Origin alone must also be rejected");
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
        token: "t".to_string(), vault: None, passphrase: None,
        vault_path: dir.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state.vault = Some(vida_core::vault::Vault::default());
    state.passphrase = Some("pass".to_string());
    let backend = Box::new(LocalPathBackend::new(dir.path().to_path_buf()));
    state.sync = Some(SyncCoordinator::with_state(backend, state.vault_path.clone(), None));

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
        });
        v.revision += 1;
    }

    // First sync: no remote file → Uploaded
    let (result, _) = tokio_test::block_on(state.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }), "expected Uploaded, got {:?}", result);

    // Delete the vault file (simulate remote deletion)
    std::fs::remove_file(&state.vault_path).unwrap();

    // Don't change local vault — only remote is missing
    // Sync → should detect remote missing
    let (result, _) = tokio_test::block_on(state.sync()).unwrap();
    assert!(matches!(result, SyncResult::RemoteMissing), "expected RemoteMissing, got {:?}", result);
}

#[test]
fn remote_missing_clear_state_removes_sync_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = DaemonState {
        token: "t".to_string(), vault: None, passphrase: None,
        vault_path: dir.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    // Set up without writing file
    state.vault = Some(vida_core::vault::Vault::default());
    state.passphrase = Some("pass".to_string());
    let backend = Box::new(LocalPathBackend::new(dir.path().to_path_buf()));
    state.sync = Some(SyncCoordinator::with_state(backend, state.vault_path.clone(), None));

    // Sync to establish state (uploads to non-existent remote)
    let (result, _) = tokio_test::block_on(state.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }));

    let state_path = vida_core::config::config_dir().unwrap().join("sync_state.json");
    assert!(state_path.exists(), "sync state should exist");

    state.handle_remote_missing("clear_state").unwrap();

    assert!(!state_path.exists(), "sync state should be deleted after clear_state");
}

// -----------------------------------------------------------------------
// Write failure → SyncState not updated
// -----------------------------------------------------------------------

#[test]
fn write_failure_does_not_update_sync_state() {
    let dir = tempfile::tempdir().unwrap();
    let vault_path = dir.path().join("vault.age");

    let mut state = DaemonState {
        token: "t".to_string(), vault: None, passphrase: None,
        vault_path: vault_path.clone(), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state.create_vault("pass").unwrap();

    let backend = Box::new(LocalPathBackend::new(dir.path().to_path_buf()));
    let mut sync = SyncCoordinator::with_state(backend, vault_path.clone(), None);
    let ct = vida_core::vault::encrypt(state.vault.as_ref().unwrap(), "pass").unwrap();
    let local = vida_core::sync::LocalVaultInfo {
        ciphertext: ct, revision: 1, device_id: "test".to_string(),
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
        ciphertext: ct2, revision: 1, device_id: "test".to_string(),
    };
    let _ = tokio_test::block_on(sync.sync(&local2));

    let hash_after = sync.state().map(|s| s.last_synced_hash.clone());
    assert_eq!(hash_before, hash_after, "SyncState must not change on NoChange");

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
        let perms = std::fs::metadata(dir.path().join("daemon.token")).unwrap().permissions();
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
    let info = state.unlock("mypass", false).unwrap();
    assert!(!info.locked);
}

#[test]
fn unlock_wrong_passphrase_fails() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("correct").unwrap();
    state.lock();
    assert!(state.unlock("wrong", false).is_err());
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
    assert_eq!(state.reveal_credential(&host_id).unwrap(), "secret-password-123");
}

#[test]
fn add_host_with_password() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    let summary = state.update_host(HostRequest {
        id: None, name: "web".to_string(), host: "1.2.3.4".to_string(),
        user: "admin".to_string(), port: 22, tags: vec!["prod".to_string()],
        group: Some("servers".to_string()), color: Some("#ff0000".to_string()),
        password: Some("mypassword".to_string()), notes: Some("notes".to_string()),
    }).unwrap();
    assert_eq!(summary.name, "web");
}

#[test]
fn update_host_keep_credential() {
    let (mut state, _dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    add_test_host(&mut state, "original");
    let host_id = state.list_hosts().unwrap()[0].id.clone();
    state.update_host(HostRequest {
        id: Some(host_id.clone()), name: "updated".to_string(), host: "10.0.0.1".to_string(),
        user: "root".to_string(), port: 22, tags: vec![], group: None, color: None,
        password: None, notes: None,
    }).unwrap();
    assert_eq!(state.reveal_credential(&host_id).unwrap(), "secret-password-123");
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
        ..Default::default()
    };
    state.update_settings(settings).unwrap();
    state.lock();
    let ct = std::fs::read(&state.vault_path).unwrap();
    let vault = vida_core::vault::decrypt(&ct, "pass").unwrap();
    assert_eq!(vault.settings.scrollback_lines, 10000);
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
    let hosts = state.read_conflict_file(conflict_path.to_str().unwrap()).unwrap();
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
        id: uuid::Uuid::new_v4().to_string(), name: "conflict-host".to_string(),
        host: "192.168.1.1".to_string(), user: "admin".to_string(), port: 22,
        tags: vec![], group: None, color: None,
        auth: vida_core::vault::AuthMethod::Password {
            password: vida_core::vault::SecureString::new("conflict-pass".to_string()),
        },
        notes: None,
    });
    let ct = vida_core::vault::encrypt(&conflict_vault, "pass").unwrap();
    let conflict_path = dir.path().join("vault (conflicted copy).age");
    std::fs::write(&conflict_path, &ct).unwrap();

    let hosts = state.adopt_conflict_file(conflict_path.to_str().unwrap()).unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].name, "conflict-host");
    assert!(!conflict_path.exists());
    assert!(dir.path().join("vault (conflicted copy).age.reviewed").exists());
}

#[test]
fn ignore_conflict_file_renames() {
    let (mut state, dir) = test_state("tok");
    state.create_vault("pass").unwrap();
    let conflict_path = dir.path().join("vault (conflicted copy).age");
    std::fs::write(&conflict_path, b"fake").unwrap();
    state.ignore_conflict_file(conflict_path.to_str().unwrap()).unwrap();
    assert!(!conflict_path.exists());
    assert!(dir.path().join("vault (conflicted copy).age.reviewed").exists());
}

#[test]
fn downloaded_writes_file_and_updates_vault() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    // Device A: create vault with host
    let mut state_a = DaemonState {
        token: "a".to_string(), vault: None, passphrase: None,
        vault_path: dir_a.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state_a.create_vault("pass").unwrap();
    add_test_host(&mut state_a, "host-from-a");

    // Device B: set up without writing file
    let mut state_b = DaemonState {
        token: "b".to_string(), vault: None, passphrase: None,
        vault_path: dir_b.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state_b.vault = Some(vida_core::vault::Vault::default());
    state_b.passphrase = Some("pass".to_string());
    let backend_b = Box::new(LocalPathBackend::new(dir_b.path().to_path_buf()));
    state_b.sync = Some(SyncCoordinator::with_state(backend_b, state_b.vault_path.clone(), None));
    assert_eq!(state_b.list_hosts().unwrap().len(), 0);

    // B uploads empty vault to establish state
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }));

    // A's file overwrites B's
    let ct_a = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a).unwrap();

    // B syncs → Downloaded
    let (result, hosts) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Downloaded { .. }), "expected Downloaded, got {:?}", result);
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
        token: "a".to_string(), vault: None, passphrase: None,
        vault_path: dir_a.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state_a.create_vault("pass").unwrap();
    add_test_host(&mut state_a, "host-from-a");

    // Device B: set up without writing file
    let mut state_b = DaemonState {
        token: "b".to_string(), vault: None, passphrase: None,
        vault_path: dir_b.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state_b.vault = Some(vida_core::vault::Vault::default());
    state_b.passphrase = Some("pass".to_string());
    let backend_b = Box::new(LocalPathBackend::new(dir_b.path().to_path_buf()));
    state_b.sync = Some(SyncCoordinator::with_state(backend_b, state_b.vault_path.clone(), None));

    // B uploads empty vault to establish state
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }));

    // A's file overwrites B's → B syncs → Downloaded (establishes sync state)
    let ct_a = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a).unwrap();
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Downloaded { .. }));

    // Record SyncState before the failed sync
    let hash_before = state_b.sync.as_ref().unwrap().state().map(|s| s.last_synced_hash.clone());
    let rev_before = state_b.sync.as_ref().unwrap().state().map(|s| s.last_synced_revision);

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
    assert!(result.is_err(), "sync should fail when directory is read-only, got: {:?}", result.ok());

    // SyncState must be unchanged — system still knows it hasn't synced
    let hash_after = state_b.sync.as_ref().unwrap().state().map(|s| s.last_synced_hash.clone());
    let rev_after = state_b.sync.as_ref().unwrap().state().map(|s| s.last_synced_revision);
    assert_eq!(hash_before, hash_after, "SyncState hash must not change on write failure");
    assert_eq!(rev_before, rev_after, "SyncState revision must not change on write failure");

    // Fix permissions and sync again → should still return Downloaded (retry succeeds)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir_b.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (result, hosts) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Downloaded { .. }), "after fixing permissions, sync should succeed with Downloaded, got {:?}", result);
    assert!(hosts.is_some());
    assert!(hosts.unwrap().iter().any(|h| h.name == "host-from-a2"));
}

#[test]
fn conflict_resolve_remote_replaces_vault() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    // Device A: create vault with host
    let mut state_a = DaemonState {
        token: "a".to_string(), vault: None, passphrase: None,
        vault_path: dir_a.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state_a.create_vault("pass").unwrap();
    add_test_host(&mut state_a, "host-a");

    // Device B: set up without writing file
    let mut state_b = DaemonState {
        token: "b".to_string(), vault: None, passphrase: None,
        vault_path: dir_b.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state_b.vault = Some(vida_core::vault::Vault::default());
    state_b.passphrase = Some("pass".to_string());
    let backend_b = Box::new(LocalPathBackend::new(dir_b.path().to_path_buf()));
    state_b.sync = Some(SyncCoordinator::with_state(backend_b, state_b.vault_path.clone(), None));

    // B uploads empty vault to establish state
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }));

    // A's file overwrites B's
    let ct_a = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a).unwrap();

    // B syncs → Downloaded
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Downloaded { .. }), "expected Downloaded, got {:?}", result);

    // Both sides change
    add_test_host(&mut state_a, "host-a2");
    add_test_host(&mut state_b, "host-b");
    let ct_a2 = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a2).unwrap();

    // B syncs → Conflict
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Conflict { .. }), "expected Conflict, got {:?}", result);

    // Resolve choosing remote
    let (hosts,) = tokio_test::block_on(state_b.resolve_conflict(ConflictChoice::Remote)).unwrap();
    assert!(hosts.iter().any(|h| h.name == "host-a" || h.name == "host-a2"));

    // Verify no write-back
    add_test_host(&mut state_b, "new-after-resolve");
    let ct_check = std::fs::read(&state_b.vault_path).unwrap();
    let vault_check = vida_core::vault::decrypt(&ct_check, "pass").unwrap();
    assert!(!vault_check.hosts.iter().any(|h| h.name == "host-b"), "old local host-b must not be written back");
}

// -----------------------------------------------------------------------
// GetSettings IPC test
// -----------------------------------------------------------------------

#[tokio::test]
async fn get_settings_returns_vault_settings() {
    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    // Auth
    send_recv(&mut ws, &mut reader, &format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#, token
    )).await;

    // Create vault
    send_recv(&mut ws, &mut reader, r#"{"method":"CreateVault","params":{"passphrase":"test123"},"id":2}"#).await;

    // Get settings — should return defaults
    let resp = send_recv(&mut ws, &mut reader, r#"{"method":"GetSettings","id":3}"#).await;
    assert_eq!(resp["type"], "Ok", "GetSettings should succeed: {}", resp);
    let result = &resp["result"];
    assert_eq!(result["scrollback_lines"], 5000, "default scrollback_lines");
    assert!(result["sync_local_path"].is_null(), "default sync_local_path is null");
}

#[tokio::test]
async fn sync_response_includes_new_fields() {
    let (addr, token, _dir) = start_daemon().await;
    let (mut ws, mut reader) = connect(addr).await;

    // Auth
    send_recv(&mut ws, &mut reader, &format!(
        r#"{{"method":"Auth","params":{{"token":"{}"}},"id":1}}"#, token
    )).await;

    // Create vault
    send_recv(&mut ws, &mut reader, r#"{"method":"CreateVault","params":{"passphrase":"test123"},"id":2}"#).await;

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
        token: "a".to_string(), vault: None, passphrase: None,
        vault_path: dir_a.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state_a.create_vault("pass").unwrap();
    add_test_host(&mut state_a, "host-a");

    // Device B: vault in memory only (no file on disk)
    let mut state_b = DaemonState {
        token: "b".to_string(), vault: None, passphrase: None,
        vault_path: dir_b.path().join("vault.age"), sync: None,
        i18n: vida_core::i18n::I18n::default(),
    };
    state_b.vault = Some(vida_core::vault::Vault::default());
    state_b.passphrase = Some("pass".to_string());
    let backend_b = Box::new(LocalPathBackend::new(dir_b.path().to_path_buf()));
    state_b.sync = Some(SyncCoordinator::with_state(backend_b, state_b.vault_path.clone(), None));

    // B1: upload empty vault to establish state (no file on disk → upload)
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Uploaded { .. }), "B1 should be Uploaded, got: {:?}", result);

    // A's file overwrites B's → B2: sync → Downloaded (B's local unchanged)
    let ct_a = std::fs::read(&state_a.vault_path).unwrap();
    std::fs::write(&state_b.vault_path, &ct_a).unwrap();
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    assert!(matches!(result, SyncResult::Downloaded { .. }), "B2 should be Downloaded, got: {:?}", result);

    // NOW both sides change → B3: sync → Conflict
    add_test_host(&mut state_a, "host-a2");
    add_test_host(&mut state_b, "host-b");
    let (result, _) = tokio_test::block_on(state_b.sync()).unwrap();
    match &result {
        SyncResult::Conflict { remote_ciphertext, .. } => {
            assert!(remote_ciphertext.is_some(), "Conflict must have remote_ciphertext");
            let remote = state_b.decrypt_remote_hosts(remote_ciphertext.as_ref().unwrap()).unwrap();
            assert!(!remote.is_empty(), "remote_hosts must not be empty");
            assert!(remote.iter().any(|h| h.name == "host-a" || h.name == "host-a2"),
                "remote_hosts should contain A's hosts: {:?}", remote);
            for host in &remote {
                assert!(host.auth_kind == "password" || host.auth_kind == "key" || host.auth_kind == "key_inline",
                    "auth_kind must be sanitized, got: {}", host.auth_kind);
            }
        }
        other => panic!("expected Conflict, got {:?}", other),
    }
}
