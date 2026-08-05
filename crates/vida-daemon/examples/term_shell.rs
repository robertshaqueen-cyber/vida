//! M2a-1: PTY + Term 打通探针。
//!
//! 真实启动一个 shell，字节流经 Processor 进 Term，把 grid 原地重绘到
//! stdout（看起来就是一个正常终端）。vim / htop / ls --color 显示对
//! 不对，肉眼直接可见。
//!
//! 规格四个必须处理项：
//! a. 阻塞读放在专用 OS 线程（std::thread::spawn），不占 tokio worker
//! b. 子进程退出后 child.wait() 回收，不留僵尸
//! c. 读线程在 EOF/PTY 关闭时干净退出（不泄漏线程）
//! d. 单次读取 ≥8KB（16KB 缓冲，整块喂给 Processor）
//!
//! 交互模型（评审必改）：
//! - PTY 读线程发 Ev::Output / Ev::OutputClosed
//! - stdin 读线程（raw mode，字节透传）发 Ev::Input / Ev::InputClosed
//! - 主循环单点 recv 按事件分派，任一侧无数据都不阻塞另一侧
//! - **Ctrl+] (0x1d) 退出程序**；Ctrl+C (0x03) 原样透传给 shell
//! - 所有输出显式 \r\n（raw mode 关闭 ONLCR，\n 只下移不回车）
//! - 原地重绘：\x1b[H\x1b[2J 清屏 → 逐行 grid → 光标定位，
//!   重绘限频 60fps（16ms 内多次变化合并为一次）
//! - SessionInput 约定：原始字节透传（写入 decisions.md 供 M2a-2 遵循）

use std::io::{Read, Write};
use std::process::Command as StdCommand;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use anyhow::{Context, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Ctrl+] — 退出 term_shell（raw mode 下 Ctrl+C 属于 shell）。
const EXIT_BYTE: u8 = 0x1d;
/// 重绘间隔：60fps。
const REDRAW_INTERVAL: Duration = Duration::from_millis(16);

/// 事件：PTY 输出与 stdin 输入多路复用。
enum Ev {
    /// PTY 输出块。
    Output(Vec<u8>),
    /// stdin 原始字节。
    Input(Vec<u8>),
    /// PTY 读线程 EOF（shell 退出 / PTY 关闭）。
    OutputClosed,
    /// stdin EOF（Ctrl+D）。
    InputClosed,
}

/// raw mode guard：任何路径退出（含 panic）都恢复终端。
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// 网格尺寸参数。
struct ShellSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for ShellSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// 状态消息。raw mode 下 \n 只下移不回车，必须显式 \r\n。
fn say(msg: &str) {
    print!("\r\n[term_shell] {}", msg);
    let _ = std::io::stdout().flush();
}

/// 确定 shell 路径：$SHELL → /bin/zsh → /bin/sh。
fn detect_shell() -> String {
    if let Ok(s) = std::env::var("SHELL")
        && !s.is_empty()
    {
        return s;
    }
    for candidate in ["/bin/zsh", "/bin/sh"] {
        if StdCommand::new(candidate)
            .arg("-c")
            .arg("true")
            .status()
            .is_ok()
        {
            return candidate.to_string();
        }
    }
    "/bin/sh".to_string()
}

/// 统计非默认 cell（验证颜色/属性确实进入了 Term）。
fn count_style_cells(term: &Term<VoidListener>) -> usize {
    use alacritty_terminal::vte::ansi::NamedColor;
    let default_bg = alacritty_terminal::vte::ansi::Color::Named(NamedColor::Background);
    let default_fg = alacritty_terminal::vte::ansi::Color::Named(NamedColor::Foreground);
    term.grid()
        .display_iter()
        .filter(|i| i.cell.fg != default_fg || i.cell.bg != default_bg || !i.cell.flags.is_empty())
        .count()
}

/// 原地重绘：光标归位 → 逐行 grid（每行 \x1b[K 清到行尾，不残留
/// 上一帧字符，省掉整屏 2J）→ grid 下方预留一行画状态消息 →
/// 光标定位。行数 = grid 行数，不多不少。
fn redraw(term: &Term<VoidListener>, status: &str, rows: usize) {
    let mut out = String::with_capacity(4 * 1024);
    out.push_str("\x1b[H");
    let mut current_row: i32 = i32::MIN;
    for indexed in term.grid().display_iter() {
        let point = indexed.point;
        if point.line.0 != current_row {
            if current_row != i32::MIN {
                out.push_str("\x1b[K\r\n");
            }
            current_row = point.line.0;
        }
        out.push(indexed.cell.c);
    }
    out.push_str("\x1b[K"); // 最后一行清到行尾（不换行）
    // 状态行：grid 下方预留一行，循环内的状态消息画在这里，
    // 不会被 redraw 覆盖。
    out.push_str(&format!("\x1b[{};1H\x1b[K{}", rows + 1, status));
    // 光标定位（ANSI 1-based，grid 内 0-based）
    let cursor = term.grid().cursor.point;
    out.push_str(&format!(
        "\x1b[{};{}H",
        cursor.line.0 + 1,
        cursor.column.0 + 1
    ));
    print!("{}", out);
    let _ = std::io::stdout().flush();
}

