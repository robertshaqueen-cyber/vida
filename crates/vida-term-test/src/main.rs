//! vida-term-test — M2a-3 CLI 验收工具。
//!
//! 与 daemon 通过 WebSocket 通信，用于手动验证终端功能。
//!
//! 注意：所有 send 命令的 data 参数必须是字节数组 JSON。
//! CLI 负责将人的输入（含转义序列）转为字节数组，不把负担甩给使用者。

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tracing::info;

use vida_daemon::protocol::{PtyRequest, Request};

// ---------------------------------------------------------------------------
// CLI 参数
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "vida-term-test", about = "vida 终端验收工具")]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,

    /// daemon WebSocket 地址（默认从 ~/.config/vida/daemon.port 读取）
    #[arg(long, default_value = None)]
    addr: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// 打开会话
    Open {
        #[arg(long, default_value = "80")]
        cols: u16,
        #[arg(long, default_value = "24")]
        rows: u16,
    },
    /// 发送输入（支持转义序列：\\n \\r \\t \\e \\xNN \\u{NNNN}）
    Send {
        session_id: String,
        /// 输入文本（支持转义序列）
        text: String,
    },
    /// 打印屏幕
    Screen {
        session_id: String,
        #[arg(long)]
        ansi: bool,
    },
    /// 调整会话尺寸
    Resize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    /// 监听推送帧
    Watch { session_id: String },
    /// 列出会话
    List,
    /// 关闭会话
    Close { session_id: String },
}

// ---------------------------------------------------------------------------
// 转义序列解析
// ---------------------------------------------------------------------------

/// 将含转义序列的文本转为字节序列。
///
/// 支持的转义：
/// - \\n (0x0A), \\r (0x0D), \\t (0x09), \\e (0x1B)
/// - \\xNN（十六进制字节）
/// - \\u{NNNN}（Unicode 码点 → UTF-8 字节）
fn parse_escape(input: &str) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(0x0A),
            Some('r') => out.push(0x0D),
            Some('t') => out.push(0x09),
            Some('e') => out.push(0x1B),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                let byte = u8::from_str_radix(&hex, 16)
                    .with_context(|| format!("无效的十六进制转义: \\x{}", hex))?;
                out.push(byte);
            }
            Some('u') => {
                if chars.next() != Some('{') {
                    anyhow::bail!("\\u 转义必须使用 {{}} 包裹，如 \\u{{4f60}}");
                }
                let hex: String = chars.by_ref().take_while(|c| *c != '}').collect();
                if chars.next() != Some('}') {
                    anyhow::bail!("\\u 转义未闭合: \\u{{{}", hex);
                }
                let cp = u32::from_str_radix(&hex, 16)
                    .with_context(|| format!("无效的 Unicode 码点: \\u{{{}", hex))?;
                let ch = char::from_u32(cp)
                    .with_context(|| format!("无效的 Unicode 码点: U+{:X}", cp))?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            Some(other) => anyhow::bail!("未知的转义序列: \\{}", other),
            None => anyhow::bail!("转义序列不完整（末尾单个反斜杠）"),
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// WebSocket 客户端
// ---------------------------------------------------------------------------

struct DaemonClient {
    write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    buf: Vec<Vec<u8>>, // 缓冲收到的二进制帧
}

impl DaemonClient {
    async fn connect(addr: &str) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(addr)
            .await
            .with_context(|| format!("连接 daemon 失败: {}", addr))?;
        let (write, read) = ws.split();
        Ok(Self {
            write,
            read,
            buf: Vec::new(),
        })
    }

    /// 发送请求并等待文本响应（跳过二进制帧）。
    async fn call(&mut self, req: &Request) -> Result<Value> {
        let json = serde_json::to_string(req)?;
        self.write
            .send(Message::Text(json.into()))
            .await
            .context("发送请求失败")?;

        loop {
            tokio::select! {
                msg = self.read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            let val: Value = serde_json::from_str(&text)
                                .context("解析响应 JSON 失败")?;
                            if val["type"] == "Error" {
                                anyhow::bail!(
                                    "daemon 返回错误: {}",
                                    val["message"].as_str().unwrap_or("未知错误")
                                );
                            }
                            return Ok(val);
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            self.buf.push(bytes.to_vec());
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => anyhow::bail!("读取错误: {}", e),
                        None => anyhow::bail!("连接关闭"),
                    }
                }
            }
        }
    }

    /// 阻塞读取下一帧（文本或二进制）。
    async fn recv_raw(&mut self) -> Option<Result<Message>> {
        self.read.next().await.map(|r| r.map_err(|e| e.into()))
    }
}

