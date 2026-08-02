use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
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
    },
}

type PendingMap = HashMap<u64, oneshot::Sender<Result<serde_json::Value>>>;

#[derive(Clone)]
pub struct WsClient {
    tx: mpsc::UnboundedSender<(WsRequest, oneshot::Sender<Result<serde_json::Value>>)>,
}

impl std::fmt::Debug for WsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsClient").finish()
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl WsClient {
    /// Connect to daemon WebSocket and authenticate.
    pub async fn connect() -> Result<Self> {
        let token = read_token()?;
        Self::connect_with_token(&token).await
    }

    /// Connect with an explicit token (for retry after re-read).
    pub async fn connect_with_token(token: &str) -> Result<Self> {
        let port = read_port()?;
        let url = format!("ws://127.0.0.1:{}", port);

        let (ws_stream, _) = connect_async(url)
            .await
            .context("无法连接到守护进程，请确认 vida-daemon 正在运行")?;

        let (mut ws_write, mut ws_read) = ws_stream.split();

        // Send auth as first WS message (before spawning background task)
        let auth_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let auth_req = WsRequest {
            method: "Auth".to_string(),
            params: Some(serde_json::json!({"token": token})),
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
                        anyhow::bail!("认证失败: {}", message);
                    }
                }
                Some(Ok(Message::Close(_))) => anyhow::bail!("连接被关闭"),
                Some(Err(e)) => anyhow::bail!("WebSocket 错误: {}", e),
                None => anyhow::bail!("连接断开"),
                _ => {}
            }
        };

        if auth_response.get("authenticated").and_then(|v| v.as_bool()) != Some(true) {
            anyhow::bail!("认证失败：token 无效");
        }

        // Auth succeeded — now create the mpsc channel and spawn the background R/W task
        let (tx, mut rx) =
            mpsc::unbounded_channel::<(WsRequest, oneshot::Sender<Result<serde_json::Value>>)>();

        tokio::spawn(async move {
            let mut pending: PendingMap = HashMap::new();

            loop {
                tokio::select! {
                    Some(msg) = ws_read.next() => {
                        match msg {
                            Ok(Message::Text(text)) => {
                                match serde_json::from_str::<WsResponse>(&text) {
                                    Ok(WsResponse::Ok { id, result }) => {
                                        if let Some(sender) = pending.remove(&id) {
                                            let _ = sender.send(Ok(result));
                                        }
                                    }
                                    Ok(WsResponse::Error { id, message, .. }) => {
                                        if let Some(sender) = pending.remove(&id) {
                                            let _ = sender.send(Err(anyhow::anyhow!("{}", message)));
                                        }
                                    }
                                    Err(_) => {}
                                }
                            }
                            Ok(Message::Close(_)) => break,
                            Err(_) => break,
                            _ => {}
                        }
                    }
                    Some((req, response_tx)) = rx.recv() => {
                        let id = req.id;
                        pending.insert(id, response_tx);
                        let text = serde_json::to_string(&req).unwrap();
                        if ws_write.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self { tx })
    }

    /// Send a request with params.
    pub async fn send(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let (response_tx, response_rx) = oneshot::channel();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

        let req = WsRequest {
            method: method.to_string(),
            params: Some(params),
            id,
        };

        self.tx
            .send((req, response_tx))
            .context("Failed to send request to daemon")?;

        response_rx
            .await
            .context("Daemon response channel closed")?
    }

    /// Send a request with no params (unit variant).
    pub async fn send_no_params(&self, method: &str) -> Result<serde_json::Value> {
        let (response_tx, response_rx) = oneshot::channel();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

        let req = WsRequest {
            method: method.to_string(),
            params: None,
            id,
        };

        self.tx
            .send((req, response_tx))
            .context("Failed to send request to daemon")?;

        response_rx
            .await
            .context("Daemon response channel closed")?
    }

    // Convenience methods --------------------------------------------------

    pub async fn vault_status(&self) -> Result<serde_json::Value> {
        self.send_no_params("VaultStatus").await
    }

    pub async fn create_vault(&self, passphrase: &str) -> Result<serde_json::Value> {
        self.send("CreateVault", serde_json::json!({"passphrase": passphrase}))
            .await
    }

    pub async fn unlock(&self, passphrase: &str, remember: bool) -> Result<serde_json::Value> {
        self.send(
            "Unlock",
            serde_json::json!({"passphrase": passphrase, "remember": remember}),
        )
        .await
    }

    pub async fn lock(&self) -> Result<serde_json::Value> {
        self.send_no_params("Lock").await
    }

    pub async fn list_hosts(&self) -> Result<serde_json::Value> {
        self.send_no_params("ListHosts").await
    }

    pub async fn get_settings(&self) -> Result<serde_json::Value> {
        self.send_no_params("GetSettings").await
    }

    pub async fn update_settings(&self, settings: serde_json::Value) -> Result<serde_json::Value> {
        self.send("UpdateSettings", serde_json::json!({"settings": settings}))
            .await
    }

    pub async fn reveal_credential(&self, host_id: &str) -> Result<serde_json::Value> {
        self.send("RevealCredential", serde_json::json!({"host_id": host_id}))
            .await
    }
}

/// Read daemon token from config dir (e.g. ~/Library/Application Support/vida/daemon.token on macOS)
fn read_token() -> Result<String> {
    let path = vida_core::config::config_dir()?.join("daemon.token");
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
