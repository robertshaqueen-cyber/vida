use serde_json::{Value, json};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

async fn write_message(stdin: &mut tokio::process::ChildStdin, message: Value) {
    stdin
        .write_all(format!("{message}\n").as_bytes())
        .await
        .expect("write MCP message");
    stdin.flush().await.expect("flush MCP message");
}

async fn read_message(reader: &mut BufReader<tokio::process::ChildStdout>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    )
    .await
    .expect("MCP response timeout")
    .expect("read MCP response");
    assert!(!line.is_empty(), "MCP server exited before responding");
    serde_json::from_str(&line).expect("valid JSON-RPC response")
}

#[tokio::test]
async fn stdio_lifecycle_lists_only_reviewed_tools_without_daemon() {
    let isolated_config = std::env::temp_dir().join(format!(
        "vida-mcp-stdio-test-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("unnamed")
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_vida-mcp"))
        .env("VIDA_CONFIG_DIR", &isolated_config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start vida-mcp");
    let mut stdin = child.stdin.take().expect("MCP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("MCP stdout"));

    write_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "vida-integration-test", "version": "1"}
            }
        }),
    )
    .await;
    let initialized = read_message(&mut stdout).await;
    assert_eq!(initialized["id"], 1);
    assert_eq!(initialized["result"]["serverInfo"]["name"], "vida-mcp");
    assert!(initialized["result"]["capabilities"]["tools"].is_object());

    write_message(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    write_message(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let listed = read_message(&mut stdout).await;
    let mut names = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        [
            "exec",
            "host_list",
            "host_prepare",
            "screen_read",
            "session_list",
            "session_open",
            "vida_status"
        ]
    );
    for tool in listed["result"]["tools"].as_array().expect("tools array") {
        assert!(tool["inputSchema"].is_object());
        assert!(tool["outputSchema"].is_object());
    }

    // Startup does not require a running daemon; an unavailable daemon is a
    // tool-level error, not a broken MCP lifecycle or protocol response.
    write_message(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "vida_status", "arguments": {}}
        }),
    )
    .await;
    let unavailable = read_message(&mut stdout).await;
    assert_eq!(unavailable["id"], 3);
    assert_eq!(unavailable["result"]["isError"], true);
    let text = unavailable["result"]["content"][0]["text"]
        .as_str()
        .expect("tool error text");
    assert!(text.contains("Start vida-daemon"));

    drop(stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("MCP process exit timeout")
        .expect("wait for MCP process");
    assert!(status.success());
    let _ = std::fs::remove_dir_all(isolated_config);
}
