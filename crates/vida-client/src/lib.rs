//! Shared authenticated WebSocket client for Vida frontends.
//!
//! GUI, command-line, and MCP adapters must use this crate instead of
//! duplicating daemon discovery, authentication, request correlation, and
//! terminal push routing.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug, serde::Serialize)]
struct WsRequest {
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
    id: u64,
}

/// Daemon response: {"id":N, "type":"Ok"|"Error", ...}
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
enum WsResponse {
    #[serde(rename = "Ok")]
    Ok { id: u64, result: serde_json::Value },
    #[serde(rename = "Error")]
    Error {
        id: u64,
        #[allow(dead_code)] // error code reserved for protocol debugging
        code: i32,
        message: String,
        /// Error category for GUI i18n display.
        #[serde(skip_serializing_if = "Option::is_none")]
        category: Option<String>,
    },
}

/// Custom error type that includes the error category for GUI display.
#[derive(Debug)]
pub struct DaemonError {
    pub message: String,
    pub category: Option<String>,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DaemonError {}

/// Type alias for results that include the error category.
pub type DaemonResult<T> = std::result::Result<T, DaemonError>;

type PendingMap = HashMap<u64, oneshot::Sender<DaemonResult<serde_json::Value>>>;

/// 服务端推送消息（非请求-响应路径的旁路出口）。
#[derive(Debug, Clone)]
pub enum PushMsg {
    /// 终端二进制帧（已解出 session_id，bytes 为完整 payload）。
    Frame { session_id: String, bytes: Vec<u8> },
    /// 会话结束事件。exit_code 缺失时为 None（未知退出码，不伪造 0）。
    SessionClosed {
        session_id: String,
        exit_code: Option<u32>,
    },
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct AgentApproval {
    pub approval_id: String,
    pub session_id: String,
    pub host_id: Option<String>,
    pub command: String,
    pub reasons: Vec<String>,
    pub matched_rules: Vec<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnerEvent {
    ApprovalRequested {
        approval: AgentApproval,
    },
    ApprovalResolved {
        approval_id: String,
        status: String,
    },
    SessionOpened {
        session_id: String,
        host_id: String,
        host_name: String,
        cols: u16,
        rows: u16,
    },
}

/// 推送订阅注册表。
/// 多路复用：同一连接可订阅多个会话（daemon M2b-1 支持）。
/// 用 unbounded channel：iced 的 Subscription stream 内部持有 receiver
/// （不经 Message 传递，Message 只需 PushMsg 数据本身可 Clone）。
///
/// `prepared_receivers` 解决新会话的交接竞态：向 daemon 发送
/// SubscribeSession 前先注册 sender，首帧即使在 iced 建立 stream 前到达
/// 也会在 channel 中等待，而不会被丢弃。
#[derive(Default)]
struct PushRegistryState {
    senders: HashMap<String, mpsc::UnboundedSender<PushMsg>>,
    prepared_receivers: HashMap<String, mpsc::UnboundedReceiver<PushMsg>>,
    owner_event_sender: Option<mpsc::UnboundedSender<OwnerEvent>>,
    prepared_owner_event_receiver: Option<mpsc::UnboundedReceiver<OwnerEvent>>,
}

type PushRegistry = Arc<std::sync::Mutex<PushRegistryState>>;

#[derive(Clone)]
pub struct WsClient {
    tx: mpsc::UnboundedSender<(WsRequest, oneshot::Sender<DaemonResult<serde_json::Value>>)>,
    /// 推送注册表（后台任务写入，subscribe 返回接收端）。
    push_registry: PushRegistry,
}

impl std::fmt::Debug for WsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsClient").finish()
    }
}

// iced Subscription 的 data 需要 Hash + PartialEq：
// 以 push_registry 的 Arc 指针为 identity——同一连接多次构造相同，
// 重连（新 Arc）自动产生新 identity → iced 重启订阅 stream。
impl PartialEq for WsClient {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.push_registry, &other.push_registry)
    }
}

impl Eq for WsClient {}

impl std::hash::Hash for WsClient {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.push_registry).hash(state);
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Prefix marking an auth-rejection error, so `connect()` can retry with a
/// freshly re-read token (S0 contract).
const AUTH_FAILED_PREFIX: &str = "AUTH_FAILED:";