// ---------------------------------------------------------------------------
// 帧解码
// ---------------------------------------------------------------------------

/// 从 JSON 解析带样式的行。
fn parse_styled_rows(rows: &[Value]) -> Result<Vec<Vec<vida_daemon::pty::StyledCell>>> {
    let mut result: Vec<Vec<vida_daemon::pty::StyledCell>> = Vec::new();
    for row in rows {
        let cells = row.as_array().context("rows 应为数组")?;
        let mut row_cells: Vec<vida_daemon::pty::StyledCell> = Vec::new();
        for cell in cells {
            let c = cell["c"].as_str().context("cell.c 缺失")?;
            let ch = c.chars().next().context("cell.c 应为单个字符")?;
            let fg = parse_ansi_color(&cell["fg"])?;
            let bg = parse_ansi_color(&cell["bg"])?;
            let flags = cell["flags"].as_u64().unwrap_or(0) as u8;
            row_cells.push(vida_daemon::pty::StyledCell {
                c: ch,
                fg,
                bg,
                flags,
            });
        }
        result.push(row_cells);
    }
    Ok(result)
}

fn parse_ansi_color(val: &Value) -> Result<vida_daemon::pty::AnsiColor> {
    // serde 枚举格式："Default" 或 {"Indexed": 5} 或 {"Rgb": [r,g,b]}
    if let Some(s) = val.as_str() {
        return match s {
            "Default" => Ok(vida_daemon::pty::AnsiColor::Default),
            other => anyhow::bail!("未知颜色标签: {}", other),
        };
    }
    let obj = val.as_object().context("color 应为字符串或对象")?;
    if let Some(idx) = obj.get("Indexed").and_then(|v| v.as_u64()) {
        return Ok(vida_daemon::pty::AnsiColor::Indexed(idx as u8));
    }
    if let Some(arr) = obj.get("Rgb").and_then(|v| v.as_array()) {
        let r = arr.first().and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        let g = arr.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        let b = arr.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as u8;
        return Ok(vida_daemon::pty::AnsiColor::Rgb(r, g, b));
    }
    anyhow::bail!("无法解析颜色: {:?}", val)
}

/// 从二进制推送帧解析出可读信息（用于 watch 模式）。
fn decode_frame(bytes: &[u8]) -> Option<(u64, usize)> {
    if bytes.len() < 2 || bytes[0] != 0x01 {
        return None;
    }
    let sid_len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    let payload_start = 3 + sid_len;
    if bytes.len() < payload_start + 13 {
        return None;
    }
    let seq = u64::from_be_bytes([
        bytes[payload_start],
        bytes[payload_start + 1],
        bytes[payload_start + 2],
        bytes[payload_start + 3],
        bytes[payload_start + 4],
        bytes[payload_start + 5],
        bytes[payload_start + 6],
        bytes[payload_start + 7],
    ]);
    let line_count = u16::from_be_bytes([bytes[payload_start + 11], bytes[payload_start + 12]]);
    Some((seq, line_count as usize))
}

// ---------------------------------------------------------------------------
// ANSI 颜色输出
// ---------------------------------------------------------------------------

