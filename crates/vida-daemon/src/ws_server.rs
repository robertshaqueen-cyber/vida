use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::PushPayload;
use crate::protocol::{PtyRequest, Request, Response, ResponsePayload, SyncResponse};
use crate::pty::push::BoundedReceiver;
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

    // 推送通道：PTY 推送循环 → 桥接任务 → tokio channel → 此处
    let (push_tx, mut push_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let mut push_bridge: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            // WebSocket 输入
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let push_cmd_arc: Arc<Mutex<Option<PushCommand>>> =
                            Arc::new(Mutex::new(None));
                        let response = handle_message(
                            &text, &state, &mut authenticated, &push_cmd_arc,
                        ).await;
                        // 处理推送相关命令（订阅/取消订阅）
                        if let Some(cmd) = push_cmd_arc.lock().await.take() {
                            match cmd {
                                PushCommand::Subscribe(rx) => {
                                    let tx = push_tx.clone();
                                    // 桥接任务：轮询 std BoundedReceiver（try_recv），
                                    // 避免阻塞 tokio runtime 线程。
                                    push_bridge = Some(tokio::spawn(async move {
                                        loop {
                                            match rx.try_recv() {
                                                Some(payload) => {
                                                    if tx.send(payload.bytes).await.is_err() {
                                                        break;
                                                    }
                                                }
                                                None => {
                                                    // 无帧时让出，10ms 后重试
                                                    tokio::time::sleep(
                                                        std::time::Duration::from_millis(5),
                                                    )
                                                    .await;
                                                }
                                            }
                                        }
                                    }));
                                }
                                PushCommand::Unsubscribe => {
                                    if let Some(handle) = push_bridge.take() {
                                        handle.abort();
                                    }
                                }
                            }
                        }
                        if let Err(e) = write.send(Message::Text(response.into())).await {
                            error!("Failed to send response to {}: {}", addr, e);
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("Connection closed by {}", addr);
                        break;
                    }
                    Some(Err(e)) => {
                        // 客户端未做关闭握手就断开（网络中断、进程被 kill）是
                        // 常见且无害的情况，INFO 即可。ERROR 留给真正需要
                        // 用户注意的问题。
                        info!("Connection dropped without close handshake ({}): {}", addr, e);
                        break;
                    }
                    None => break,
                    _ => {}
                }
            }

            // PTY 推送输出
            Some(bytes) = push_rx.recv() => {
                if let Err(e) = write.send(Message::Binary(tokio_tungstenite::tungstenite::Bytes::from(bytes))).await {
                    error!("Failed to push binary frame to {}: {}", addr, e);
                    break;
                }
            }
        }
    }

    // 连接断开：必须中止推送桥接任务。
    // 否则桥接任务仍持有 push_tx（sender clone），channel 不会关闭，
    // 它会继续从会话的 BoundedReceiver 读帧并 send 到无人接收的
    // channel —— 死循环浪费资源。
    if let Some(handle) = push_bridge.take() {
        handle.abort();
    }

    info!("Connection dropped: {}", addr);
}

/// 推送控制命令：handle_message 返回给连接层，用于管理订阅桥接。
enum PushCommand {
    /// 订阅推送：携带 BoundedReceiver，由连接层启动桥接任务。
    Subscribe(BoundedReceiver<PushPayload>),
    /// 取消订阅：中止桥接任务。
    Unsubscribe,
}