/// PTY 读线程：阻塞读，发 Ev::Output，EOF 发 Ev::OutputClosed。
fn pty_reader_thread(mut reader: Box<dyn Read + Send>, tx: Sender<Ev>) {
    let mut buf = [0u8; 16 * 1024]; // ≥8KB 缓冲
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF：shell 退出 / PTY 关闭
            Ok(n) => {
                // 整块发送，不逐字节
                if tx.send(Ev::Output(buf[..n].to_vec())).is_err() {
                    return; // 主线程已退出
                }
            }
            Err(_) => break, // 读错误（PTY 关闭）→ 干净退出
        }
    }
    let _ = tx.send(Ev::OutputClosed);
    drop(tx);
}

/// stdin 读线程：raw mode 逐字节透传，发 Ev::Input，EOF 发 Ev::InputClosed。
///
/// 直接用 libc::read(fd 0) 而非 std::io::Stdin：
/// std::io::Stdin 内部有 BufReader 行缓冲，raw mode 下可能与其
/// 冲突（实测：stdin 即时 EOF 时 read 立即 Ok(0)，误触发退出链）。
/// libc::read 是纯 syscall，无缓冲层，行为与真实终端一致。
#[cfg(unix)]
fn stdin_thread(tx: Sender<Ev>) {
    let mut buf = [0u8; 1024];
    loop {
        // EINTR 重试；EOF 返回 0
        let n = loop {
            let r = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if r < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                let _ = tx.send(Ev::InputClosed);
                return;
            }
            break r as usize;
        };
        if n == 0 {
            break;
        }
        if tx.send(Ev::Input(buf[..n].to_vec())).is_err() {
            return;
        }
    }
    let _ = tx.send(Ev::InputClosed);
    drop(tx);
}

