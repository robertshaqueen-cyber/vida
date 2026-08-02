use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// stdio-to-WebSocket bridge for MCP clients that only support stdio.
/// Reads JSON-RPC from stdin, forwards to daemon WebSocket, returns responses to stdout.
#[tokio::main]
async fn main() -> Result<()> {
    let daemon_url = std::env::var("VIDA_DAEMON_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:9527".to_string());

    let (ws_stream, _) = connect_async(&daemon_url)
        .await
        .context("Failed to connect to vida daemon")?;

    let (mut ws_write, mut ws_read) = ws_stream.split();
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // EOF
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Forward to daemon
        ws_write
            .send(Message::Text(trimmed.into()))
            .await
            .context("Failed to send to daemon")?;

        // Wait for response
        if let Some(Ok(Message::Text(response))) = ws_read.next().await {
            stdout.write_all(response.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}