fn render_ansi(
    rows: &[Vec<vida_daemon::pty::StyledCell>],
    _cols: u16,
    cursor: &vida_daemon::pty::CursorPos,
) -> String {
    use vida_daemon::pty::AnsiColor;

    let mut out = String::new();
    let mut prev_flags: u8 = 0;
    let mut prev_fg: Option<&AnsiColor> = None;
    let mut prev_bg: Option<&AnsiColor> = None;

    for (row_idx, row) in rows.iter().enumerate() {
        for cell in row {
            // 检查是否需要重置属性
            let need_reset =
                cell.flags != prev_flags || Some(&cell.fg) != prev_fg || Some(&cell.bg) != prev_bg;

            if need_reset {
                out.push_str("\x1b[0m"); // 重置
                // 前景色
                match &cell.fg {
                    AnsiColor::Default => {}
                    AnsiColor::Indexed(idx) => {
                        if *idx < 16 {
                            out.push_str(&format!("\x1b[38;5;{}m", idx));
                        } else {
                            // 256 色中 16-231 是 6x6x6 色立方，232-255 是灰阶
                            // 简化：直接输出 256 色代码
                            out.push_str(&format!("\x1b[38;5;{}m", idx));
                        }
                    }
                    AnsiColor::Rgb(r, g, b) => {
                        out.push_str(&format!("\x1b[38;2;{};{};{}m", r, g, b));
                    }
                }
                // 背景色
                match &cell.bg {
                    AnsiColor::Default => {}
                    AnsiColor::Indexed(idx) => {
                        out.push_str(&format!("\x1b[48;5;{}m", idx));
                    }
                    AnsiColor::Rgb(r, g, b) => {
                        out.push_str(&format!("\x1b[48;2;{};{};{}m", r, g, b));
                    }
                }
                // 属性
                if cell.flags & 0x01 != 0 {
                    out.push_str("\x1b[1m");
                } // bold
                if cell.flags & 0x02 != 0 {
                    out.push_str("\x1b[3m");
                } // italic
                if cell.flags & 0x04 != 0 {
                    out.push_str("\x1b[4m");
                } // underline
                if cell.flags & 0x08 != 0 {
                    out.push_str("\x1b[7m");
                } // reverse
                if cell.flags & 0x40 != 0 {
                    // wide: 不输出额外控制，由客户端处理
                }
                prev_flags = cell.flags;
                prev_fg = Some(&cell.fg);
                prev_bg = Some(&cell.bg);
            }

            out.push(cell.c);
        }
        // 行末处理：如果下一行不是同一行，换行
        if row_idx < rows.len() - 1 {
            out.push('\n');
        }
    }

    // 光标定位
    out.push_str(&format!(
        "\x1b[{};{}H\x1b[?25h",
        cursor.row + 1,
        cursor.col + 1
    ));

    out
}