impl WsClient {
    /// Connect to daemon WebSocket and authenticate.
    ///
    /// S0 contract: if auth is rejected (token rotated by a daemon restart),
    /// re-read the token file and retry once. A plain connection failure is
    /// returned as-is.
    pub async fn connect() -> Result<Self> {
        Self::connect_for_role("owner").await
    }

    /// Connect with the restricted Agent token. This role cannot call human
    /// `SessionInput` or vault mutation methods; daemon policy is authoritative.
    pub async fn connect_agent() -> Result<Self> {
        Self::connect_for_role("agent").await
    }

    async fn connect_for_role(role: &str) -> Result<Self> {
        let token = read_role_token(role)?;
        match Self::connect_with_token_role(&token, role).await {
            Ok(client) => Ok(client),
            Err(e) => {
                let msg = format!("{:#}", e);
                if msg.starts_with(AUTH_FAILED_PREFIX)
                    && let Ok(new_token) = read_role_token(role)
                    && new_token != token
                {
                    return Self::connect_with_token_role(&new_token, role).await;
                }
                Err(e)
            }
        }
    }

    /// Connect with an explicit token (for retry after re-read).
    pub async fn connect_with_token(token: &str) -> Result<Self> {
        Self::connect_with_token_role(token, "owner").await
    }

    async fn connect_with_token_role(token: &str, role: &str) -> Result<Self> {
        let port = read_port()?;
        let address = format!("ws://127.0.0.1:{port}");
        Self::connect_to_role(&address, token, role).await
    }

    /// Connect to an explicit daemon address with an explicit token.
    ///
    /// Use this for isolated tests and managed deployments. Normal local
    /// clients should use [`WsClient::connect`] so token rotation is handled
    /// by re-reading Vida's protected config files.
    pub async fn connect_to(address: &str, token: &str) -> Result<Self> {
        Self::connect_to_role(address, token, "owner").await
    }

