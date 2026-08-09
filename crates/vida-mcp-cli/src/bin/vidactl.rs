//! Human- and script-facing Vida daemon client.
//!
//! This first M5 checkpoint is deliberately read-only. Commands that can
//! modify a terminal or vault arrive together with policy and audit support in
//! the next checkpoint; exposing raw write access before that would bypass the
//! product's safety model.

use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};
use vida_client::{DaemonError, WsClient};

const EXIT_CONNECTION: u8 = 10;
const EXIT_DAEMON: u8 = 11;
const EXIT_NOT_FOUND: u8 = 12;
const EXIT_AMBIGUOUS: u8 = 13;
const EXIT_DATA: u8 = 14;

#[derive(Debug, Parser)]
#[command(
    name = "vidactl",
    version,
    about = "Vida daemon 的命令行客户端",
    long_about = "读取 Vida daemon 的金库状态、主机和终端会话。\n\
                  需要交互操作终端时请使用 Vida GUI；当前检查点不提供写入命令。"
)]
struct Cli {
    /// 输出稳定的 JSON envelope，供脚本使用；不要用于交互阅读。
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 检查 daemon、认证文件和金库状态；不要用它代替持续健康监控。
    Doctor,
    /// 查看金库是否存在、是否解锁；不会读取任何凭据。
    Status,
    /// 读取主机清单；需要修改主机时请使用 Vida GUI。
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// 读取当前终端会话；需要输入或接管时请使用 Vida GUI。
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    /// 列出不含口令和私钥内容的主机摘要。
    List,
    /// 按完整 ID 或唯一名称查看主机摘要；不会显示凭据。
    Show { host: String },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// 列出 daemon 当前发现的本地与 SSH 会话。
    List,
    /// 读取当前终端屏幕快照；普通命令的完整流水应等待后续 stream 命令。
    Screen { session_id: String },
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct VaultStatus {
    locked: bool,
    host_count: usize,
    revision: u64,
    device_id: String,
    vault_exists: bool,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct HostSummary {
    id: String,
    name: String,
    host: String,
    user: String,
    port: u16,
    tags: Vec<String>,
    group: Option<String>,
    color: Option<String>,
    auth_kind: String,
    notes: Option<String>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct SessionInfo {
    session_id: String,
    cols: u16,
    rows: u16,
    alive: bool,
    exit_code: Option<u32>,
    foreground_process: Option<String>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct CursorPos {
    row: u16,
    col: u16,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct ScreenData {
    lines: Vec<String>,
    wide_cols: Vec<Vec<u16>>,
    cursor: CursorPos,
}

#[derive(Debug)]
struct CliError {
    code: u8,
    kind: &'static str,
    message: String,
    category: Option<String>,
}

impl CliError {
    fn connection(_error: anyhow::Error) -> Self {
        Self {
            code: EXIT_CONNECTION,
            kind: "connection_error",
            message:
                "无法连接到 Vida daemon。它可能尚未启动或正在重启；请先启动 vida-daemon，然后重试。"
                    .to_string(),
            category: None,
        }
    }

    fn data(error: anyhow::Error) -> Self {
        Self {
            code: EXIT_DATA,
            kind: "invalid_daemon_response",
            message: format!("daemon 返回了无法识别的数据：{error:#}"),
            category: None,
        }
    }

    fn local(code: u8, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind,
            message: message.into(),
            category: None,
        }
    }
}

impl From<DaemonError> for CliError {
    fn from(error: DaemonError) -> Self {
        Self {
            code: EXIT_DAEMON,
            kind: "daemon_error",
            message: error.message,
            category: error.category,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(&cli).await {
        Ok(output) => {
            print_success(cli.json, output);
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_error(cli.json, &error);
            ExitCode::from(error.code)
        }
    }
}

async fn execute(cli: &Cli) -> std::result::Result<Output, CliError> {
    let client = WsClient::connect().await.map_err(CliError::connection)?;

    match &cli.command {
        Command::Doctor => {
            let status = typed_status(&client).await?;
            let config_dir = vida_core::config::config_dir().map_err(CliError::connection)?;
            Ok(Output::Doctor {
                config_dir: config_dir.display().to_string(),
                status,
            })
        }
        Command::Status => Ok(Output::Status(typed_status(&client).await?)),
        Command::Host { command } => match command {
            HostCommand::List => Ok(Output::Hosts(typed_hosts(&client).await?)),
            HostCommand::Show { host } => {
                let hosts = typed_hosts(&client).await?;
                let selected = select_host(hosts, host)?;
                Ok(Output::Host(selected))
            }
        },
        Command::Session { command } => match command {
            SessionCommand::List => Ok(Output::Sessions(typed_sessions(&client).await?)),
            SessionCommand::Screen { session_id } => {
                let value = client.read_screen(session_id).await?;
                let screen = serde_json::from_value(value)
                    .context("ReadScreen 字段不完整")
                    .map_err(CliError::data)?;
                Ok(Output::Screen(screen))
            }
        },
    }
}

async fn typed_status(client: &WsClient) -> std::result::Result<VaultStatus, CliError> {
    let value = client.vault_status().await?;
    serde_json::from_value(value)
        .context("VaultStatus 字段不完整")
        .map_err(CliError::data)
}

async fn typed_hosts(client: &WsClient) -> std::result::Result<Vec<HostSummary>, CliError> {
    let value = client.list_hosts().await?;
    serde_json::from_value(value)
        .context("ListHosts 字段不完整")
        .map_err(CliError::data)
}

async fn typed_sessions(client: &WsClient) -> std::result::Result<Vec<SessionInfo>, CliError> {
    let value = client.list_sessions().await?;
    serde_json::from_value(value)
        .context("ListSessions 字段不完整")
        .map_err(CliError::data)
}

fn select_host(
    hosts: Vec<HostSummary>,
    selector: &str,
) -> std::result::Result<HostSummary, CliError> {
    if let Some(host) = hosts.iter().find(|host| host.id == selector) {
        return Ok(host.clone());
    }

    let mut matches = hosts.into_iter().filter(|host| host.name == selector);
    let Some(first) = matches.next() else {
        return Err(CliError::local(
            EXIT_NOT_FOUND,
            "host_not_found",
            format!("没有找到主机“{selector}”。请先运行 vidactl host list。"),
        ));
    };
    if matches.next().is_some() {
        return Err(CliError::local(
            EXIT_AMBIGUOUS,
            "host_ambiguous",
            format!("主机名“{selector}”不唯一，请改用完整主机 ID。"),
        ));
    }
    Ok(first)
}

enum Output {
    Doctor {
        config_dir: String,
        status: VaultStatus,
    },
    Status(VaultStatus),
    Hosts(Vec<HostSummary>),
    Host(HostSummary),
    Sessions(Vec<SessionInfo>),
    Screen(ScreenData),
}

impl Output {
    fn json_value(&self) -> Value {
        match self {
            Self::Doctor { config_dir, status } => json!({
                "config_dir": config_dir,
                "daemon": "reachable",
                "vault": status,
            }),
            Self::Status(status) => json!(status),
            Self::Hosts(hosts) => json!(hosts),
            Self::Host(host) => json!(host),
            Self::Sessions(sessions) => json!(sessions),
            Self::Screen(screen) => json!(screen),
        }
    }

    fn print_human(&self) {
        match self {
            Self::Doctor { config_dir, status } => {
                println!("Vida daemon：可连接、认证成功");
                println!("配置目录：{config_dir}");
                println!(
                    "金库：{}（{} 台主机）",
                    if !status.vault_exists {
                        "尚未创建"
                    } else if status.locked {
                        "已锁定"
                    } else {
                        "已解锁"
                    },
                    status.host_count
                );
            }
            Self::Status(status) => {
                println!("金库存在：{}", yes_no(status.vault_exists));
                println!(
                    "锁定状态：{}",
                    if status.locked {
                        "已锁定"
                    } else {
                        "已解锁"
                    }
                );
                println!("主机数量：{}", status.host_count);
                println!("修订版本：{}", status.revision);
                println!("设备 ID：{}", status.device_id);
            }
            Self::Hosts(hosts) => {
                if hosts.is_empty() {
                    println!("没有已保存的主机。");
                } else {
                    for host in hosts {
                        println!(
                            "{}\t{}@{}:{}\t{}\t{}",
                            host.id, host.user, host.host, host.port, host.auth_kind, host.name
                        );
                    }
                }
            }
            Self::Host(host) => {
                println!("名称：{}", host.name);
                println!("ID：{}", host.id);
                println!("地址：{}@{}:{}", host.user, host.host, host.port);
                println!("认证：{}", host.auth_kind);
                println!("分组：{}", host.group.as_deref().unwrap_or("—"));
                println!(
                    "标签：{}",
                    if host.tags.is_empty() {
                        "—".to_string()
                    } else {
                        host.tags.join(", ")
                    }
                );
                println!("备注：{}", host.notes.as_deref().unwrap_or("—"));
            }
            Self::Sessions(sessions) => {
                if sessions.is_empty() {
                    println!("没有活动终端会话。");
                } else {
                    for session in sessions {
                        println!(
                            "{}\t{}×{}\t{}\t{}",
                            session.session_id,
                            session.cols,
                            session.rows,
                            if session.alive { "alive" } else { "closed" },
                            session.foreground_process.as_deref().unwrap_or("—")
                        );
                    }
                }
            }
            Self::Screen(screen) => print!("{}", screen.lines.join("\n")),
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "是" } else { "否" }
}

fn print_success(as_json: bool, output: Output) {
    if as_json {
        println!("{}", success_envelope(&output));
    } else {
        output.print_human();
    }
}

fn print_error(as_json: bool, error: &CliError) {
    if as_json {
        println!("{}", error_envelope(error));
    } else {
        eprintln!("错误：{}", error.message);
    }
}

fn success_envelope(output: &Output) -> Value {
    json!({"ok": true, "data": output.json_value()})
}

fn error_envelope(error: &CliError) -> Value {
    json!({
        "ok": false,
        "error": {
            "kind": error.kind,
            "message": error.message,
            "category": error.category,
            "exit_code": error.code,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, name: &str) -> HostSummary {
        HostSummary {
            id: id.to_string(),
            name: name.to_string(),
            host: "example.test".to_string(),
            user: "root".to_string(),
            port: 22,
            tags: vec![],
            group: None,
            color: None,
            auth_kind: "password".to_string(),
            notes: None,
        }
    }

    #[test]
    fn host_selector_prefers_exact_id() {
        let selected = select_host(vec![host("id-1", "same"), host("same", "other")], "same")
            .expect("exact id");
        assert_eq!(selected.id, "same");
    }

    #[test]
    fn duplicate_host_name_is_not_silently_selected() {
        let error = select_host(vec![host("id-1", "same"), host("id-2", "same")], "same")
            .expect_err("ambiguous name");
        assert_eq!(error.code, EXIT_AMBIGUOUS);
        assert_eq!(error.kind, "host_ambiguous");
    }

    #[test]
    fn missing_host_has_stable_error_code() {
        let error = select_host(vec![], "missing").expect_err("missing host");
        assert_eq!(error.code, EXIT_NOT_FOUND);
        assert_eq!(error.kind, "host_not_found");
    }

    #[test]
    fn json_envelopes_keep_stable_success_and_error_shape() {
        let success = success_envelope(&Output::Sessions(vec![]));
        assert_eq!(success, json!({"ok": true, "data": []}));

        let error = CliError::local(EXIT_NOT_FOUND, "host_not_found", "missing");
        assert_eq!(
            error_envelope(&error),
            json!({
                "ok": false,
                "error": {
                    "kind": "host_not_found",
                    "message": "missing",
                    "category": null,
                    "exit_code": EXIT_NOT_FOUND,
                }
            })
        );
    }
}
