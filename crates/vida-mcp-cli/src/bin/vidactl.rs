//! Human- and script-facing Vida daemon client.
//!
//! Read operations use the owner's daemon role. Agent command execution uses a
//! separate token and can only reach the daemon-owned policy/approval/audit
//! gate; it cannot fall through to raw human terminal input.

use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::{Value, json};
use vida_client::{DaemonError, WsClient};
use vida_core::i18n::I18n;

const EXIT_CONNECTION: u8 = 10;
const EXIT_DAEMON: u8 = 11;
const EXIT_NOT_FOUND: u8 = 12;
const EXIT_AMBIGUOUS: u8 = 13;
const EXIT_DATA: u8 = 14;
const EXIT_NEEDS_APPROVAL: u8 = 20;
const EXIT_REJECTED: u8 = 21;

#[derive(Debug, Parser)]
#[command(
    name = "vidactl",
    version,
    about = "Vida daemon 的命令行客户端",
    long_about = "读取 Vida daemon 状态，并通过统一安全策略让 Agent 执行完整命令。\n\
                  人的交互式终端输入仍请使用 Vida GUI。"
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
    /// 查看并处理 Agent 危险命令审批；不要用它发送普通终端输入。
    Approval {
        #[command(subcommand)]
        command: ApprovalCommand,
    },
    /// 查看 Agent 写入审计；不要把它当作完整终端流水。
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        host_id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    /// 列出不含口令和私钥内容的主机摘要。
    List,
    /// 按完整 ID 或唯一名称查看主机摘要；不会显示凭据。
    Show { host: String },
    /// 设置 Agent 写入权限；人的终端输入不受影响。
    Trust { host: String, trust: TrustArg },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// 列出 daemon 当前发现的本地与 SSH 会话。
    List,
    /// 读取当前终端屏幕快照；普通命令的完整流水应等待后续 stream 命令。
    Screen { session_id: String },
    /// 通过 daemon 安全策略发送一条完整命令；不要用于交互按键或秘密值。
    Exec {
        session_id: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ApprovalCommand {
    /// 列出仍在 120 秒有效期内的待审批命令。
    List,
    /// 批准并发送一条待审批命令。
    Approve { approval_id: String },
    /// 拒绝一条待审批命令，保证它不会发送。
    Deny { approval_id: String },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TrustArg {
    Readonly,
    Ask,
    Trusted,
}

impl TrustArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Readonly => "readonly",
            Self::Ask => "ask",
            Self::Trusted => "trusted",
        }
    }
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
    agent_trust: String,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct SessionInfo {
    session_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    target_kind: String,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    host_name: Option<String>,
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
    fn connection(_error: anyhow::Error, i18n: &I18n) -> Self {
        Self {
            code: EXIT_CONNECTION,
            kind: "connection_error",
            message: i18n.tr("cli_error_connection").to_string(),
            category: None,
        }
    }

    fn data(error: anyhow::Error, i18n: &I18n) -> Self {
        Self {
            code: EXIT_DATA,
            kind: "invalid_daemon_response",
            message: i18n.trf("cli_error_invalid_data", &[&format!("{error:#}")]),
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
    let i18n = I18n::new(vida_core::i18n::detect_lang());
    match execute(&cli, &i18n).await {
        Ok(output) => {
            let exit_code = output.exit_code();
            print_success(cli.json, output, &i18n);
            ExitCode::from(exit_code)
        }
        Err(error) => {
            print_error(cli.json, &error, &i18n);
            ExitCode::from(error.code)
        }
    }
}

async fn execute(cli: &Cli, i18n: &I18n) -> std::result::Result<Output, CliError> {
    let agent_role = matches!(
        &cli.command,
        Command::Session {
            command: SessionCommand::Exec { .. }
        }
    );
    let client = if agent_role {
        WsClient::connect_agent().await
    } else {
        WsClient::connect().await
    }
    .map_err(|error| CliError::connection(error, i18n))?;

    match &cli.command {
        Command::Doctor => {
            let status = typed_status(&client, i18n).await?;
            let config_dir = vida_core::config::config_dir()
                .map_err(|error| CliError::connection(error, i18n))?;
            Ok(Output::Doctor {
                config_dir: config_dir.display().to_string(),
                status,
            })
        }
        Command::Status => Ok(Output::Status(typed_status(&client, i18n).await?)),
        Command::Host { command } => match command {
            HostCommand::List => Ok(Output::Hosts(typed_hosts(&client, i18n).await?)),
            HostCommand::Show { host } => {
                let hosts = typed_hosts(&client, i18n).await?;
                let selected = select_host(hosts, host, i18n)?;
                Ok(Output::Host(selected))
            }
            HostCommand::Trust { host, trust } => {
                let hosts = typed_hosts(&client, i18n).await?;
                let selected = select_host(hosts, host, i18n)?;
                let result = client
                    .set_host_agent_trust(&selected.id, trust.as_str())
                    .await?;
                Ok(Output::HostTrust {
                    host: selected.name,
                    result,
                })
            }
        },
        Command::Session { command } => match command {
            SessionCommand::List => Ok(Output::Sessions(typed_sessions(&client, i18n).await?)),
            SessionCommand::Screen { session_id } => {
                let value = client.read_screen(session_id).await?;
                let screen = serde_json::from_value(value)
                    .context("ReadScreen 字段不完整")
                    .map_err(|error| CliError::data(error, i18n))?;
                Ok(Output::Screen(screen))
            }
            SessionCommand::Exec {
                session_id,
                command,
            } => {
                let command = command.join(" ");
                Ok(Output::AgentExec(
                    client.agent_exec(session_id, &command).await?,
                ))
            }
        },
        Command::Approval { command } => match command {
            ApprovalCommand::List => Ok(Output::Approvals(client.list_agent_approvals().await?)),
            ApprovalCommand::Approve { approval_id } => Ok(Output::ApprovalAction(
                client.approve_agent_action(approval_id).await?,
            )),
            ApprovalCommand::Deny { approval_id } => Ok(Output::ApprovalAction(
                client.deny_agent_action(approval_id).await?,
            )),
        },
        Command::Audit { limit, host_id } => Ok(Output::Audit(
            client.read_agent_audit(*limit, host_id.as_deref()).await?,
        )),
    }
}

async fn typed_status(
    client: &WsClient,
    i18n: &I18n,
) -> std::result::Result<VaultStatus, CliError> {
    let value = client.vault_status().await?;
    serde_json::from_value(value)
        .context("VaultStatus 字段不完整")
        .map_err(|error| CliError::data(error, i18n))
}

async fn typed_hosts(
    client: &WsClient,
    i18n: &I18n,
) -> std::result::Result<Vec<HostSummary>, CliError> {
    let value = client.list_hosts().await?;
    serde_json::from_value(value)
        .context("ListHosts 字段不完整")
        .map_err(|error| CliError::data(error, i18n))
}

async fn typed_sessions(
    client: &WsClient,
    i18n: &I18n,
) -> std::result::Result<Vec<SessionInfo>, CliError> {
    let value = client.list_sessions().await?;
    serde_json::from_value(value)
        .context("ListSessions 字段不完整")
        .map_err(|error| CliError::data(error, i18n))
}

fn select_host(
    hosts: Vec<HostSummary>,
    selector: &str,
    i18n: &I18n,
) -> std::result::Result<HostSummary, CliError> {
    if let Some(host) = hosts.iter().find(|host| host.id == selector) {
        return Ok(host.clone());
    }

    let mut matches = hosts.into_iter().filter(|host| host.name == selector);
    let Some(first) = matches.next() else {
        return Err(CliError::local(
            EXIT_NOT_FOUND,
            "host_not_found",
            i18n.trf("cli_error_host_not_found", &[selector]),
        ));
    };
    if matches.next().is_some() {
        return Err(CliError::local(
            EXIT_AMBIGUOUS,
            "host_ambiguous",
            i18n.trf("cli_error_host_ambiguous", &[selector]),
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
    HostTrust {
        host: String,
        result: Value,
    },
    AgentExec(Value),
    Approvals(Value),
    ApprovalAction(Value),
    Audit(Value),
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
            Self::HostTrust { result, .. }
            | Self::AgentExec(result)
            | Self::Approvals(result)
            | Self::ApprovalAction(result)
            | Self::Audit(result) => result.clone(),
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::AgentExec(result) => match result.get("status").and_then(Value::as_str) {
                Some("needs_approval") => EXIT_NEEDS_APPROVAL,
                Some("rejected") => EXIT_REJECTED,
                _ => 0,
            },
            _ => 0,
        }
    }

    fn human_text(&self, i18n: &I18n) -> String {
        match self {
            Self::Doctor { config_dir, status } => {
                let count = status.host_count.to_string();
                format!(
                    "{}\n{}\n{}\n",
                    i18n.tr("cli_doctor_daemon_ok"),
                    i18n.trf("cli_doctor_config_dir", &[config_dir]),
                    i18n.trf(
                        "cli_doctor_vault",
                        &[vault_state(status, i18n), count.as_str()]
                    )
                )
            }
            Self::Status(status) => {
                format!(
                    "{}\n{}\n{}\n{}\n{}\n",
                    i18n.trf(
                        "cli_status_vault_exists",
                        &[yes_no(status.vault_exists, i18n)]
                    ),
                    i18n.trf(
                        "cli_status_lock_state",
                        &[if status.locked {
                            i18n.tr("cli_vault_locked")
                        } else {
                            i18n.tr("cli_vault_unlocked")
                        }]
                    ),
                    i18n.trf("cli_status_host_count", &[&status.host_count.to_string()]),
                    i18n.trf("cli_status_revision", &[&status.revision.to_string()]),
                    i18n.trf("cli_status_device_id", &[&status.device_id]),
                )
            }
            Self::Hosts(hosts) => {
                if hosts.is_empty() {
                    format!("{}\n", i18n.tr("cli_hosts_empty"))
                } else {
                    hosts
                        .iter()
                        .map(|host| {
                            format!(
                                "{}\t{}@{}:{}\t{}\t{}",
                                host.id,
                                host.user,
                                host.host,
                                host.port,
                                auth_label(&host.auth_kind, i18n),
                                host.name
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                        + "\n"
                }
            }
            Self::Host(host) => format!(
                "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
                i18n.trf("cli_host_name", &[&host.name]),
                i18n.trf("cli_host_id", &[&host.id]),
                i18n.trf(
                    "cli_host_address",
                    &[&host.user, &host.host, &host.port.to_string()]
                ),
                i18n.trf("cli_host_auth", &[auth_label(&host.auth_kind, i18n)]),
                i18n.trf("cli_host_group", &[host.group.as_deref().unwrap_or("—")]),
                i18n.trf(
                    "cli_host_tags",
                    &[&if host.tags.is_empty() {
                        "—".to_string()
                    } else {
                        host.tags.join(", ")
                    }]
                ),
                i18n.trf("cli_host_notes", &[host.notes.as_deref().unwrap_or("—")]),
                i18n.trf("cli_host_agent_trust", &[&host.agent_trust]),
            ),
            Self::Sessions(sessions) => {
                if sessions.is_empty() {
                    format!("{}\n", i18n.tr("cli_sessions_empty"))
                } else {
                    sessions
                        .iter()
                        .map(|session| {
                            format!(
                                "{}\t{}\t{}\t{}\t{}×{}\t{}\t{}",
                                session.session_id,
                                if session.title.is_empty() {
                                    "—"
                                } else {
                                    &session.title
                                },
                                if session.target_kind.is_empty() {
                                    "unknown"
                                } else {
                                    &session.target_kind
                                },
                                session.host_id.as_deref().unwrap_or("—"),
                                session.cols,
                                session.rows,
                                if session.alive {
                                    i18n.tr("cli_session_alive")
                                } else {
                                    i18n.tr("cli_session_closed")
                                },
                                session.foreground_process.as_deref().unwrap_or("—")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                        + "\n"
                }
            }
            Self::Screen(screen) => screen.lines.join("\n"),
            Self::HostTrust { host, result } => {
                i18n.trf(
                    "cli_host_trust_updated",
                    &[
                        host,
                        result
                            .get("trust")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                    ],
                ) + "\n"
            }
            Self::AgentExec(result) => match result.get("status").and_then(Value::as_str) {
                Some("sent") => format!("{}\n", i18n.tr("cli_agent_sent")),
                Some("needs_approval") => {
                    i18n.trf(
                        "cli_agent_needs_approval",
                        &[
                            result
                                .get("approval_id")
                                .and_then(Value::as_str)
                                .unwrap_or("—"),
                            &result
                                .get("reasons")
                                .and_then(Value::as_array)
                                .map(|items| {
                                    items
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default(),
                        ],
                    ) + "\n"
                }
                Some("rejected") => {
                    i18n.trf(
                        "cli_agent_rejected",
                        &[result.get("message").and_then(Value::as_str).unwrap_or("—")],
                    ) + "\n"
                }
                _ => format!("{}\n", result),
            },
            Self::Approvals(result) => {
                let Some(items) = result.as_array() else {
                    return format!("{}\n", result);
                };
                if items.is_empty() {
                    format!("{}\n", i18n.tr("cli_approvals_empty"))
                } else {
                    items
                        .iter()
                        .map(|item| {
                            format!(
                                "{}\t{}\t{}",
                                item.get("approval_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("—"),
                                item.get("session_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("—"),
                                item.get("command").and_then(Value::as_str).unwrap_or("—")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                        + "\n"
                }
            }
            Self::ApprovalAction(result) => format!(
                "{}\n",
                match result.get("status").and_then(Value::as_str) {
                    Some("approved") => i18n.tr("cli_approval_approved"),
                    Some("denied") => i18n.tr("cli_approval_denied"),
                    _ => i18n.tr("cli_approval_updated"),
                }
            ),
            Self::Audit(result) => {
                let Some(items) = result.as_array() else {
                    return format!("{}\n", result);
                };
                if items.is_empty() {
                    format!("{}\n", i18n.tr("cli_audit_empty"))
                } else {
                    items
                        .iter()
                        .map(|item| {
                            format!(
                                "{}\t{}\t{}\t{}",
                                item.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                                item.get("outcome").and_then(Value::as_str).unwrap_or("—"),
                                item.get("session_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("—"),
                                item.get("input").and_then(Value::as_str).unwrap_or("—")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                        + "\n"
                }
            }
        }
    }
}

fn vault_state<'a>(status: &VaultStatus, i18n: &'a I18n) -> &'a str {
    if !status.vault_exists {
        i18n.tr("cli_vault_missing")
    } else if status.locked {
        i18n.tr("cli_vault_locked")
    } else {
        i18n.tr("cli_vault_unlocked")
    }
}

fn yes_no(value: bool, i18n: &I18n) -> &'static str {
    if value {
        i18n.tr("cli_yes")
    } else {
        i18n.tr("cli_no")
    }
}

fn auth_label<'a>(kind: &'a str, i18n: &'a I18n) -> &'a str {
    match kind {
        "password" => i18n.tr("editor_auth_password"),
        "key" => i18n.tr("editor_auth_key_file"),
        _ => kind,
    }
}

fn print_success(as_json: bool, output: Output, i18n: &I18n) {
    if as_json {
        println!("{}", success_envelope(&output));
    } else {
        print!("{}", output.human_text(i18n));
    }
}

fn print_error(as_json: bool, error: &CliError, i18n: &I18n) {
    if as_json {
        println!("{}", error_envelope(error));
    } else {
        eprintln!("{}", i18n.trf("cli_error_prefix", &[&error.message]));
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
    use vida_core::i18n::Lang;

    fn zh() -> I18n {
        I18n::new(Lang::ZhCn)
    }

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
            agent_trust: "ask".to_string(),
        }
    }

    #[test]
    fn host_selector_prefers_exact_id() {
        let selected = select_host(
            vec![host("id-1", "same"), host("same", "other")],
            "same",
            &zh(),
        )
        .expect("exact id");
        assert_eq!(selected.id, "same");
    }

    #[test]
    fn duplicate_host_name_is_not_silently_selected() {
        let error = select_host(
            vec![host("id-1", "same"), host("id-2", "same")],
            "same",
            &zh(),
        )
        .expect_err("ambiguous name");
        assert_eq!(error.code, EXIT_AMBIGUOUS);
        assert_eq!(error.kind, "host_ambiguous");
    }

    #[test]
    fn missing_host_has_stable_error_code() {
        let error = select_host(vec![], "missing", &zh()).expect_err("missing host");
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

    #[test]
    fn human_output_follows_selected_language() {
        let output = Output::Status(VaultStatus {
            locked: false,
            host_count: 2,
            revision: 9,
            device_id: "device-1".into(),
            vault_exists: true,
        });
        let english = output.human_text(&I18n::new(Lang::En));
        assert!(english.contains("Vault exists: Yes"));
        assert!(english.contains("Lock state: Unlocked"));
        assert!(!english.contains("金库"));
    }

    #[test]
    fn session_output_exposes_agent_target_identity() {
        let session = SessionInfo {
            session_id: "session-1".into(),
            title: "Tokyo production".into(),
            target_kind: "ssh".into(),
            host_id: Some("host-1".into()),
            host_name: Some("Tokyo production".into()),
            cols: 120,
            rows: 40,
            alive: true,
            exit_code: None,
            foreground_process: Some("bash".into()),
        };
        let output = Output::Sessions(vec![session]);

        let human = output.human_text(&I18n::new(Lang::En));
        assert!(human.contains("session-1\tTokyo production\tssh\thost-1\t120×40"));
        let json = success_envelope(&output);
        assert_eq!(json["data"][0]["title"], "Tokyo production");
        assert_eq!(json["data"][0]["target_kind"], "ssh");
        assert_eq!(json["data"][0]["host_id"], "host-1");
    }

    #[test]
    fn screen_human_output_is_not_translated() {
        let output = Output::Screen(ScreenData {
            lines: vec!["remote 中文 output".into()],
            wide_cols: vec![],
            cursor: CursorPos { row: 0, col: 0 },
        });
        assert_eq!(
            output.human_text(&I18n::new(Lang::En)),
            "remote 中文 output"
        );
    }

    #[test]
    fn agent_policy_statuses_have_stable_exit_codes_and_json() {
        let pending = Output::AgentExec(json!({
            "status": "needs_approval",
            "approval_id": "approval-1",
            "reasons": ["recursive deletion"]
        }));
        assert_eq!(pending.exit_code(), EXIT_NEEDS_APPROVAL);
        assert_eq!(
            success_envelope(&pending)["data"]["approval_id"],
            "approval-1"
        );

        let rejected = Output::AgentExec(json!({
            "status": "rejected",
            "reason": "readonly",
            "message": "not sent"
        }));
        assert_eq!(rejected.exit_code(), EXIT_REJECTED);

        let sent = Output::AgentExec(json!({"status": "sent"}));
        assert_eq!(sent.exit_code(), 0);
    }
}
