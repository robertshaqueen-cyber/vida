use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::protocol::{Request, Response, ResponsePayload, SyncResponse};
use crate::state::DaemonState;
use vida_core::sync::SyncResult;

// ---------------------------------------------------------------------------
// Token management
// ---------------------------------------------------------------------------

fn generate_token() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..32)
        .map(|_| format!("{:02x}", rng.random::<u8>()))
        .collect()
}

fn token_path() -> Result<std::path::PathBuf> {
    Ok(vida_core::config::config_dir()?.join("daemon.token"))
}

pub fn load_or_create_token() -> Result<String> {
    let path = token_path()?;
    if path.exists() {
        let token = std::fs::read_to_string(&path).context("Failed to read daemon.token")?;
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    let token = generate_token();
    std::fs::write(&path, &token).context("Failed to write daemon.token")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    info!("Generated new daemon token: {}", &token[..8]);
    Ok(token)
}

pub fn cleanup_token() {
    let _ = std::fs::remove_file(token_path().unwrap_or_default());
    let _ = std::fs::remove_file(port_path().unwrap_or_default());
}

fn port_path() -> Result<std::path::PathBuf> {
    Ok(vida_core::config::config_dir()?.join("daemon.port"))
}

fn tokens_match(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    if a_bytes.len() != b_bytes.len() {
        return false;
    }
    ConstantTimeEq::ct_eq(a_bytes, b_bytes).into()
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(stream: TcpStream, addr: SocketAddr, state: Arc<Mutex<DaemonState>>) {
    info!("New WebSocket connection from {}", addr);

    let ws_stream = match tokio_tungstenite::accept_hdr_async(
        stream,
        #[allow(clippy::result_large_err)]
        |req: &tokio_tungstenite::tungstenite::http::Request<()>,
         resp: tokio_tungstenite::tungstenite::http::Response<()>| {
            if req.headers().contains_key("Origin") {
                warn!("Rejected connection from {} with Origin header", addr);
                return Err(tokio_tungstenite::tungstenite::http::Response::builder()
                    .status(403)
                    .body(Some("Origin header not allowed".to_string()))
                    .unwrap());
            }
            Ok(resp)
        },
    )
    .await
    {
        Ok(ws) => ws,
        Err(e) => {
            error!("WebSocket handshake failed for {}: {}", addr, e);
            return;
        }
    };

    let (mut write, mut read) = ws_stream.split();
    let mut authenticated = false;

    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let response = handle_message(&text, &state, &mut authenticated).await;
                if let Err(e) = write.send(Message::Text(response.into())).await {
                    error!("Failed to send response to {}: {}", addr, e);
                    break;
                }
            }
            Ok(Message::Close(_)) => {
                info!("Connection closed by {}", addr);
                break;
            }
            Err(e) => {
                error!("Error reading from {}: {}", addr, e);
                break;
            }
            _ => {}
        }
    }

    info!("Connection dropped: {}", addr);
}

