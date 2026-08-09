use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    if vida_daemon::ssh_auth::run_helper_if_requested()? {
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vida_daemon=info".into()),
        )
        .init();

    info!("vida daemon starting");

    // 首次启动：配置目录（如 ~/Library/Application Support/vida）可能不存在，
    // 必须在写入 token/port/金库 之前创建，否则新用户首次启动直接失败。
    vida_core::config::ensure_dirs()?;
    let stale_keys = vida_daemon::pty::cleanup_stale_inline_keys()?;
    if stale_keys > 0 {
        info!("已清理 {} 个上次异常退出遗留的 SSH 临时密钥", stale_keys);
    }

    let token = vida_daemon::ws_server::load_or_create_token()?;
    info!("Auth token loaded");

    let state = Arc::new(Mutex::new(vida_daemon::state::DaemonState::new(token)?));

    let addr = vida_daemon::ws_server::start(state.clone()).await?;
    info!("WebSocket server listening on {}", addr);

    tokio::signal::ctrl_c().await?;

    // 回收所有 PTY 会话子进程，不留僵尸
    {
        let state = state.lock().await;
        if let Ok(mut pty) = state.pty.write() {
            pty.shutdown();
        }
    }

    vida_daemon::ws_server::cleanup_token();
    info!("vida daemon shutting down");
    Ok(())
}
