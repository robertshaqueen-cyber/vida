use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::PushPayload;
use crate::agent::{AgentController, CommandDecision, SessionTarget};
use crate::protocol::{PtyRequest, Request, Response, ResponsePayload, SyncResponse};
use crate::pty::SessionInfo;
use crate::pty::push::{BoundedReceiver, PushKind};
use crate::state::DaemonState;
use vida_core::sync::SyncResult;

/// Agent-facing session identity. PTY owns the live process facts while the
/// Agent binding and device-local GUI layout own what that process represents.
#[derive(Debug, serde::Serialize)]
struct ListedSession {
    #[serde(flatten)]
    process: SessionInfo,
    title: String,
    target_kind: &'static str,
    host_id: Option<String>,
    host_name: Option<String>,
}

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
            write_agent_token(&token)?;
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
    write_agent_token(&token)?;
    Ok(token)
}

fn derive_agent_token(owner_token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(
        format!("vida-agent:{owner_token}").as_bytes(),
    ))
}

fn write_agent_token(owner_token: &str) -> Result<()> {
    let path = vida_core::config::config_dir()?.join("agent.token");
    std::fs::write(&path, derive_agent_token(owner_token))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn cleanup_token() {
    let _ = std::fs::remove_file(token_path().unwrap_or_default());
    let _ = std::fs::remove_file(port_path().unwrap_or_default());
    let _ = std::fs::remove_file(
        vida_core::config::config_dir()
            .unwrap_or_default()
            .join("agent.token"),
    );
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientRole {
    Owner,
    Agent,
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    state: Arc<Mutex<DaemonState>>,
    agent: Arc<Mutex<AgentController>>,
) {
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
    let mut role = None;
    let mut agent_event_rx = agent.lock().await.subscribe_events();

    // 推送通道：PTY 推送循环 → 桥接任务 → tokio channel → 此处
    // (session_id, payload)：多路复用，一个连接可订阅多个会话。
    let (push_tx, mut push_rx) = tokio::sync::mpsc::channel::<(String, PushPayload)>(16);
    // 每个会话一个桥接任务（key = session_id）
    let mut push_bridges: std::collections::HashMap<String, tokio::task::JoinHandle<()>> =
        std::collections::HashMap::new();

    loop {
        tokio::select! {
            // WebSocket 输入
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let push_cmd_arc: Arc<Mutex<Option<PushCommand>>> =
                            Arc::new(Mutex::new(None));
                        let response = handle_message(
                            &text, &state, &agent, &mut role, &push_cmd_arc,
                        ).await;
                        // 处理推送相关命令（订阅/取消订阅）
                        if let Some(cmd) = push_cmd_arc.lock().await.take() {
                            match cmd {
                                PushCommand::Subscribe(rx, sid) => {
                                    let tx = push_tx.clone();
                                    let sid2 = sid.clone();
                                    // 桥接任务：轮询 std BoundedReceiver（try_recv），
                                    // 避免阻塞 tokio runtime 线程。按 session_id 存储，
                                    // 同一连接可订阅多个会话（M2b-1 多路复用）。
                                    if let Some(old) = push_bridges.insert(sid, tokio::spawn(async move {
                                        loop {
                                            match rx.try_recv() {
                                                Some(payload) => {
                                                    // 直接传递 payload，由连接层区分
                                                    // 二进制帧 vs session_closed 事件
                                                    if tx.send((sid2.clone(), payload)).await.is_err() {
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
                                    })) {
                                        // 重复订阅同一会话：abort 旧桥接任务
                                        old.abort();
                                    }
                                }
                                PushCommand::Unsubscribe(sid) => {
                                    if let Some(handle) = push_bridges.remove(&sid) {
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
                        // 常见且无害的情况，DEBUG 即可。ERROR 留给真正需要
                        // 用户注意的问题。
                        debug!("Connection dropped without close handshake ({}): {}", addr, e);
                        break;
                    }
                    None => break,
                    _ => {}
                }
            }

            // PTY 推送输出：二进制帧 或 session_closed 事件（含所属会话 id）
            Some((sid, payload)) = push_rx.recv() => {
                match payload.kind {
                    PushKind::Frame => {
                        if let Err(e) = write.send(Message::Binary(tokio_tungstenite::tungstenite::Bytes::from(payload.bytes))).await {
                            error!("Failed to push binary frame to {}: {}", addr, e);
                            break;
                        }
                    }
                    PushKind::SessionClosed { exit_code } => {
                        if !payload.bytes.is_empty()
                            && let Err(e) = write
                                .send(Message::Binary(
                                    tokio_tungstenite::tungstenite::Bytes::from(payload.bytes),
                                ))
                                .await
                        {
                            error!("Failed to push final binary frame to {}: {}", addr, e);
                            break;
                        }
                        // 规格 5.3：shell 退出时推送事件
                        let event = serde_json::json!({
                            "type": "Event",
                            "event": "session_closed",
                            "data": {
                                "session_id": sid,
                                "exit_code": exit_code,
                            },
                        });
                        if let Err(e) = write.send(Message::Text(event.to_string().into())).await {
                            error!("Failed to push session_closed event to {}: {}", addr, e);
                            break;
                        }
                    }
                }
            }

            event = agent_event_rx.recv() => {
                match event {
                    Ok(event) if role == Some(ClientRole::Owner) => {
                        let event = serde_json::json!({
                            "type": "Event",
                            "event": "agent_approval",
                            "data": event,
                        });
                        if let Err(error) = write.send(Message::Text(event.to_string().into())).await {
                            debug!("Failed to push Agent approval event to {}: {}", addr, error);
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        warn!("Agent approval event receiver for {} lagged by {} events", addr, count);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // 连接断开：必须中止全部推送桥接任务。
    // 否则桥接任务仍持有 push_tx（sender clone），channel 不会关闭，
    // 它会继续从会话的 BoundedReceiver 读帧并 send 到无人接收的
    // channel —— 死循环浪费资源。会话本身保留（不关闭）。
    for (sid, handle) in push_bridges.drain() {
        handle.abort();
        debug!("aborted push bridge for session {}", sid);
    }

    info!("Connection dropped: {}", addr);
}

/// 推送控制命令：handle_message 返回给连接层，用于管理订阅桥接。
enum PushCommand {
    /// 订阅推送：携带 BoundedReceiver + session_id，由连接层启动桥接任务。
    Subscribe(BoundedReceiver<PushPayload>, String),
    /// 取消订阅：按 session_id 中止对应桥接任务。
    Unsubscribe(String),
}

async fn handle_message(
    text: &str,
    state: &Arc<Mutex<DaemonState>>,
    agent: &Arc<Mutex<AgentController>>,
    role: &mut Option<ClientRole>,
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
    if role.is_none() {
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
        let requested_role = raw
            .get("params")
            .and_then(|p| p.get("role"))
            .and_then(|value| value.as_str())
            .unwrap_or("owner");

        let state_token = state.lock().await.token.clone();
        let authenticated_role = if requested_role == "agent"
            && tokens_match(token, &derive_agent_token(&state_token))
        {
            Some(ClientRole::Agent)
        } else if requested_role == "owner" && tokens_match(token, &state_token) {
            Some(ClientRole::Owner)
        } else {
            None
        };

        if authenticated_role.is_none() {
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

        *role = authenticated_role;
        info!("Client authenticated as {:?}", role.unwrap());
        return serde_json::to_string(&Response {
            id,
            payload: ResponsePayload::Ok {
                result: serde_json::json!({
                    "authenticated": true,
                    "role": match role.unwrap() {
                        ClientRole::Owner => "owner",
                        ClientRole::Agent => "agent",
                    }
                }),
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

    let role = role.expect("authenticated role established above");
    if !request_allowed_for_role(&request, role) {
        return serde_json::to_string(&Response {
            id,
            payload: ResponsePayload::Error {
                code: -6,
                message: "该客户端无权调用此操作。Agent 必须使用受策略保护的专用接口。".to_string(),
                category: Some("forbidden".to_string()),
            },
        })
        .unwrap();
    }

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
            .replace(PushCommand::Subscribe(rx, session_id.clone()));
        return serde_json::to_string(&Response {
            id,
            payload: ResponsePayload::Ok {
                result: serde_json::json!({"ok": true}),
            },
        })
        .unwrap();
    }
    if let Request::Pty(PtyRequest::UnsubscribeSession { session_id }) = &request {
        // 同步检查会话存在性（持锁时间最短）
        let exists = {
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
            let sessions = match pty.try_list_sessions() {
                Ok(sessions) => sessions,
                Err(error) => {
                    return serde_json::to_string(&Response {
                        id,
                        payload: ResponsePayload::Error {
                            code: -5,
                            message: error.to_string(),
                            category: None,
                        },
                    })
                    .unwrap();
                }
            };
            sessions.iter().any(|s| s.session_id == *session_id)
        };
        if !exists {
            return serde_json::to_string(&Response {
                id,
                payload: ResponsePayload::Error {
                    code: -5,
                    message: "会话不存在".to_string(),
                    category: None,
                },
            })
            .unwrap();
        }
        // pty guard 已释放，安全 await
        push_cmd_out
            .lock()
            .await
            .replace(PushCommand::Unsubscribe(session_id.clone()));
        return serde_json::to_string(&Response {
            id,
            payload: ResponsePayload::Ok {
                result: serde_json::json!({"ok": true}),
            },
        })
        .unwrap();
    }

    let result = handle_request(request, state, agent).await;

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
    agent: &Arc<Mutex<AgentController>>,
) -> Result<serde_json::Value> {
    // PTY 请求：不持金库锁，只锁 state.pty（独立 RwLock）。
    // PtyRequest 由 #[serde(untagged)] 在反序列化时已分流，
    // 编译器保证 handle_pty_request 穷尽匹配所有 PtyRequest 变体。
    if let Request::Pty(pty_req) = &request {
        return handle_pty_request(pty_req, state, agent).await;
    }

    if matches!(
        &request,
        Request::AgentExec { .. }
            | Request::ListAgentApprovals
            | Request::ApproveAgentAction { .. }
            | Request::DenyAgentAction { .. }
            | Request::ReadAgentAudit { .. }
    ) {
        return handle_agent_request(request, state, agent).await;
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
            remember_seconds,
        } => {
            let remember_seconds = remember_seconds
                .or_else(|| remember.then_some(vida_core::keyring_cache::MAX_CACHE_SECONDS));
            let info = state.unlock(&passphrase, remember_seconds)?;
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
            Ok(backup_response(data))
        }
        Request::PreviewBackup { data, passphrase } => {
            let (host_count, modified_at, host_names) = state.preview_backup(&data, &passphrase)?;
            let modified_at_display = chrono::DateTime::from_timestamp(modified_at, 0)
                .map(|date| date.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "未知时间".to_string());
            Ok(serde_json::json!({
                "host_count": host_count,
                "modified_at": modified_at_display,
                "host_names": host_names,
            }))
        }
        Request::RestoreBackup { data, passphrase } => {
            let (hosts, warning) = state.restore_backup(&data, &passphrase)?;
            let settings = state.get_settings()?;
            Ok(serde_json::json!({
                "restored": true,
                "hosts": hosts,
                "settings": settings,
                "warning": warning,
            }))
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
        Request::SetHostAgentTrust { host_id, trust } => {
            state.set_host_agent_trust(&host_id, trust)?;
            Ok(serde_json::json!({"updated": true, "trust": trust}))
        }

        Request::AgentExec { .. }
        | Request::ListAgentApprovals
        | Request::ApproveAgentAction { .. }
        | Request::DenyAgentAction { .. }
        | Request::ReadAgentAudit { .. } => {
            unreachable!("Agent requests handled before state lock")
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

fn request_allowed_for_role(request: &Request, role: ClientRole) -> bool {
    match role {
        ClientRole::Owner => !matches!(request, Request::AgentExec { .. }),
        ClientRole::Agent => matches!(
            request,
            Request::AgentExec { .. }
                | Request::VaultStatus
                | Request::ListHosts
                | Request::Pty(PtyRequest::ListSessions)
                | Request::Pty(PtyRequest::ReadScreen { .. })
                | Request::Pty(PtyRequest::ReadScreenStyled { .. })
                | Request::Pty(PtyRequest::SubscribeSession { .. })
                | Request::Pty(PtyRequest::UnsubscribeSession { .. })
        ),
    }
}

async fn handle_agent_request(
    request: Request,
    state: &Arc<Mutex<DaemonState>>,
    agent: &Arc<Mutex<AgentController>>,
) -> Result<serde_json::Value> {
    let now = chrono::Utc::now().timestamp();
    match request {
        Request::AgentExec {
            session_id,
            command,
        } => {
            if command.is_empty() || command.len() > 65_536 {
                anyhow::bail!("Agent 命令必须为 1–65536 字节");
            }
            if command.contains(['\r', '\n', '\0']) {
                anyhow::bail!("Agent exec 只接受一条不含换行或 NUL 的完整命令");
            }

            let target = agent.lock().await.target(&session_id).cloned();
            let trust = match target {
                Some(SessionTarget::Local) => vida_core::agent_policy::AgentTrust::Ask,
                Some(SessionTarget::Host { host_id }) => {
                    state.lock().await.host_agent_trust(&host_id)?
                }
                None => vida_core::agent_policy::AgentTrust::Ask,
            };
            let decision = agent
                .lock()
                .await
                .evaluate(&session_id, &command, trust, now)?;
            match decision {
                CommandDecision::RejectUnknownSession => Ok(serde_json::json!({
                    "status": "rejected",
                    "reason": "unknown_session",
                    "message": "该会话没有可信的主机映射。请在人类客户端重新打开会话后再试。"
                })),
                CommandDecision::RejectReadonly => Ok(serde_json::json!({
                    "status": "rejected",
                    "reason": "readonly",
                    "message": "该主机的 Agent 权限为只读，命令未发送。"
                })),
                CommandDecision::NeedsApproval(item) => Ok(serde_json::json!({
                    "status": "needs_approval",
                    "approval_id": item.approval_id,
                    "command": item.command,
                    "reasons": item.reasons,
                    "matched_rules": item.matched_rules,
                    "expires_at": item.expires_at,
                })),
                CommandDecision::Allow { matches } => {
                    // Persist the policy decision before sending bytes so an
                    // audit-file failure can never produce an unaudited write.
                    agent
                        .lock()
                        .await
                        .record_allowed(&session_id, &command, &matches)?;
                    let pty = state.lock().await.pty.clone();
                    let send_result = pty
                        .write()
                        .map_err(|_| anyhow::anyhow!("PTY 锁异常"))
                        .and_then(|pty| {
                            let mut bytes = command.as_bytes().to_vec();
                            bytes.push(b'\r');
                            pty.session_input(&session_id, &bytes)
                        });
                    if let Err(error) = send_result {
                        agent.lock().await.record_dispatch_failed(
                            &session_id,
                            &command,
                            &matches,
                            None,
                            "terminal dispatch failed",
                        )?;
                        return Err(error);
                    }
                    Ok(serde_json::json!({
                        "status": "sent",
                        "matched_rules": matches,
                    }))
                }
            }
        }
        Request::ListAgentApprovals => {
            let pending = agent.lock().await.pending(now)?;
            Ok(serde_json::to_value(pending)?)
        }
        Request::ApproveAgentAction { approval_id } => {
            let item = agent.lock().await.take_for_approval(&approval_id, now)?;
            // As above, approval must be durable before the command is sent.
            agent.lock().await.record_approved(&item)?;
            let pty = state.lock().await.pty.clone();
            let send_result = pty
                .write()
                .map_err(|_| anyhow::anyhow!("PTY 锁异常"))
                .and_then(|pty| {
                    let mut bytes = item.command.as_bytes().to_vec();
                    bytes.push(b'\r');
                    pty.session_input(&item.session_id, &bytes)
                });
            if let Err(error) = send_result {
                agent.lock().await.record_dispatch_failed(
                    &item.session_id,
                    &item.command,
                    &[],
                    Some(item.approval_id.clone()),
                    "approved command dispatch failed",
                )?;
                agent.lock().await.resolve(&item.approval_id, "failed");
                return Err(error);
            }
            agent.lock().await.resolve(&item.approval_id, "approved");
            Ok(serde_json::json!({"status": "approved", "sent": true}))
        }
        Request::DenyAgentAction { approval_id } => {
            agent.lock().await.deny(&approval_id, now)?;
            Ok(serde_json::json!({"status": "denied", "sent": false}))
        }
        Request::ReadAgentAudit { limit, host_id } => {
            let entries = agent
                .lock()
                .await
                .read_audit(limit.min(1000), host_id.as_deref())?;
            Ok(serde_json::to_value(entries)?)
        }
        _ => unreachable!("only Agent requests reach handle_agent_request"),
    }
}

/// Keep the encrypted backup bytes in the response. The GUI owns destination
/// selection and persists these bytes only after the user confirms a path.
fn backup_response(data: Vec<u8>) -> serde_json::Value {
    let bytes = data.len();
    serde_json::json!({"bytes": bytes, "data": data})
}

/// PTY 会话请求（M2a-2）。只锁 `state.pty`（独立 RwLock），
/// 全程不接触金库锁。编译器强制穷尽匹配所有 PtyRequest 变体。
#[allow(clippy::all, unused_mut)]
async fn handle_pty_request(
    request: &PtyRequest,
    state: &Arc<Mutex<DaemonState>>,
    agent: &Arc<Mutex<AgentController>>,
) -> Result<serde_json::Value> {
    // 短暂拿金库锁仅为了取出 pty 的 Arc 引用，随即释放；
    // 后续 PTY 操作只持有 pty 自己的 RwLock，不与金库锁争用。
    let pty = state.lock().await.pty.clone();

    match request {
        PtyRequest::OpenLocalSession { cols, rows } => {
            let session_id = {
                let mut pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
                pty.open_session(*cols, *rows)?
            };
            agent
                .lock()
                .await
                .register_session(&session_id, SessionTarget::Local)?;
            Ok(serde_json::json!({"session_id": session_id}))
        }
        PtyRequest::OpenSshSession {
            host_id,
            cols,
            rows,
        } => {
            let host = state.lock().await.host_for_ssh(host_id)?;
            let auth = match host.auth {
                vida_core::vault::AuthMethod::Password { password } => {
                    crate::pty::SshAuth::Password(secrecy::SecretString::from(
                        password.expose().to_owned(),
                    ))
                }
                vida_core::vault::AuthMethod::Key {
                    private_key_path,
                    passphrase,
                } => crate::pty::SshAuth::KeyFile {
                    path: private_key_path,
                    passphrase: passphrase
                        .map(|value| secrecy::SecretString::from(value.expose().to_owned())),
                },
                vida_core::vault::AuthMethod::KeyInline {
                    private_key,
                    passphrase,
                } => crate::pty::SshAuth::InlineKey {
                    private_key: secrecy::SecretString::from(private_key.expose().to_owned()),
                    passphrase: passphrase
                        .map(|value| secrecy::SecretString::from(value.expose().to_owned())),
                },
            };
            let session_id = {
                let mut pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
                pty.open_ssh_session(*cols, *rows, &host.host, &host.user, host.port, auth)?
            };
            agent.lock().await.register_session(
                &session_id,
                SessionTarget::Host {
                    host_id: host_id.clone(),
                },
            )?;
            Ok(serde_json::json!({"session_id": session_id}))
        }
        PtyRequest::SessionInput { session_id, data } => {
            let mut pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            pty.session_input(session_id, data)?;
            Ok(serde_json::json!({"ok": true}))
        }
        PtyRequest::PasteSession { session_id, data } => {
            let pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            pty.paste_session(session_id, data)?;
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
        PtyRequest::ScrollSession { session_id, lines } => {
            let pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
            let display_offset = pty.scroll_session(session_id, *lines)?;
            Ok(serde_json::json!({"ok": true, "display_offset": display_offset}))
        }
        PtyRequest::CloseSession { session_id } => {
            {
                let mut pty = pty.write().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
                pty.close_session(session_id)?;
            }
            agent.lock().await.remove_session(session_id)?;
            Ok(serde_json::json!({"ok": true}))
        }
        PtyRequest::ListSessions => {
            let sessions = {
                let pty = pty.read().map_err(|_| anyhow::anyhow!("PTY 锁异常"))?;
                pty.try_list_sessions()?
            };
            let sessions = describe_sessions(sessions, state, agent).await;
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

async fn describe_sessions(
    sessions: Vec<SessionInfo>,
    state: &Arc<Mutex<DaemonState>>,
    agent: &Arc<Mutex<AgentController>>,
) -> Vec<ListedSession> {
    let layout = vida_core::config::load_ui_state().terminal_layout;
    let targets: std::collections::HashMap<_, _> = {
        let agent = agent.lock().await;
        sessions
            .iter()
            .filter_map(|session| {
                agent
                    .target(&session.session_id)
                    .cloned()
                    .map(|target| (session.session_id.clone(), target))
            })
            .collect()
    };
    let (host_names, local_title, local_numbered) = {
        let state = state.lock().await;
        let host_names = state
            .list_hosts()
            .unwrap_or_default()
            .into_iter()
            .map(|host| (host.id, host.name))
            .collect::<std::collections::HashMap<_, _>>();
        let local_title = state.i18n.tr("terminal_local_title").to_string();
        let local_numbered = layout
            .iter()
            .map(|entry| {
                (
                    entry.session_id.clone(),
                    state
                        .i18n
                        .trf("terminal_local_numbered", &[&entry.number.to_string()]),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        (host_names, local_title, local_numbered)
    };

    let mut described = sessions
        .into_iter()
        .map(|process| {
            let layout_entry = layout
                .iter()
                .find(|entry| entry.session_id == process.session_id);
            let target = targets.get(&process.session_id).cloned().or_else(|| {
                layout_entry.map(|entry| {
                    entry
                        .host_id
                        .clone()
                        .map(|host_id| SessionTarget::Host { host_id })
                        .unwrap_or(SessionTarget::Local)
                })
            });

            match target {
                Some(SessionTarget::Host { host_id }) => {
                    let host_name = host_names.get(&host_id).cloned();
                    ListedSession {
                        title: host_name.clone().unwrap_or_else(|| host_id.clone()),
                        target_kind: "ssh",
                        host_id: Some(host_id),
                        host_name,
                        process,
                    }
                }
                Some(SessionTarget::Local) => ListedSession {
                    title: local_numbered
                        .get(&process.session_id)
                        .cloned()
                        .unwrap_or_else(|| local_title.clone()),
                    target_kind: "local",
                    host_id: None,
                    host_name: None,
                    process,
                },
                None => ListedSession {
                    title: process.session_id.clone(),
                    target_kind: "unknown",
                    host_id: None,
                    host_name: None,
                    process,
                },
            }
        })
        .collect::<Vec<_>>();

    // Match the GUI's persisted tab order; detached sessions not present in
    // the layout remain visible after the known tabs.
    described.sort_by_key(|session| {
        layout
            .iter()
            .position(|entry| entry.session_id == session.process.session_id)
            .unwrap_or(usize::MAX)
    });
    described
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
    // 防御：确保目录存在（正常启动已由 main 的 ensure_dirs 处理）
    if let Some(dir) = port_file.parent() {
        std::fs::create_dir_all(dir).with_context(|| {
            format!(
                "无法创建配置目录 {}：请检查权限（当前用户需对该路径有写权限）",
                dir.display()
            )
        })?;
    }
    std::fs::write(&port_file, addr.port().to_string())
        .with_context(|| format!("Failed to write daemon port to {}", port_file.display()))?;

    let agent_dir = state
        .lock()
        .await
        .vault_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    let include_ui_state = vida_core::config::config_dir().is_ok_and(|dir| dir == agent_dir);
    let agent = Arc::new(Mutex::new(AgentController::load_at(
        &agent_dir,
        include_ui_state,
    )?));
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let state = Arc::clone(&state);
                    let agent = Arc::clone(&agent);
                    tokio::spawn(handle_connection(stream, addr, state, agent));
                }
                Err(e) => {
                    error!("Accept error: {}", e);
                }
            }
        }
    });

    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::backup_response;

    #[test]
    fn backup_response_preserves_every_encrypted_byte() {
        let ciphertext = vec![0, 1, 127, 128, 254, 255];
        let response = backup_response(ciphertext.clone());
        let returned: Vec<u8> = response["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap() as u8)
            .collect();

        assert_eq!(response["bytes"], ciphertext.len());
        assert_eq!(returned, ciphertext);
    }
}
