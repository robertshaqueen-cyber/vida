use rmcp::{
    ErrorData, Json, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        Implementation, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
        ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use vida_client::{
    AgentHostDraft, AgentHostNotesDocument, AgentHostNotesUpdate, AgentNotesUpdateMode, WsClient,
};

const SERVER_INSTRUCTIONS: &str = "Vida exposes terminal sessions that are also visible to the human in the Vida GUI. Call session_list before screen_read or exec so you can identify the intended local or SSH session by title and host. Use host_list followed by session_open only when the human asks you to connect a configured SSH host; Vida resolves its credential internally and opens the terminal in the GUI. Use host_prepare only when the human asks you to add a new SSH profile: it fills non-secret fields in Vida's normal editor, while the human must choose authentication and save. Each configured host exposes an encrypted-vault-backed Markdown notes resource at vida://host/<host_id>/notes. Read it before making host-specific changes, and call notes_append after completing a change so a future conversation has durable context. Use notes_replace only when the human explicitly asks to replace one section. Read the current screen before acting. Commands always pass through the daemon's Agent policy; if exec returns needs_approval, do not submit it again—ask the human to approve the pending request in the Vida GUI. Never ask Vida tools for passwords, private keys, passphrases, or raw keystrokes because those capabilities are intentionally unavailable.";

#[derive(Clone)]
struct VidaMcp {
    tool_router: ToolRouter<Self>,
    client: std::sync::Arc<Mutex<Option<WsClient>>>,
}

impl VidaMcp {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            client: Default::default(),
        }
    }

    async fn connect(&self) -> Result<WsClient, String> {
        if let Some(client) = self.client.lock().await.clone() {
            return Ok(client);
        }

        let client = WsClient::connect_agent().await.map_err(|error| {
            format!(
                "Unable to connect to the Vida daemon as an Agent. Start vida-daemon and make sure its local configuration files are accessible, then call this tool again. Details: {error:#}"
            )
        })?;
        self.client.lock().await.replace(client.clone());
        Ok(client)
    }

    async fn invalidate_client(&self) {
        self.client.lock().await.take();
    }

    /// Read-only requests may be retried once after reconnecting. They cannot
    /// execute terminal input, so retrying after an ambiguous disconnect is safe.
    async fn read_request(&self, request: ReadRequest) -> Result<Value, String> {
        let client = self.connect().await?;
        match request.send(&client).await {
            Ok(value) => Ok(value),
            Err(first_error) => {
                self.invalidate_client().await;
                let client = self.connect().await.map_err(|reconnect_error| {
                    format!(
                        "The Vida daemon connection was lost and reconnecting failed. Start vida-daemon, then call this read-only tool again. First error: {first_error}. Reconnect error: {reconnect_error}"
                    )
                })?;
                request.send(&client).await.map_err(|error| {
                    format!(
                        "The Vida daemon rejected the read-only request after reconnecting. The vault may be locked or the session may no longer exist. Inspect the Vida GUI, then retry. Details: {error}"
                    )
                })
            }
        }
    }

    /// Writes are deliberately never retried: a lost response does not prove
    /// that the daemon failed to dispatch the command.
    async fn exec_request(&self, session_id: &str, command: &str) -> Result<Value, String> {
        let client = self.connect().await?;
        match client.agent_exec(session_id, command).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.invalidate_client().await;
                Err(format!(
                    "The command result could not be confirmed, so Vida did not retry it. Inspect the target with screen_read and check the Vida GUI approval/audit state before deciding whether to submit a new command. Details: {error}"
                ))
            }
        }
    }

    /// Host preparation is a human-visible write request. Do not retry an
    /// uncertain response because the GUI may already contain the draft.
    async fn host_prepare_request(&self, draft: &AgentHostDraft) -> Result<Value, String> {
        let client = self.connect().await?;
        match client.agent_prepare_host(draft).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.invalidate_client().await;
                Err(format!(
                    "The host draft result could not be confirmed, so Vida did not retry it. Check whether the add-host editor already opened in the Vida GUI before submitting another draft. The vault may also be locked. Details: {error}"
                ))
            }
        }
    }

    async fn notes_update_request(&self, update: &AgentHostNotesUpdate) -> Result<Value, String> {
        let client = self.connect().await?;
        match client.agent_update_host_notes(update).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.invalidate_client().await;
                Err(format!(
                    "The host notes result could not be confirmed, so Vida did not retry it. Read the host notes again before deciding whether another update is needed. The vault may also be locked. Details: {error}"
                ))
            }
        }
    }

    /// Opening by host ID is daemon-idempotent: if the first response is lost,
    /// retrying returns the already-alive matching session instead of creating
    /// another SSH login.
    async fn session_open_request(&self, host_id: &str) -> Result<Value, String> {
        let client = self.connect().await?;
        match client.agent_open_ssh_session(host_id).await {
            Ok(value) => Ok(value),
            Err(first_error) => {
                self.invalidate_client().await;
                let client = self.connect().await.map_err(|reconnect_error| {
                    format!(
                        "The SSH session result could not be confirmed and reconnecting to Vida failed. Inspect the Vida GUI before trying again. First error: {first_error}. Reconnect error: {reconnect_error}"
                    )
                })?;
                client.agent_open_ssh_session(host_id).await.map_err(|error| {
                    format!(
                        "Vida could not open or recover the configured SSH session. The vault may be locked, the host may have been removed, or SSH could not start. Unlock Vida and inspect the host configuration, then retry. Details: {error}"
                    )
                })
            }
        }
    }
}