async fn handle_message(
    text: &str,
    state: &Arc<Mutex<DaemonState>>,
    authenticated: &mut bool,
) -> String {
    // Clone i18n once (lightweight, language table only) for all messages in this call
    let i18n = state.lock().await.i18n.clone();

    let raw: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::to_string(&Response {
                id: 0,
                payload: ResponsePayload::Error {
                    code: -1,
                    message: format!("Invalid JSON: {}", e),
                    category: Some("invalid_json".to_string()),
                },
            })
            .unwrap();
        }
    };

    let id = raw.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
    let method = raw.get("method").and_then(|v| v.as_str()).unwrap_or("");

    // Auth check: first message must be "auth"
    if !*authenticated {
        if method != "Auth" {
            return serde_json::to_string(&Response {
                id,
                payload: ResponsePayload::Error {
                    code: -2,
                    message: i18n.tr("daemon_ws_not_authed").to_string(),
                    category: Some("not_authed".to_string()),
                },
            })
            .unwrap();
        }

        let token = raw
            .get("params")
            .and_then(|p| p.get("token"))
            .and_then(|t| t.as_str())
            .unwrap_or("");

        let state_token = state.lock().await.token.clone();

        if !tokens_match(token, &state_token) {
            return serde_json::to_string(&Response {
                id,
                payload: ResponsePayload::Error {
                    code: -3,
                    message: i18n.tr("daemon_ws_auth_failed").to_string(),
                    category: Some("auth_failed".to_string()),
                },
            })
            .unwrap();
        }

        *authenticated = true;
        info!("Client authenticated");
        return serde_json::to_string(&Response {
            id,
            payload: ResponsePayload::Ok {
                result: serde_json::json!({"authenticated": true}),
            },
        })
        .unwrap();
    }

    // Parse request
    let request: Request = match serde_json::from_value(raw.clone()) {
        Ok(r) => r,
        Err(e) => {
            return serde_json::to_string(&Response {
                id,
                payload: ResponsePayload::Error {
                    code: -4,
                    message: i18n.trf("daemon_ws_unknown_request", &[&e.to_string()]),
                    category: Some("unknown_request".to_string()),
                },
            })
            .unwrap();
        }
    };

    let result = handle_request(request, state).await;

    match result {
        Ok(payload) => serde_json::to_string(&Response {
            id,
            payload: ResponsePayload::Ok { result: payload },
        })
        .unwrap(),
        Err(e) => {
            let msg = format!("{:#}", e);
            warn!("Request {} failed: {}", method, msg);
            
            // Classify error based on message content
            let category = classify_error(&msg);
            
            serde_json::to_string(&Response {
                id,
                payload: ResponsePayload::Error {
                    code: -5,
                    message: msg,
                    category,
                },
            })
            .unwrap()
        }
    }
}

async fn handle_request(
    request: Request,
    state: &Arc<Mutex<DaemonState>>,
) -> Result<serde_json::Value> {
    let mut state = state.lock().await;

    match request {
        Request::Auth { .. } => unreachable!(),

        // Vault
        Request::CreateVault { passphrase } => {
            let info = state.create_vault(&passphrase)?;
            Ok(serde_json::to_value(info)?)
        }
        Request::Unlock {
            passphrase,
            remember,
        } => {
            let info = state.unlock(&passphrase, remember)?;
            Ok(serde_json::to_value(info)?)
        }
        Request::Lock => {
            state.lock();
            Ok(serde_json::json!({"locked": true}))
        }
        Request::VaultStatus => {
            let info = state.vault_status();
            Ok(serde_json::to_value(info)?)
        }
        Request::ExportBackup { passphrase } => {
            let data = state.export_backup(passphrase.as_deref())?;
            Ok(serde_json::json!({"bytes": data.len()}))
        }

        // Settings
        Request::GetSettings => {
            let settings = state.get_settings()?;
            Ok(serde_json::to_value(settings)?)
        }
        Request::UpdateSettings { settings } => {
            state.update_settings(settings)?;
            Ok(serde_json::json!({"updated": true}))
        }

        // Hosts
        Request::ListHosts => {
            let hosts = state.list_hosts()?;
            Ok(serde_json::to_value(hosts)?)
        }
        Request::RevealCredential { host_id } => {
            let credential = state.reveal_credential(&host_id)?;
            Ok(serde_json::json!({"credential": credential}))
        }
        Request::UpdateHost { host } => {
            let info = state.update_host(host)?;
            Ok(serde_json::to_value(info)?)
        }
        Request::DeleteHost { host_id } => {
            state.delete_host(&host_id)?;
            Ok(serde_json::json!({"deleted": true}))
        }

        // Sync
        Request::Sync => {
            let (result, hosts) = state.sync().await?;

            // Extract files and remote_hosts from SyncResult
            let (files, remote_hosts) = match &result {
                SyncResult::ConflictFilesDetected { files } => (Some(files.clone()), None),
                SyncResult::Conflict {
                    remote_ciphertext, ..
                } => {
                    let remote_hosts = remote_ciphertext
                        .as_ref()
                        .and_then(|ct| state.decrypt_remote_hosts(ct).ok());
                    (None, remote_hosts)
                }
                _ => (None, None),
            };

            let resp = SyncResponse {
                status: sync_status_string(&result),
                remote_meta: extract_remote_meta(&result),
                hosts,
                files,
                remote_hosts,
            };
            Ok(serde_json::to_value(resp)?)
        }
        Request::ResolveConflict { choice } => {
            let (hosts,) = state.resolve_conflict(choice).await?;
            Ok(serde_json::json!({
                "resolved": true,
                "hosts": hosts,
            }))
        }

        // Conflict files
        Request::ReadConflictFile { path } => {
            let hosts = state.read_conflict_file(&path)?;
            Ok(serde_json::to_value(hosts)?)
        }
        Request::AdoptConflictFile { path } => {
            let hosts = state.adopt_conflict_file(&path)?;
            Ok(serde_json::json!({
                "adopted": true,
                "hosts": hosts,
            }))
        }
        Request::IgnoreConflictFile { path } => {
            state.ignore_conflict_file(&path)?;
            Ok(serde_json::json!({"ignored": true}))
        }

        // Remote missing
        Request::HandleRemoteMissing { action } => {
            state.handle_remote_missing(&action)?;
            Ok(serde_json::json!({"handled": true}))
        }
    }
}