    async fn connect_to_role(address: &str, token: &str, role: &str) -> Result<Self> {
        let (ws_stream, _) = connect_async(address)
            .await
            .context("无法连接到守护进程，请确认 vida-daemon 正在运行")?;

        let (mut ws_write, mut ws_read) = ws_stream.split();

        // Send auth as first WS message (before spawning background task)
        let auth_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let auth_req = WsRequest {
            method: "Auth".to_string(),
            params: Some(serde_json::json!({"token": token, "role": role})),
            id: auth_id,
        };
        let auth_text = serde_json::to_string(&auth_req).unwrap();
        ws_write
            .send(Message::Text(auth_text.into()))
            .await
            .context("Failed to send auth message")?;

        // Read auth response
        let auth_response = loop {
            match ws_read.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(WsResponse::Ok { id, result }) =
                        serde_json::from_str::<WsResponse>(&text)
                    {
                        if id == auth_id {
                            break result;
                        }
                    } else if let Ok(WsResponse::Error { message, .. }) =
                        serde_json::from_str::<WsResponse>(&text)
                    {
                        anyhow::bail!("{}{}", AUTH_FAILED_PREFIX, message);
                    }
                }
                Some(Ok(Message::Close(_))) => anyhow::bail!("连接被关闭"),
                Some(Err(e)) => anyhow::bail!("WebSocket 错误: {}", e),
                None => anyhow::bail!("连接断开"),
                _ => {}
            }
        };

        if auth_response.get("authenticated").and_then(|v| v.as_bool()) != Some(true) {
            anyhow::bail!("{}token 无效", AUTH_FAILED_PREFIX);
        }

        // Auth succeeded — now create the mpsc channel and spawn the background R/W task
        let (tx, mut rx) = mpsc::unbounded_channel::<(
            WsRequest,
            oneshot::Sender<DaemonResult<serde_json::Value>>,
        )>();
        let (owner_event_sender, owner_event_receiver) = mpsc::unbounded_channel();
        let push_registry: PushRegistry = Arc::new(std::sync::Mutex::new(PushRegistryState {
            owner_event_sender: Some(owner_event_sender),
            prepared_owner_event_receiver: Some(owner_event_receiver),
            ..PushRegistryState::default()
        }));
        let push_registry_task = push_registry.clone();

        tokio::spawn(async move {
            let mut pending: PendingMap = HashMap::new();

            loop {
                tokio::select! {
                    msg = ws_read.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str::<WsResponse>(&text) {
                                    Ok(WsResponse::Ok { id, result }) => {
                                        if let Some(sender) = pending.remove(&id) {
                                            let _ = sender.send(Ok(result));
                                        }
                                    }
                                    Ok(WsResponse::Error { id, message, category, .. }) => {
                                        if let Some(sender) = pending.remove(&id) {
                                            let _ = sender.send(Err(DaemonError { message, category }));
                                        }
                                    }
                                    // 非响应文本（如 Event 推送）走旁路出口
                                    Err(_) => forward_text_push(&text, &push_registry_task),
                                }
                            }
                            Some(Ok(Message::Binary(bytes))) => {
                                forward_binary_push(&bytes, &push_registry_task);
                            }
                            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                            _ => {}
                        }
                    }
                    request = rx.recv() => {
                        match request {
                            Some((req, response_tx)) => {
                                let id = req.id;
                                pending.insert(id, response_tx);
                                let text = serde_json::to_string(&req).unwrap();
                                if ws_write.send(Message::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }

            // The registry owns every terminal subscription sender. If it is
            // kept alive after the socket task exits, the GUI waits forever on
            // a dead connection and never enters its reconnect flow.
            close_push_subscriptions(&push_registry_task);
        });

        Ok(Self { tx, push_registry })
    }

    /// 订阅某会话的推送：注册一个发送端，返回接收端。
    /// 重连后注册表是新的（WsClient 重建），订阅方必须重新订阅。
    pub fn subscribe(&self, session_id: &str) -> mpsc::UnboundedReceiver<PushMsg> {
        let mut reg = match self.push_registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(push_rx) = reg.prepared_receivers.remove(session_id) {
            return push_rx;
        }
        let (push_tx, push_rx) = mpsc::unbounded_channel();
        reg.senders.insert(session_id.to_string(), push_tx);
        push_rx
    }

    /// 在请求 daemon 发送首帧前预先建立本地推送通道。
    /// iced 稍后构建 Subscription 时会取走已准备的 receiver。
    pub fn prepare_subscription(&self, session_id: &str) {
        let mut reg = match self.push_registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if reg.senders.contains_key(session_id) {
            return;
        }
        let (push_tx, push_rx) = mpsc::unbounded_channel();
        reg.senders.insert(session_id.to_string(), push_tx);
        reg.prepared_receivers
            .insert(session_id.to_string(), push_rx);
    }

    /// 取消订阅：移除注册，不再转发该会话的推送。
    pub fn unsubscribe(&self, session_id: &str) {
        let mut reg = match self.push_registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        reg.senders.remove(session_id);
        reg.prepared_receivers.remove(session_id);
    }

    /// Take the one owner-event stream attached to this WebSocket connection.
    /// It is prepared during connect so approval events arriving before iced
    /// builds its Subscription remain queued instead of being lost.
    pub fn subscribe_owner_events(&self) -> mpsc::UnboundedReceiver<OwnerEvent> {
        let mut reg = match self.push_registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(receiver) = reg.prepared_owner_event_receiver.take() {
            return receiver;
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        reg.owner_event_sender = Some(sender);
        receiver
    }

    /// Send a request with params.
    pub async fn send(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> DaemonResult<serde_json::Value> {
        let (response_tx, response_rx) = oneshot::channel();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

        let req = WsRequest {
            method: method.to_string(),
            params: Some(params),
            id,
        };

        self.tx.send((req, response_tx)).map_err(|_| DaemonError {
            message: "Failed to send request to daemon".to_string(),
            category: None,
        })?;

        response_rx.await.map_err(|_| DaemonError {
            message: "Daemon response channel closed".to_string(),
            category: None,
        })?
    }

    /// 按调用顺序把请求放入 WebSocket 发送队列，不等待响应。
    ///
    /// 用于终端逐键输入和 resize：它们已经由 GUI 侧校验，且必须保持事件
    /// 顺序。不要用于需要读取结果或向用户展示服务端业务错误的操作。
    pub fn send_queued(&self, method: &str, params: serde_json::Value) -> DaemonResult<()> {
        let (response_tx, _response_rx) = oneshot::channel();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let req = WsRequest {
            method: method.to_string(),
            params: Some(params),
            id,
        };
        self.tx.send((req, response_tx)).map_err(|_| DaemonError {
            message: "终端连接已断开，输入未发送。请等待自动重连或返回后重新打开终端。".to_string(),
            category: None,
        })
    }

    /// Send a request with no params (unit variant).
    pub async fn send_no_params(&self, method: &str) -> DaemonResult<serde_json::Value> {
        let (response_tx, response_rx) = oneshot::channel();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

        let req = WsRequest {
            method: method.to_string(),
            params: None,
            id,
        };

        self.tx.send((req, response_tx)).map_err(|_| DaemonError {
            message: "Failed to send request to daemon".to_string(),
            category: None,
        })?;

        response_rx.await.map_err(|_| DaemonError {
            message: "Daemon response channel closed".to_string(),
            category: None,
        })?
    }

    // Convenience methods --------------------------------------------------

    pub async fn vault_status(&self) -> DaemonResult<serde_json::Value> {
        self.send_no_params("VaultStatus").await
    }

    pub async fn create_vault(&self, passphrase: &str) -> DaemonResult<serde_json::Value> {
        self.send("CreateVault", serde_json::json!({"passphrase": passphrase}))
            .await
    }

    pub async fn unlock(
        &self,
        passphrase: &str,
        remember_seconds: Option<u64>,
    ) -> DaemonResult<serde_json::Value> {
        self.send(
            "Unlock",
            serde_json::json!({
                "passphrase": passphrase,
                "remember_seconds": remember_seconds,
            }),
        )
        .await
    }

    pub async fn lock(&self) -> DaemonResult<serde_json::Value> {
        self.send_no_params("Lock").await
    }

    pub async fn list_hosts(&self) -> DaemonResult<serde_json::Value> {
        self.send_no_params("ListHosts").await
    }

    pub async fn list_sessions(&self) -> DaemonResult<serde_json::Value> {
        self.send_no_params("ListSessions").await
    }

    pub async fn read_screen(&self, session_id: &str) -> DaemonResult<serde_json::Value> {
        self.send("ReadScreen", serde_json::json!({"session_id": session_id}))
            .await
    }

    pub async fn agent_exec(
        &self,
        session_id: &str,
        command: &str,
    ) -> DaemonResult<serde_json::Value> {
        self.send(
            "AgentExec",
            serde_json::json!({"session_id": session_id, "command": command}),
        )
        .await
    }

    /// Ask the daemon to open one configured SSH host using credentials that
    /// remain inside the unlocked vault.
    pub async fn agent_open_ssh_session(&self, host_id: &str) -> DaemonResult<serde_json::Value> {
        self.send(
            "AgentOpenSshSession",
            serde_json::json!({"host_id": host_id}),
        )
        .await
    }

    pub async fn list_agent_approvals(&self) -> DaemonResult<serde_json::Value> {
        self.send_no_params("ListAgentApprovals").await
    }

    pub async fn approve_agent_action(&self, approval_id: &str) -> DaemonResult<serde_json::Value> {
        self.send(
            "ApproveAgentAction",
            serde_json::json!({"approval_id": approval_id}),
        )
        .await
    }

    pub async fn deny_agent_action(&self, approval_id: &str) -> DaemonResult<serde_json::Value> {
        self.send(
            "DenyAgentAction",
            serde_json::json!({"approval_id": approval_id}),
        )
        .await
    }

    pub async fn read_agent_audit(
        &self,
        limit: usize,
        host_id: Option<&str>,
    ) -> DaemonResult<serde_json::Value> {
        self.send(
            "ReadAgentAudit",
            serde_json::json!({"limit": limit, "host_id": host_id}),
        )
        .await
    }

    pub async fn set_host_agent_trust(
        &self,
        host_id: &str,
        trust: &str,
    ) -> DaemonResult<serde_json::Value> {
        self.send(
            "SetHostAgentTrust",
            serde_json::json!({"host_id": host_id, "trust": trust}),
        )
        .await
    }

    pub async fn get_settings(&self) -> DaemonResult<serde_json::Value> {
        self.send_no_params("GetSettings").await
    }

    pub async fn update_settings(
        &self,
        settings: serde_json::Value,
    ) -> DaemonResult<serde_json::Value> {
        self.send("UpdateSettings", serde_json::json!({"settings": settings}))
            .await
    }

    pub async fn sync(&self) -> DaemonResult<serde_json::Value> {
        self.send_no_params("Sync").await
    }

    pub async fn reveal_credential(&self, host_id: &str) -> DaemonResult<serde_json::Value> {
        self.send("RevealCredential", serde_json::json!({"host_id": host_id}))
            .await
    }
}

/// Drop every terminal push sender when the underlying WebSocket exits.
/// Receivers wake with `None` and report the disconnect to the GUI.
fn close_push_subscriptions(registry: &PushRegistry) {
    let mut reg = match registry.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    reg.senders.clear();
    reg.prepared_receivers.clear();
    reg.owner_event_sender = None;
    reg.prepared_owner_event_receiver = None;
}

/// 二进制帧头格式（与 daemon encode_frame 对应）：
/// [0x01][session_id_len: u16 BE][session_id][payload]
fn decode_frame_header(bytes: &[u8]) -> Option<(String, &[u8])> {
    if bytes.len() < 3 || bytes[0] != 0x01 {
        return None;
    }
    let id_len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    if 3 + id_len > bytes.len() {
        return None;
    }
    let id = String::from_utf8(bytes[3..3 + id_len].to_vec()).ok()?;
    Some((id, &bytes[3 + id_len..]))
}

/// 把二进制推送帧转发给对应会话的订阅者。
/// 解码失败（非终端帧）时静默丢弃——不是本客户端的职责。
fn forward_binary_push(bytes: &[u8], registry: &PushRegistry) {
    let Some((session_id, payload)) = decode_frame_header(bytes) else {
        return;
    };
    let reg = match registry.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(tx) = reg.senders.get(&session_id) {
        let _ = tx.send(PushMsg::Frame {
            session_id,
            bytes: payload.to_vec(),
        });
    }
}

/// 把文本 Event 推送（如 session_closed）转发给对应会话的订阅者。
#[derive(serde::Deserialize)]
struct WsEvent {
    #[serde(rename = "type")]
    event_type: String,
    event: String,
    data: serde_json::Value,
}

fn forward_text_push(text: &str, registry: &PushRegistry) {
    let event: WsEvent = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(_) => return,
    };
    if event.event_type != "Event" {
        return;
    }
    if event.event.as_str() == "agent_approval" {
        let owner_event = match serde_json::from_value::<OwnerEvent>(event.data) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!("Agent approval event has invalid data: {error}");
                return;
            }
        };
        let reg = match registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(sender) = &reg.owner_event_sender {
            let _ = sender.send(owner_event);
        }
    } else if event.event.as_str() == "session_closed" {
        // session_id 缺失/类型错误：记 warn 并丢弃，不构造空串——
        // 否则 reg.get("") 查不到订阅者，事件被静默吞掉。
        let Some(session_id) = event.data.get("session_id").and_then(|v| v.as_str()) else {
            tracing::warn!("session_closed 事件缺少有效的 session_id: {}", event.data);
            return;
        };
        // exit_code 缺失 → None（未知退出码），不伪造 0。
        let exit_code = event
            .data
            .get("exit_code")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let reg = match registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(tx) = reg.senders.get(session_id) {
            let _ = tx.send(PushMsg::SessionClosed {
                session_id: session_id.to_string(),
                exit_code,
            });
        }
    }
}