enum ReadRequest {
    Status,
    Hosts,
    Sessions,
    Screen(String),
    Notes(String),
}

impl ReadRequest {
    async fn send(&self, client: &WsClient) -> vida_client::DaemonResult<Value> {
        match self {
            Self::Status => client.vault_status().await,
            Self::Hosts => client.list_hosts().await,
            Self::Sessions => client.list_sessions().await,
            Self::Screen(session_id) => client.read_screen(session_id).await,
            Self::Notes(host_id) => client.agent_read_host_notes(host_id).await,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct StatusOutput {
    vault_exists: bool,
    locked: bool,
    host_count: usize,
    revision: u64,
    device_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct HostListOutput {
    hosts: Vec<HostOutput>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct HostOutput {
    id: String,
    name: String,
    host: String,
    user: String,
    port: u16,
    tags: Vec<String>,
    group: Option<String>,
    auth_kind: String,
    agent_trust: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HostPrepareInput {
    /// Human-readable profile name shown in Vida.
    name: String,
    /// SSH hostname or IP address. Credentials are not accepted here.
    host: String,
    /// SSH username.
    user: String,
    #[serde(default = "default_ssh_port")]
    #[schemars(default = "default_ssh_port")]
    #[schemars(range(min = 1))]
    port: u16,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

fn default_ssh_port() -> u16 {
    22
}

#[derive(Debug, Serialize, JsonSchema)]
struct HostPrepareOutput {
    draft_id: String,
    /// Always awaiting_human: the vault has not been modified yet.
    status: String,
    next_action: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NotesReadInput {
    /// Exact configured host ID returned by host_list.
    host_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct NotesOutput {
    host_id: String,
    host_name: String,
    revision: u64,
    markdown: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NotesUpdateInput {
    /// Exact configured host ID returned by host_list.
    host_id: String,
    /// Exact level-two Markdown section title, without the leading ##.
    section: String,
    /// Markdown content to append or use as the replacement section body.
    text: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SessionListOutput {
    sessions: Vec<SessionOutput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionOpenInput {
    /// Exact configured host ID returned by host_list.
    host_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SessionOpenOutput {
    session_id: String,
    host_id: String,
    title: String,
    cols: u16,
    rows: u16,
    /// True when Vida returned an already-alive session for this host.
    reused: bool,
    next_action: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SessionOutput {
    session_id: String,
    title: String,
    target_kind: String,
    host_id: Option<String>,
    host_name: Option<String>,
    cols: u16,
    rows: u16,
    alive: bool,
    exit_code: Option<u32>,
    foreground_process: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScreenReadInput {
    /// Exact session_id returned by session_list.
    session_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ScreenReadOutput {
    session_id: String,
    /// Current terminal viewport as plain text. This is not a complete command transcript.
    text: String,
    wide_cols: Vec<Vec<u16>>,
    cursor: CursorOutput,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CursorOutput {
    row: u16,
    col: u16,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExecInput {
    /// Exact session_id returned by session_list.
    session_id: String,
    /// One complete shell command without newline, carriage return, or NUL.
    command: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ExecOutput {
    session_id: String,
    /// sent, needs_approval, or rejected.
    status: String,
    approval_id: Option<String>,
    expires_at: Option<i64>,
    reasons: Vec<String>,
    matched_rules: Vec<String>,
    reason: Option<String>,
    message: Option<String>,
    next_action: String,
}

fn decode<T: serde::de::DeserializeOwned>(value: Value, operation: &str) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| {
        format!(
            "Vida returned an unexpected {operation} response. Update vida-mcp, vidactl, the GUI, and vida-daemon together, then retry. Details: {error}"
        )
    })
}

fn string_list(value: Option<&Value>, object_key: &str) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str().map(ToOwned::to_owned).or_else(|| {
                item.get(object_key)
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
        })
        .collect()
}

fn decode_exec_output(session_id: String, value: Value) -> ExecOutput {
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("rejected")
        .to_string();
    let approval_id = value
        .get("approval_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let expires_at = value.get("expires_at").and_then(Value::as_i64);
    let reasons = string_list(value.get("reasons"), "reason");
    let matched_rules = string_list(value.get("matched_rules"), "rule_id");
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let next_action = match status.as_str() {
        "sent" => "Use screen_read to observe the command result before taking another action.",
        "needs_approval" => "Do not call exec again for this command. Ask the human to approve the pending request in the Vida GUI, then use screen_read after they decide.",
        _ => "The command was not sent. Read the message, inspect the session and host Agent trust in the Vida GUI, and revise the plan.",
    }
    .to_string();

    ExecOutput {
        session_id,
        status,
        approval_id,
        expires_at,
        reasons,
        matched_rules,
        reason,
        message,
        next_action,
    }
}

#[tool_router(router = tool_router)]
impl VidaMcp {
    #[tool(
        name = "vida_status",
        description = "Use when you need to verify that Vida is reachable and learn whether its vault is locked before other Vida operations. Do not use when you need terminal contents, session identity, credentials, or host secrets; use session_list, screen_read, or host_list as appropriate."
    )]
    async fn vida_status(&self) -> Result<Json<StatusOutput>, String> {
        let value = self.read_request(ReadRequest::Status).await?;
        Ok(Json(decode(value, "status")?))
    }

    #[tool(
        name = "host_list",
        description = "Use when you need safe summaries of configured Vida hosts, including IDs, display names, addresses, authentication kind, and Agent trust, or before session_open to select an exact host ID. Do not use to retrieve credentials, private keys, or passphrases; those capabilities are intentionally unavailable, and active terminals are discovered with session_list."
    )]
    async fn host_list(&self) -> Result<Json<HostListOutput>, String> {
        let value = self.read_request(ReadRequest::Hosts).await?;
        let hosts = decode(value, "host list")?;
        Ok(Json(HostListOutput { hosts }))
    }

    #[tool(
        name = "host_prepare",
        description = "Use only when the human asks you to add a new SSH host and has the Vida GUI open. Provide non-secret profile metadata; Vida opens its normal add-host editor so the human can select authentication, review every field, and save. Do not use for passwords, private keys, passphrases, trust-policy changes, unattended host creation, edits to existing hosts, or speculative inventory building."
    )]
    async fn host_prepare(
        &self,
        Parameters(input): Parameters<HostPrepareInput>,
    ) -> Result<Json<HostPrepareOutput>, String> {
        let draft = AgentHostDraft {
            name: input.name,
            host: input.host,
            user: input.user,
            port: input.port,
            tags: input.tags,
            group: input.group,
            notes: input.notes,
        };
        let value = self.host_prepare_request(&draft).await?;
        #[derive(Deserialize)]
        struct RawHostPrepareOutput {
            draft_id: String,
            status: String,
        }
        let raw: RawHostPrepareOutput = decode(value, "host prepare")?;
        Ok(Json(HostPrepareOutput {
            draft_id: raw.draft_id,
            status: raw.status,
            next_action: "Ask the human to review the prefilled Vida add-host editor, choose authentication, and click Save. The host does not exist until the human saves it."
                .into(),
        }))
    }

    #[tool(
        name = "notes_read",
        description = "Use after host_list to read the durable Markdown operations record for one configured host, especially before making host-specific changes or answering what is installed/configured. Do not use for live terminal output, credentials, or unconfigured hosts; use screen_read for the current terminal screen."
    )]
    async fn notes_read(
        &self,
        Parameters(input): Parameters<NotesReadInput>,
    ) -> Result<Json<NotesOutput>, String> {
        let value = self.read_request(ReadRequest::Notes(input.host_id)).await?;
        Ok(Json(decode(value, "host notes")?))
    }

    #[tool(
        name = "notes_append",
        description = "Use after completing a change to a configured host to append concise durable Markdown under one named section, so future conversations know what changed. Also use when the human explicitly asks you to record operational context. Do not use for credentials, command transcripts, temporary observations, speculative plans, or replacing existing history; use notes_replace only for an explicitly requested correction."
    )]
    async fn notes_append(
        &self,
        Parameters(input): Parameters<NotesUpdateInput>,
    ) -> Result<Json<NotesOutput>, String> {
        self.update_notes(input, AgentNotesUpdateMode::Append).await
    }

    #[tool(
        name = "notes_replace",
        description = "Use only when the human explicitly asks you to correct or replace one existing host-notes section. The named section body is replaced while all other Markdown sections are preserved. Do not use for ordinary change logging, credentials, whole-document rewrites, live terminal output, or silent cleanup; use notes_append for normal durable updates."
    )]
    async fn notes_replace(
        &self,
        Parameters(input): Parameters<NotesUpdateInput>,
    ) -> Result<Json<NotesOutput>, String> {
        self.update_notes(input, AgentNotesUpdateMode::Replace)
            .await
    }

    #[tool(
        name = "session_list",
        description = "Use before screen_read or exec to identify the intended visible local or SSH terminal by title, host, target kind, and session_id. Do not guess a session ID or use host_list as a substitute; configured hosts are not necessarily active terminal sessions."
    )]
    async fn session_list(&self) -> Result<Json<SessionListOutput>, String> {
        let value = self.read_request(ReadRequest::Sessions).await?;
        let sessions = decode(value, "session list")?;
        Ok(Json(SessionListOutput { sessions }))
    }

    #[tool(
        name = "session_open",
        description = "Use after host_list when the human asks you to connect one SSH host already configured in Vida. The daemon uses the vault credential internally, creates a human-visible GUI terminal, and reuses an alive session for repeated requests. Do not use for local terminals, unconfigured addresses, credential entry, host creation, or speculative/background connections."
    )]
    async fn session_open(
        &self,
        Parameters(input): Parameters<SessionOpenInput>,
    ) -> Result<Json<SessionOpenOutput>, String> {
        let value = self.session_open_request(&input.host_id).await?;
        #[derive(Deserialize)]
        struct RawSessionOpenOutput {
            session_id: String,
            host_id: String,
            title: String,
            cols: u16,
            rows: u16,
            reused: bool,
        }
        let raw: RawSessionOpenOutput = decode(value, "session open")?;
        Ok(Json(SessionOpenOutput {
            next_action: format!(
                "Call session_list to confirm the target, then screen_read on session {} before sending any command.",
                raw.session_id
            ),
            session_id: raw.session_id,
            host_id: raw.host_id,
            title: raw.title,
            cols: raw.cols,
            rows: raw.rows,
            reused: raw.reused,
        }))
    }

    #[tool(
        name = "screen_read",
        description = "Use after session_list to inspect the current visible terminal viewport before acting and after a command to observe its result. Do not treat this as a complete shell transcript or use it for a stale/guessed session_id; scrollback outside the current viewport may be absent."
    )]
    async fn screen_read(
        &self,
        Parameters(input): Parameters<ScreenReadInput>,
    ) -> Result<Json<ScreenReadOutput>, String> {
        let value = self
            .read_request(ReadRequest::Screen(input.session_id.clone()))
            .await?;
        #[derive(Deserialize)]
        struct RawScreen {
            lines: Vec<String>,
            wide_cols: Vec<Vec<u16>>,
            cursor: CursorOutput,
        }
        let raw: RawScreen = decode(value, "screen")?;
        // PTY rows are padded to the terminal width. They are useful to a
        // renderer but waste Agent context; trailing blanks carry no visible
        // information, while cursor and wide-column coordinates remain intact.
        let lines = raw
            .lines
            .into_iter()
            .map(|line| line.trim_end_matches(' ').to_string())
            .collect::<Vec<_>>();
        Ok(Json(ScreenReadOutput {
            session_id: input.session_id,
            text: lines.join("\n"),
            wide_cols: raw.wide_cols,
            cursor: raw.cursor,
        }))
    }

    #[tool(
        name = "exec",
        description = "Use only after session_list and screen_read when you need to send one complete non-interactive shell command to a specific visible terminal. Do not use for passwords, private keys, passphrases, raw keystrokes, multiline input, or approval decisions. If status is needs_approval, do not resubmit: ask the human to approve it in the Vida GUI, then inspect the terminal with screen_read."
    )]
    async fn exec(
        &self,
        Parameters(input): Parameters<ExecInput>,
    ) -> Result<Json<ExecOutput>, String> {
        let value = self.exec_request(&input.session_id, &input.command).await?;
        Ok(Json(decode_exec_output(input.session_id, value)))
    }

    async fn update_notes(
        &self,
        input: NotesUpdateInput,
        mode: AgentNotesUpdateMode,
    ) -> Result<Json<NotesOutput>, String> {
        let update = AgentHostNotesUpdate {
            host_id: input.host_id,
            section: input.section,
            text: input.text,
            mode,
        };
        let value = self.notes_update_request(&update).await?;
        Ok(Json(decode(value, "host notes update")?))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for VidaMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_tools()
                .build(),
        )
        .with_server_info(Implementation::new("vida-mcp", env!("CARGO_PKG_VERSION")))
        .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let value = self
            .read_request(ReadRequest::Hosts)
            .await
            .map_err(|error| ErrorData::internal_error(error, None))?;
        let hosts: Vec<HostOutput> = decode(value, "host resources")
            .map_err(|error| ErrorData::internal_error(error, None))?;
        let resources = hosts
            .into_iter()
            .map(|host| {
                Resource::new(
                    format!("vida://host/{}/notes", host.id),
                    format!("{} operations notes", host.name),
                )
                .with_title(format!("{} · Operations notes", host.name))
                .with_description(
                    "Encrypted-vault-backed Markdown context for this configured Vida host. Read before host-specific work; update through notes_append or notes_replace.",
                )
                .with_mime_type("text/markdown")
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let host_id = request
            .uri
            .strip_prefix("vida://host/")
            .and_then(|rest| rest.strip_suffix("/notes"))
            .filter(|host_id| !host_id.is_empty() && !host_id.contains('/'))
            .ok_or_else(|| {
                ErrorData::resource_not_found(
                    "Vida host notes resource not found; call resources/list for a current URI",
                    None,
                )
            })?;
        let value = self
            .read_request(ReadRequest::Notes(host_id.to_string()))
            .await
            .map_err(|error| ErrorData::internal_error(error, None))?;
        let document: AgentHostNotesDocument = decode(value, "host notes resource")
            .map_err(|error| ErrorData::internal_error(error, None))?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(document.markdown, request.uri).with_mime_type("text/markdown"),
        ])
        .into())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout belongs exclusively to MCP JSON-RPC. Any future diagnostics must
    // use stderr and must never include commands or credential material.
    VidaMcp::new().serve(stdio()).await?.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_only_the_ten_reviewed_tools() {
        let server = VidaMcp::new();
        let mut names = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            [
                "exec",
                "host_list",
                "host_prepare",
                "notes_append",
                "notes_read",
                "notes_replace",
                "screen_read",
                "session_list",
                "session_open",
                "vida_status"
            ]
        );
    }

    #[test]
    fn every_tool_description_states_when_to_use_and_not_use() {
        let server = VidaMcp::new();
        for tool in server.tool_router.list_all() {
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(
                description.contains("Use "),
                "{} lacks Use guidance",
                tool.name
            );
            assert!(
                description.contains("Do not"),
                "{} lacks Do not use guidance",
                tool.name
            );
        }
    }

    #[test]
    fn output_schemas_are_declared_for_agent_validation() {
        let server = VidaMcp::new();
        for tool in server.tool_router.list_all() {
            assert!(
                tool.output_schema.is_some(),
                "{} has no output schema",
                tool.name
            );
        }
    }

    #[test]
    fn danger_matches_are_reduced_to_non_command_rule_ids() {
        let value = serde_json::json!([
            {"rule_id": "recursive_delete", "reason": "recursive deletion"},
            "legacy_rule"
        ]);
        assert_eq!(
            string_list(Some(&value), "rule_id"),
            ["recursive_delete", "legacy_rule"]
        );
    }

    #[test]
    fn terminal_padding_is_not_part_of_agent_text() {
        let lines = ["prompt %   ", "output", "     "]
            .into_iter()
            .map(|line| line.trim_end_matches(' ').to_string())
            .collect::<Vec<_>>();
        assert_eq!(lines, ["prompt %", "output", ""]);
    }

    #[test]
    fn approval_result_tells_agent_to_wait_without_echoing_command() {
        let output = decode_exec_output(
            "session-1".to_string(),
            serde_json::json!({
                "status": "needs_approval",
                "approval_id": "approval-1",
                "command": "sensitive command text",
                "reasons": ["recursive or forced file deletion"],
                "matched_rules": [{"rule_id": "recursive_delete", "reason": "delete"}],
                "expires_at": 1234
            }),
        );
        let json = serde_json::to_value(output).expect("serialize output");
        assert_eq!(json["status"], "needs_approval");
        assert!(
            json["next_action"]
                .as_str()
                .unwrap()
                .contains("Do not call exec again")
        );
        assert!(!json.to_string().contains("sensitive command text"));
    }
}
