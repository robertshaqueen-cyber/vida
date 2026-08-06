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

use vida_daemon::protocol::{PtyRequest, Request};

// ---------------------------------------------------------------------------
// CLI 参数
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "vida-term-test", about = "vida 终端验收工具")]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,

    /// daemon WebSocket 地址（默认从配置目录的 daemon.port 读取）
    #[arg(long, global = true)]
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
        /// 用 ^ 标记 wide_cols 报告的位置（验证中文对齐）
        #[arg(long)]
        show_wide: bool,
    },
    /// 调整会话尺寸
    Resize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    /// 监听推送帧
    Watch {
        session_id: String,
        /// 运行 N 秒后自动退出并打印统计（Ctrl+C 同样触发）
        #[arg(long)]
        duration: Option<u64>,
    },
    /// 列出会话
    List,
    /// 关闭会话
    Close { session_id: String },
}

// ---------------------------------------------------------------------------
// 转义序列解析
// ---------------------------------------------------------------------------

/// 从配置文件读取 daemon token。
/// 与 daemon 相同的配置目录解析：优先 VIDA_CONFIG_DIR，否则平台默认。
fn read_token() -> Result<String> {
    let config_dir = if let Ok(dir) = std::env::var("VIDA_CONFIG_DIR") {
        std::path::PathBuf::from(dir)
    } else {
        dirs::config_dir()
            .context("无法确定配置目录路径。请设置环境变量 VIDA_CONFIG_DIR 指定配置目录。")?
            .join("vida")
    };
    let token_path = config_dir.join("daemon.token");
    let token = std::fs::read_to_string(&token_path)
        .with_context(|| format!("读取 token 失败: {}", token_path.display()))?;
    Ok(token.trim().to_string())
}

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
            Some('\\') => out.push(0x5C),
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
                let mut hex = String::new();
                loop {
                    match chars.next() {
                        Some(c) if c != '}' => hex.push(c),
                        Some('}') => break,
                        Some(_) => {}
                        None => anyhow::bail!("\\u 转义未闭合: \\u{{{}", hex),
                    }
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
        let (mut write, read) = ws.split();

        // 认证：读取 token 并发送 Auth，等待认证响应
        let token = read_token()?;
        let auth_req = serde_json::json!({
            "method": "Auth",
            "params": { "token": token },
            "id": 0
        });
        write
            .send(Message::Text(auth_req.to_string().into()))
            .await
            .context("发送认证失败")?;

        // 等待认证响应
        let mut read = read;
        loop {
            match read.next().await {
                Some(Ok(Message::Text(text))) => {
                    let val: Value = serde_json::from_str(&text).context("解析认证响应失败")?;
                    if val["type"] == "Error" {
                        anyhow::bail!("认证失败: {}", val["message"]);
                    }
                    break;
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => anyhow::bail!("认证时读取错误: {}", e),
                None => anyhow::bail!("认证时连接关闭"),
            }
        }

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
    // tracing 输出写 stderr，保证 stdout 只有命令结果
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vida_term_test=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let addr = match cli.addr {
        Some(a) => a,
        None => {
            let config_dir = if let Ok(dir) = std::env::var("VIDA_CONFIG_DIR") {
                std::path::PathBuf::from(dir)
            } else {
                dirs::config_dir()
                    .context("无法确定配置目录路径。请设置环境变量 VIDA_CONFIG_DIR 指定配置目录。")?
                    .join("vida")
            };
            let port_file = config_dir.join("daemon.port");
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
            println!(
                "{}",
                resp["result"]["session_id"]
                    .as_str()
                    .context("session_id 缺失")?
            );
        }

        Commands::Send { session_id, text } => {
            let bytes = parse_escape(&text)?;
            let len = bytes.len();
            let req = Request::Pty(PtyRequest::SessionInput {
                session_id,
                data: bytes,
            });
            client.call(&req).await?;
            eprintln!("已发送 {} 字节", len);
        }

        Commands::Screen {
            session_id,
            ansi,
            show_wide,
        } => {
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
                let cursor = &resp["result"]["cursor"];
                let wide_cols = resp["result"]["wide_cols"]
                    .as_array()
                    .context("wide_cols 缺失")?;

                // 每行：行号右对齐两位 + 内容（行尾空格裁掉）
                let mut trimmed_any = false;
                for (i, line) in lines.iter().enumerate() {
                    let raw = line.as_str().unwrap_or("");
                    let trimmed = raw.trim_end();
                    if trimmed.len() != raw.len() {
                        trimmed_any = true;
                    }
                    println!("{:2}| {}", i, trimmed);
                }
                if trimmed_any {
                    eprintln!("行尾空格已省略");
                }
                // --show-wide：用 ^ 标记 wide_cols 报告的位置
                // 宽度取该行内容在终端的显示宽度（宽字符算 2 列）
                if show_wide {
                    let mut show_any = false;
                    for (i, wc) in wide_cols.iter().enumerate() {
                        let cols: Vec<usize> = wc
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_u64())
                                    .map(|v| v as usize)
                                    .collect()
                            })
                            .unwrap_or_default();
                        if cols.is_empty() {
                            continue;
                        }
                        let raw = lines[i].as_str().unwrap_or("");
                        let disp_w = raw
                            .chars()
                            .map(|c| if c as u32 > 0xFF { 2 } else { 1 })
                            .sum::<usize>();
                        let mut marker = String::with_capacity(disp_w + 1);
                        for c in 0..disp_w {
                            if cols.contains(&c) {
                                marker.push('^');
                            } else {
                                marker.push(' ');
                            }
                        }
                        println!("{:2}| {}", i, marker);
                        show_any = true;
                    }
                    if show_any {
                        eprintln!("^ = 宽字符起始列（wide_cols）");
                    }
                }
                eprintln!("cursor: row={} col={}", cursor["row"], cursor["col"]);
                // 宽字符提示（wide_cols 报告的位置）
                let wide_total: usize = wide_cols
                    .iter()
                    .map(|r| r.as_array().map(|a| a.len()).unwrap_or(0))
                    .sum();
                if wide_total > 0 {
                    eprintln!(
                        "wide_cols: 共 {} 个宽字符位置（见响应 wide_cols 字段）",
                        wide_total
                    );
                }
            }
        }

        Commands::Watch {
            session_id,
            duration,
        } => {
            // 先订阅
            let sub_req = Request::Pty(PtyRequest::SubscribeSession {
                session_id: session_id.clone(),
            });
            client.call(&sub_req).await?;
            eprintln!("已订阅 {}（Ctrl+C 或 --duration 到期退出）", session_id);

            let mut last_seq: Option<u64> = None;
            let mut last_time = std::time::Instant::now();
            let start_time = std::time::Instant::now();
            let mut frame_count: u64 = 0;
            let mut total_bytes: u64 = 0;
            let mut intervals_ms: Vec<u128> = Vec::new();

            // 退出信号：Ctrl+C 或 duration 到期（duration 用截止时间，不随循环重置）
            let duration_deadline =
                duration.map(|d| std::time::Instant::now() + std::time::Duration::from_secs(d));

            loop {
                let ctrl_c = tokio::signal::ctrl_c();
                let duration_sleep = async {
                    if let Some(deadline) = duration_deadline {
                        tokio::time::sleep_until(deadline.into()).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                };
                tokio::select! {
                    msg = client.recv_raw() => {
                        match msg {
                            Some(Ok(Message::Binary(bytes))) => {
                                let now = std::time::Instant::now();
                                let interval_ms = now.duration_since(last_time).as_millis();
                                last_time = now;
                                intervals_ms.push(interval_ms);
                                frame_count += 1;
                                total_bytes += bytes.len() as u64;

                                if let Some((seq, line_count)) = decode_frame(&bytes) {
                                    let expected_seq = last_seq.map(|s| s + 1).unwrap_or(seq);
                                    let seq_status = if seq == expected_seq { "OK" } else { "GAP!" };
                                    println!(
                                        "seq={:4} lines={:3} bytes={:5} interval={}ms [{}]",
                                        seq, line_count, bytes.len(), interval_ms, seq_status
                                    );
                                    last_seq = Some(seq);
                                } else {
                                    println!(
                                        "invalid frame: {} bytes, interval={}ms",
                                        bytes.len(), interval_ms
                                    );
                                }
                            }
                            Some(Ok(Message::Text(text))) => {
                                println!("text: {}", text);
                            }
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                eprintln!("error: {}", e);
                                break;
                            }
                            None => {
                                eprintln!("连接关闭");
                                break;
                            }
                        }
                    }
                    _ = ctrl_c => {
                        eprintln!("\n[watch] Ctrl+C 收到，退出");
                        break;
                    }
                    _ = duration_sleep => {
                        eprintln!("\n[watch] --duration 到期，退出");
                        break;
                    }
                }
            }

            // 统计
            let elapsed = start_time.elapsed().as_secs_f64();
            let avg_interval = if !intervals_ms.is_empty() {
                intervals_ms.iter().sum::<u128>() as f64 / intervals_ms.len() as f64
            } else {
                0.0
            };
            let min_interval = intervals_ms.iter().min().copied().unwrap_or(0);
            let max_interval = intervals_ms.iter().max().copied().unwrap_or(0);
            let below_16 = intervals_ms.iter().filter(|&&v| v < 16).count();
            let below_pct = if intervals_ms.is_empty() {
                0.0
            } else {
                100.0 * below_16 as f64 / intervals_ms.len() as f64
            };
            let fps = if elapsed > 0.0 {
                frame_count as f64 / elapsed
            } else {
                0.0
            };

            eprintln!(
                "=== 统计 ===\n\
                 帧数: {}（{:.1} fps）\n\
                 总字节: {}（{:.2} KB/s）\n\
                 平均间隔: {:.1} ms\n\
                 最小间隔: {} ms\n\
                 最大间隔: {} ms\n\
                 低于 16ms: {} 帧（{:.1}%）",
                frame_count,
                fps,
                total_bytes,
                total_bytes as f64 / elapsed.max(0.001) / 1000.0,
                avg_interval,
                min_interval,
                max_interval,
                below_16,
                below_pct
            );
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
            eprintln!("尺寸已调整为 {}×{}", cols, rows);
        }

        Commands::Close { session_id } => {
            let req = Request::Pty(PtyRequest::CloseSession { session_id });
            client.call(&req).await?;
            eprintln!("会话已关闭");
        }
    }

    // 发送 WebSocket Close 帧并等待对端回应（短超时），
    // 避免 daemon 侧记录「无关闭握手」的断开日志。
    if let Err(e) = client.write.send(Message::Close(None)).await {
        eprintln!("发送 Close 帧失败（连接可能已断开）: {}", e);
    }
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), client.read.next()).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::parse_escape;

    /// 必改 1：\xNN 转义解析必须产生正确的字节值。
    #[test]
    fn escape_xnn_produces_correct_bytes() {
        assert_eq!(parse_escape("\\x03").unwrap(), vec![3]);
        assert_eq!(parse_escape("\\x1b").unwrap(), vec![27]);
        assert_eq!(parse_escape("\\x41").unwrap(), vec![65]); // 'A'
    }

    /// 必改 1：常用转义序列。
    #[test]
    fn escape_common_sequences() {
        assert_eq!(parse_escape("\\n").unwrap(), vec![10]);
        assert_eq!(parse_escape("\\r").unwrap(), vec![13]);
        assert_eq!(parse_escape("\\t").unwrap(), vec![9]);
        assert_eq!(parse_escape("\\e").unwrap(), vec![27]);
        assert_eq!(parse_escape("\\\\").unwrap(), vec![92]);
    }

    /// 必改 1：\u{NNNN} 转义为 UTF-8 字节。
    #[test]
    fn escape_unicode_codepoint() {
        // 「你」= U+4F60 = UTF-8 [228, 189, 160]
        assert_eq!(parse_escape("\\u{4f60}").unwrap(), vec![228, 189, 160]);
    }

    /// 必改 1：混合文本 + 转义。
    #[test]
    fn escape_mixed_text() {
        assert_eq!(parse_escape("echo hi\\n").unwrap(), b"echo hi\n".to_vec());
        assert_eq!(
            parse_escape("ls --color\\n").unwrap(),
            b"ls --color\n".to_vec()
        );
    }

    /// 必改 1：无效转义应报错而非静默产出错误字节。
    #[test]
    fn escape_invalid_errors() {
        assert!(parse_escape("\\q").is_err(), "未知转义应报错");
        assert!(parse_escape("\\x").is_err(), "不完整 \\x 应报错");
        assert!(parse_escape("\\xZZ").is_err(), "非法十六进制应报错");
        assert!(parse_escape("\\u{110000}").is_err(), "超范围码点应报错");
    }
}