// ---------------------------------------------------------------------------
// 主函数
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vida_term_test=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let addr = match cli.addr {
        Some(a) => a,
        None => {
            let port_file = dirs::config_dir()
                .context("无法确定配置目录")?
                .join("vida/daemon.port");
            let port = tokio::fs::read_to_string(&port_file)
                .await
                .with_context(|| format!("读取 daemon 端口文件失败: {}", port_file.display()))?;
            format!("ws://127.0.0.1:{}", port.trim())
        }
    };

    let mut client = DaemonClient::connect(&addr).await?;

    match cli.cmd {
        Commands::Open { cols, rows } => {
            let req = Request::Pty(PtyRequest::OpenLocalSession { cols, rows });
            let resp = client.call(&req).await?;
            println!("{}", resp["result"]["session_id"]);
        }

        Commands::Send { session_id, text } => {
            let bytes = parse_escape(&text)?;
            let len = bytes.len();
            let req = Request::Pty(PtyRequest::SessionInput {
                session_id,
                data: bytes,
            });
            client.call(&req).await?;
            info!("已发送 {} 字节", len);
        }

        Commands::Screen { session_id, ansi } => {
            if ansi {
                let req = Request::Pty(PtyRequest::ReadScreenStyled { session_id });
                let resp = client.call(&req).await?;
                let rows = resp["result"]["rows"].as_array().context("响应格式错误")?;
                let cols = resp["result"]["cols"].as_u64().unwrap_or(80) as u16;
                let cursor = &resp["result"]["cursor"];
                let cp = vida_daemon::pty::CursorPos {
                    row: cursor["row"].as_u64().unwrap_or(0) as u16,
                    col: cursor["col"].as_u64().unwrap_or(0) as u16,
                };
                let styled_rows = parse_styled_rows(rows)?;
                let ansi_out = render_ansi(&styled_rows, cols, &cp);
                print!("{}", ansi_out);
            } else {
                let req = Request::Pty(PtyRequest::ReadScreen { session_id });
                let resp = client.call(&req).await?;
                let lines = resp["result"]["lines"].as_array().context("响应格式错误")?;
                for line in lines {
                    println!("{}", line.as_str().unwrap_or(""));
                }
                let cursor = &resp["result"]["cursor"];
                eprintln!("cursor: row={} col={}", cursor["row"], cursor["col"]);
            }
        }

        Commands::Watch { session_id } => {
            // 先订阅
            let sub_req = Request::Pty(PtyRequest::SubscribeSession {
                session_id: session_id.clone(),
            });
            client.call(&sub_req).await?;

            let mut last_seq: Option<u64> = None;
            let mut last_time = std::time::Instant::now();

            loop {
                match client.recv_raw().await {
                    Some(Ok(Message::Binary(bytes))) => {
                        let now = std::time::Instant::now();
                        let interval_ms = now.duration_since(last_time).as_millis();
                        if let Some((seq, line_count)) = decode_frame(&bytes) {
                            let expected_seq = last_seq.map(|s| s + 1).unwrap_or(seq);
                            let seq_status = if seq == expected_seq { "OK" } else { "GAP!" };
                            println!(
                                "seq={:4} lines={:3} bytes={:5} interval={}ms [{}]",
                                seq,
                                line_count,
                                bytes.len(),
                                interval_ms,
                                seq_status
                            );
                            last_seq = Some(seq);
                        } else {
                            println!(
                                "invalid frame: {} bytes, interval={}ms",
                                bytes.len(),
                                interval_ms
                            );
                        }
                        last_time = now;
                    }
                    Some(Ok(Message::Text(text))) => {
                        println!("text: {}", text);
                    }
                    Some(Ok(_)) => {} // Ping/Pong/Frame 忽略
                    Some(Err(e)) => {
                        eprintln!("error: {}", e);
                        break;
                    }
                    None => {
                        info!("连接关闭");
                        break;
                    }
                }
            }
        }

        Commands::List => {
            let req = Request::Pty(PtyRequest::ListSessions);
            let resp = client.call(&req).await?;
            let sessions = resp["result"].as_array().context("响应格式错误")?;
            if sessions.is_empty() {
                println!("无活跃会话");
            } else {
                for s in sessions {
                    println!(
                        "{}  {}×{}  {}",
                        s["session_id"],
                        s["cols"],
                        s["rows"],
                        if s["alive"].as_bool() == Some(true) {
                            "alive"
                        } else {
                            "closed"
                        }
                    );
                }
            }
        }

        Commands::Resize {
            session_id,
            cols,
            rows,
        } => {
            let req = Request::Pty(PtyRequest::ResizeSession {
                session_id,
                cols,
                rows,
            });
            client.call(&req).await?;
            info!("尺寸已调整为 {}×{}", cols, rows);
        }

        Commands::Close { session_id } => {
            let req = Request::Pty(PtyRequest::CloseSession { session_id });
            client.call(&req).await?;
            info!("会话已关闭");
        }
    }

    Ok(())
}