/// Read daemon token from config dir (e.g. ~/Library/Application Support/vida/daemon.token on macOS)
fn read_role_token(role: &str) -> Result<String> {
    let filename = if role == "agent" {
        "agent.token"
    } else {
        "daemon.token"
    };
    let path = vida_core::config::config_dir()?.join(filename);
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("无法读取守护进程 token: {}", path.display()))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("守护进程 token 为空");
    }
    Ok(token)
}

/// Read daemon port from config dir (e.g. ~/Library/Application Support/vida/daemon.port on macOS)
fn read_port() -> Result<u16> {
    let path = vida_core::config::config_dir()?.join("daemon.port");
    let port_str = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "无法读取守护进程端口文件: {}。请确认 vida-daemon 正在运行。",
            path.display()
        )
    })?;
    let port: u16 = port_str
        .trim()
        .parse()
        .with_context(|| format!("守护进程端口格式无效: '{}'", port_str.trim()))?;
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::{
        OwnerEvent, PushMsg, PushRegistry, PushRegistryState, WsClient, close_push_subscriptions,
        forward_text_push,
    };
    use futures_util::{SinkExt, StreamExt};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn websocket_exit_closes_terminal_subscription_receivers() {
        let registry: PushRegistry = Arc::new(std::sync::Mutex::new(PushRegistryState::default()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        registry
            .lock()
            .unwrap()
            .senders
            .insert("session-1".to_string(), tx);

        close_push_subscriptions(&registry);

        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn prepared_subscription_keeps_frame_that_arrives_before_iced_stream() {
        let (tx, _request_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(std::sync::Mutex::new(PushRegistryState::default()));
        let client = WsClient {
            tx,
            push_registry: registry.clone(),
        };
        client.prepare_subscription("new-session");

        let session_id = b"new-session";
        let mut wire = vec![0x01, 0x00, session_id.len() as u8];
        wire.extend_from_slice(session_id);
        wire.extend_from_slice(b"full-frame");
        super::forward_binary_push(&wire, &registry);

        let mut receiver = client.subscribe("new-session");
        match receiver.try_recv().expect("prepared first frame") {
            PushMsg::Frame { session_id, bytes } => {
                assert_eq!(session_id, "new-session");
                assert_eq!(bytes, b"full-frame");
            }
            other => panic!("unexpected push: {other:?}"),
        }
    }

    #[test]
    fn owner_approval_event_is_queued_before_gui_subscribes() {
        let (tx, _request_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(std::sync::Mutex::new(PushRegistryState::default()));
        let client = WsClient {
            tx,
            push_registry: registry.clone(),
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        {
            let mut state = registry.lock().unwrap();
            state.owner_event_sender = Some(sender);
            state.prepared_owner_event_receiver = Some(receiver);
        }

        forward_text_push(
            r#"{"type":"Event","event":"agent_approval","data":{"kind":"approval_requested","approval":{"approval_id":"approval-1","session_id":"session-1","host_id":null,"command":"reboot","reasons":["server restart"],"matched_rules":["power_control"],"created_at":10,"expires_at":130}}}"#,
            &registry,
        );

        let mut receiver = client.subscribe_owner_events();
        assert!(matches!(
            receiver.try_recv().unwrap(),
            OwnerEvent::ApprovalRequested { approval }
                if approval.approval_id == "approval-1" && approval.command == "reboot"
        ));
    }

    #[test]
    fn agent_opened_session_event_is_queued_without_credentials() {
        let (tx, _request_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(std::sync::Mutex::new(PushRegistryState::default()));
        let client = WsClient {
            tx,
            push_registry: registry.clone(),
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        {
            let mut state = registry.lock().unwrap();
            state.owner_event_sender = Some(sender);
            state.prepared_owner_event_receiver = Some(receiver);
        }

        let wire = r#"{"type":"Event","event":"agent_approval","data":{"kind":"session_opened","session_id":"session-1","host_id":"host-1","host_name":"Production","cols":100,"rows":40}}"#;
        forward_text_push(wire, &registry);

        let mut receiver = client.subscribe_owner_events();
        assert!(matches!(
            receiver.try_recv().unwrap(),
            OwnerEvent::SessionOpened {
                session_id,
                host_id,
                host_name,
                cols: 100,
                rows: 40,
            } if session_id == "session-1"
                && host_id == "host-1"
                && host_name == "Production"
        ));
        assert!(!wire.contains("password"));
        assert!(!wire.contains("private_key"));
    }

    #[tokio::test]
    async fn explicit_connection_authenticates_and_correlates_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake daemon");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut websocket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("websocket handshake");

            let auth = websocket
                .next()
                .await
                .expect("auth frame")
                .expect("valid auth frame")
                .into_text()
                .expect("auth text");
            let auth: serde_json::Value = serde_json::from_str(&auth).expect("auth json");
            assert_eq!(auth["method"], "Auth");
            assert_eq!(auth["params"]["token"], "test-token");
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "id": auth["id"],
                        "type": "Ok",
                        "result": {"authenticated": true}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("auth response");

            let request = websocket
                .next()
                .await
                .expect("request frame")
                .expect("valid request frame")
                .into_text()
                .expect("request text");
            let request: serde_json::Value = serde_json::from_str(&request).expect("request json");
            assert_eq!(request["method"], "VaultStatus");
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "id": request["id"],
                        "type": "Ok",
                        "result": {
                            "locked": true,
                            "host_count": 0,
                            "revision": 0,
                            "device_id": "test-device",
                            "vault_exists": false
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("request response");
        });

        let client = WsClient::connect_to(&format!("ws://{address}"), "test-token")
            .await
            .expect("connect client");
        let status = client.vault_status().await.expect("vault status");
        assert_eq!(status["device_id"], "test-device");
        server.await.expect("fake daemon task");
    }
}