fn main() -> Result<()> {
    // ---- 0. 真实终端尺寸（SIGWINCH 动态 resize 留到 M2a-2 的
    //          ResizeSession，本轮不做；此处仅启动时取一次）----
    // crossterm::terminal::size() 在非 tty（管道/重定向）或伪 TTY 下
    // 可能失败或返回 0，均降级 80×24。
    let (cols, rows) = crossterm::terminal::size()
        .map(|(c, r)| (c as usize, r as usize))
        .unwrap_or((80, 24));
    let (cols, rows) = if cols == 0 || rows == 0 {
        (80, 24)
    } else {
        (cols, rows)
    };

    // ---- 1. 打开 PTY（尺寸匹配真实终端）----
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            ..Default::default()
        })
        .context("openpty 失败")?;

    // ---- 2. 启动 shell ----
    let shell = detect_shell();
    let mut cmd = CommandBuilder::new(&shell);
    cmd.env("TERM", "xterm-256color"); // 让 ls --color 等正常工作
    // portable-pty spawn 默认 cwd 是 HOME；探针显式继承当前目录。
    // M2a-2 的 OpenLocalSession 固定使用 HOME（见 decisions.md）。
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawn {} 失败", shell))?;
    drop(pair.slave);

    // ---- 3. Term + Processor ----
    let config = Config {
        scrolling_history: 3000,
        ..Config::default()
    };
    let mut term: Term<VoidListener> = Term::new(config, &ShellSize { cols, rows }, VoidListener);
    let mut processor: Processor<StdSyncHandler> = Processor::new();

    // ---- 4. 事件通道 + PTY 读线程 ----
    let (tx, rx): (Sender<Ev>, Receiver<Ev>) = mpsc::channel();
    let reader_tx = tx.clone();
    let reader = pair
        .master
        .try_clone_reader()
        .context("try_clone_reader 失败")?;
    let reader_handle = thread::spawn(move || pty_reader_thread(reader, reader_tx));

    // ---- 5. stdin raw mode + 读线程 ----
    // 真实终端行为：逐字节透传，Ctrl+C(0x03)/方向键(ESC 序列)/Tab 原样到 PTY。
    // 管道模式（stdin 非 tty）下 raw mode 不可用，降级为行模式继续。
    let raw = enable_raw_mode().is_ok();
    let _guard = RawModeGuard;
    // libc::read(fd 0) 直读，绕开 std::io::Stdin 的 BufReader 行缓冲
    //（raw mode 下可能与行缓冲冲突，实测 stdin 即时 EOF 时误触发退出链）
    let stdin_handle = thread::spawn(move || stdin_thread(tx));
    // 主线程仅接收（两个发送端都已移入线程）

    // ---- 6. 启动提示 + 主循环 ----
    let mut writer: Option<Box<dyn Write + Send>> =
        Some(pair.master.take_writer().context("take_writer 失败")?);
    println!(
        "\r\n[term_shell] shell: {} | raw mode: {} | Ctrl+] 退出 | Ctrl+C 发送给 shell",
        shell, raw
    );

    let mut pty_closed = false;
    // Ctrl+] 已请求退出：关闭 writer 通知 shell（EOF 自行退出），
    // 仍继续消费 PTY 输出直到 OutputClosed。若 2 秒内 shell 未退出
    // （如卡在子进程），才 child.kill() 强制终止（SIGKILL，注意
    // portable-pty 的 kill 是 SIGKILL 不是 SIGHUP）。
    let mut exit_requested = false;
    let mut exit_deadline: Option<Instant> = None;
    const EXIT_KILL_TIMEOUT: Duration = Duration::from_secs(2);
    // 绘制限频状态：Term 处理（advance）不限频，只有绘制限频。
    // dirty = 有内容变化但尚未重绘（16ms 间隔内合并）。
    let mut dirty = false;
    let mut last_redraw = Instant::now() - REDRAW_INTERVAL;
    // 状态行：循环内消息画在 grid 下方，redraw 一并绘制
    let mut status = String::new();

    // ---- 启动即绘制：空网格 + 提示行（让用户知道程序已在运行）----
    redraw(&term, &status, rows);

    while !pty_closed {
        // 带超时轮询：超时 ≤ 16ms，保证超时返回时能补一次重绘
        let ev = rx.recv_timeout(REDRAW_INTERVAL);
        match ev {
            Ok(Ev::Output(bytes)) => {
                // 处理不限频：立即喂给 Processor
                processor.advance(&mut term, &bytes);
                dirty = true;
            }
            Ok(Ev::Input(bytes)) => {
                // Ctrl+] = 退出程序（不转发给 PTY）
                if bytes.contains(&EXIT_BYTE) {
                    if !exit_requested {
                        status =
                            "[term_shell] Ctrl+] 收到，关闭 PTY writer，等待 shell 自行退出".into();
                        dirty = true;
                        writer.take(); // 关闭 writer → shell 收到 EOF
                        exit_requested = true;
                        exit_deadline = Some(Instant::now() + EXIT_KILL_TIMEOUT);
                    }
                } else if let Some(w) = writer.as_mut() {
                    let write_ok = w.write_all(&bytes).is_ok();
                    w.flush().ok();
                    if !write_ok {
                        writer.take(); // PTY 已关闭
                    }
                }
            }
            Ok(Ev::OutputClosed) => {
                status = "[term_shell] PTY 已关闭，等待子进程回收".into();
                pty_closed = true;
            }
            Ok(Ev::InputClosed) => {
                // stdin EOF（Ctrl+D 或 stdin 重定向关闭）。
                // 真实终端中 Ctrl+D 是把 0x04 传给 shell 由 shell 决定，
                // 这里只记录日志，不关闭 writer —— 程序唯一退出出口
                // 是 Ctrl+]。
                status =
                    "[term_shell] stdin EOF（Ctrl+D 或重定向），输入通道关闭，程序继续运行".into();
                dirty = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 无事件：dirty 且已过间隔则补一次重绘
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // 所有发送端关闭
        }

        // Ctrl+] 超时兜底：2 秒内 shell 未退出（OutputClosed 未到）→ SIGKILL
        if let Some(deadline) = exit_deadline
            && !pty_closed
            && Instant::now() >= deadline
        {
            status = "[term_shell] shell 2 秒内未退出，强制终止（SIGKILL）".into();
            dirty = true;
            let _ = child.kill();
            exit_deadline = None;
        }

        // 绘制限频：≥16ms 且有变化才重绘
        if dirty && last_redraw.elapsed() >= REDRAW_INTERVAL {
            redraw(&term, &status, rows);
            last_redraw = Instant::now();
            dirty = false;
        }
    }

    // ---- 7. 最后补一次重绘（把最终状态画出来）+ 统计 ----
    if dirty {
        redraw(&term, &status, rows);
    }
    say(&format!(
        "styled cells: {}（含颜色的 cell 数；颜色重绘留待 --ansi 模式）",
        count_style_cells(&term)
    ));

    // ---- 8. 子进程回收 ----
    match child.wait() {
        Ok(status) => {
            say(&format!(
                "子进程退出: success={} exit_code={}",
                status.success(),
                status.exit_code()
            ));
        }
        Err(e) => say(&format!("wait() 失败: {}", e)),
    }

    // ---- 9. 确保读线程结束（不泄漏线程）----
    let _ = reader_handle.join();
    // stdin 线程阻塞在 read(stdin) 上：Ctrl+] 退出时 stdin 仍打开，
    // join 会死等。程序退出时 OS 回收该线程，这里不 join。
    // （EOF 路径下 stdin 线程早已自行退出，join 与否无副作用。）
    let _ = stdin_handle;
    say("读线程已 join，退出");
    Ok(())
}