fn sync_status_string(result: &SyncResult) -> String {
    match result {
        SyncResult::Uploaded { .. } => "uploaded".to_string(),
        SyncResult::Downloaded { .. } => "downloaded".to_string(),
        SyncResult::Conflict { .. } => "conflict".to_string(),
        SyncResult::NoChange => "no_change".to_string(),
        SyncResult::RemoteMissing => "remote_missing".to_string(),
        SyncResult::ConflictFilesDetected { .. } => "conflict_files_detected".to_string(),
        SyncResult::SyncNotConfigured => "sync_not_configured".to_string(),
    }
}

fn extract_remote_meta(result: &SyncResult) -> Option<serde_json::Value> {
    match result {
        SyncResult::Uploaded { new_meta } => serde_json::to_value(new_meta).ok(),
        SyncResult::Downloaded { meta, .. } => serde_json::to_value(meta).ok(),
        SyncResult::Conflict { remote_meta, .. } => serde_json::to_value(remote_meta).ok(),
        _ => None,
    }
}

/// Classify error based on message content for GUI i18n display.
///
/// This is a transitional approach. Goal: carry category from the error source
/// (VidaError { category, detail }) so downstream doesn't match on text.
fn classify_error(msg: &str) -> Option<String> {
    let lower = msg.to_lowercase();
    
    if lower.contains("wrong passphrase") || lower.contains("auth failed") || lower.contains("hmac mismatch") {
        Some("wrong_passphrase".to_string())
    } else if lower.contains("corrupted data") || lower.contains("failed to parse vault json") {
        // Narrowed: only match vault-specific corruption messages from core.
        // Removed "invalid" (too broad: port invalid, path invalid, etc.)
        // and "parse" (too broad: JSON field invalid, config parse, etc.)
        Some("vault_corrupted".to_string())
    } else if lower.contains("not found") || lower.contains("missing") {
        Some("not_found".to_string())
    } else if lower.contains("locked") {
        Some("vault_locked".to_string())
    } else if lower.contains("exists") {
        Some("vault_exists".to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Server startup
// ---------------------------------------------------------------------------

pub async fn start(state: Arc<Mutex<DaemonState>>) -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // Write port file so GUI can discover the port
    let port_file = port_path()?;
    std::fs::write(&port_file, addr.port().to_string())
        .with_context(|| format!("Failed to write daemon port to {}", port_file.display()))?;

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let state = Arc::clone(&state);
                    tokio::spawn(handle_connection(stream, addr, state));
                }
                Err(e) => {
                    error!("Accept error: {}", e);
                }
            }
        }
    });

    Ok(addr)
}