async fn handle_message(
    text: &str,
    state: &Arc<Mutex<DaemonState>>,
    authenticated: &mut bool,
    push_cmd_out: &Arc<Mutex<Option<PushCommand>>>,
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

    // 提前取出 pty 的 Arc 引用（避免持 tokio MutexGuard 跨 await）
    let pty_arc = state.lock().await.pty.clone();

    // PTY 订阅/取消订阅：在分发前处理，需写入 push_cmd_out
    if let Request::Pty(PtyRequest::SubscribeSession { session_id }) = &request {
        // 同步获取订阅接收端（持锁时间最短）
        let rx = {
            let pty = match pty_arc.read() {
                Ok(p) => p,
                Err(_) => {
                    return serde_json::to_string(&Response {
                        id,
                        payload: ResponsePayload::Error {
                            code: -5,
                            message: "PTY 锁异常".to_string(),
                            category: None,
                        },
                    })
                    .unwrap();
                }
            };
            match pty.subscribe_session(session_id) {
                Ok(rx) => rx,
                Err(e) => {
                    return serde_json::to_string(&Response {
                        id,
                        payload: ResponsePayload::Error {
                            code: -5,
                            message: format!("{}", e),
                            category: None,
                        },
                    })
                    .unwrap();
                }
            }
        };
        // pty guard 已释放，现在可以安全 await
        push_cmd_out
            .lock()
            .await
            .replace(PushCommand::Subscribe(rx));
        return serde_json::to_string(&Response {
            id,
            payload: ResponsePayload::Ok {
                result: serde_json::json!({"ok": true}),
            },
        })
        .unwrap();
    }
    if let Request::Pty(PtyRequest::UnsubscribeSession { .. }) = &request {
        // 同步检查会话存在性（持锁时间最短）
        let has_sessions = {
            let pty = match pty_arc.read() {
                Ok(p) => p,
                Err(_) => {
                    return serde_json::to_string(&Response {
                        id,
                        payload: ResponsePayload::Error {
                            code: -5,
                            message: "PTY 锁异常".to_string(),
                            category: None,
                        },
                    })
                    .unwrap();
                }
            };
            !pty.list_sessions().is_empty()
        };
        if !has_sessions {
            return serde_json::to_string(&Response {
                id,
                payload: ResponsePayload::Error {
                    code: -5,
                    message: "没有活跃的会话".to_string(),
                    category: None,
                },
            })
            .unwrap();
        }
        // pty guard 已释放，安全 await
        push_cmd_out.lock().await.replace(PushCommand::Unsubscribe);
        return serde_json::to_string(&Response {
            id,
            payload: ResponsePayload::Ok {
                result: serde_json::json!({"ok": true}),
            },
        })
        .unwrap();
    }

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
    // PTY 请求：不持金库锁，只锁 state.pty（独立 RwLock）。
    // PtyRequest 由 #[serde(untagged)] 在反序列化时已分流，
    // 编译器保证 handle_pty_request 穷尽匹配所有 PtyRequest 变体。
    if let Request::Pty(pty_req) = &request {
        return handle_pty_request(pty_req, state).await;
    }

    let mut state = state.lock().await;

    match request {
        // PTY 请求已在 handle_pty_request 处理（上方 if let 提前返回）；
        // 此处不可达，但编译器需要穷尽匹配。
        Request::Pty(_) => unreachable!("PTY 请求已在 handle_pty_request 处理"),

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

/// PTY 会话请求（M2a-2）。只锁 `state.pty`（独立 RwLock），
/// 全程不接触金库锁。编译器强制穷尽匹配所有 PtyRequest 变体。
#[allow(clippy::all, unused_mut)]
async fn handle_pty_request(
    request: &PtyRequest,
    state: &Arc<Mutex<DaemonState>>,
) -> Result<serde_json::Value> {
    // 短暂拿金库锁仅为了取出 pty 的 Arc 引用，随即释放；
    // 后续 PTY 操作只持有 pty 自己的 RwLock，不与金库锁争用。
    let pty = state.lock().await.pty.clone();

    match request {
        PtyRequest::OpenLocalSession { cols, rows } => {
            let mut pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            let session_id = pty.open_session(*cols, *rows)?;
            Ok(serde_json::json!({"session_id": session_id}))
        }
        PtyRequest::SessionInput { session_id, data } => {
            let mut pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            pty.session_input(session_id, data)?;
            Ok(serde_json::json!({"ok": true}))
        }
        PtyRequest::ResizeSession {
            session_id,
            cols,
            rows,
        } => {
            let mut pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            pty.resize_session(session_id, *cols, *rows)?;
            Ok(serde_json::json!({"ok": true}))
        }
        PtyRequest::CloseSession { session_id } => {
            let mut pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            pty.close_session(session_id)?;
            Ok(serde_json::json!({"ok": true}))
        }
        PtyRequest::ListSessions => {
            let pty = pty.read().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            let sessions = pty.list_sessions();
            Ok(serde_json::to_value(sessions)?)
        }
        PtyRequest::ReadScreen { session_id } => {
            let pty = pty.read().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            let screen = pty.read_screen(session_id)?;
            Ok(serde_json::to_value(screen)?)
        }
        PtyRequest::ReadScreenStyled { session_id } => {
            let pty = pty.read().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            let screen = pty.read_screen_styled(session_id)?;
            Ok(serde_json::to_value(screen)?)
        }
        // SubscribeSession / UnsubscribeSession 已在 handle_message 层面处理
        PtyRequest::SubscribeSession { .. } | PtyRequest::UnsubscribeSession { .. } => {
            anyhow::bail!("PTY 订阅请求未在上游处理（内部错误）")
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

    if lower.contains("wrong passphrase")
        || lower.contains("auth failed")
        || lower.contains("hmac mismatch")
    {
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
