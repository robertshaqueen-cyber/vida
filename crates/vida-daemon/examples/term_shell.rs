//! M2a-1: PTY + Term 打通探针。
//!
//! 真实启动一个 shell，字节流经 Processor 进 Term，把 grid 打印到 stdout。
//! 运行后在终端里敲命令（echo hello / ls --color / pwd），
//! 每次输出后打印一次当前屏幕网格 + 光标。
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
//!   → sleep / htop / Ctrl+C 都能正常交互
//! - SessionInput 约定：原始字节透传，daemon 不做行缓冲/换行转换/
//!   按键解释（写入 decisions.md 供 M2a-2 遵循）

use std::io::{Read, Write};
use std::process::Command as StdCommand;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use anyhow::{Context, Result};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

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

/// 打印当前屏幕（display_iter 遍历可见区）。
fn print_grid(term: &Term<VoidListener>) {
    let mut current_row: i32 = i32::MIN;
    for indexed in term.grid().display_iter() {
        let point = indexed.point;
        if point.line.0 != current_row {
            if current_row != i32::MIN {
                println!();
            }
            current_row = point.line.0;
            print!("{:2}| ", point.line.0);
        }
        print!("{}", indexed.cell.c);
    }
    println!();
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

/// 把一块字节喂进 Term 并打印屏幕。
fn process_and_print(
    processor: &mut Processor<StdSyncHandler>,
    term: &mut Term<VoidListener>,
    bytes: &[u8],
) {
    processor.advance(term, bytes);
    let styled = count_style_cells(term);
    println!("--- screen (styled cells: {}) ---", styled);
    print_grid(term);
    let cursor = term.grid().cursor.point;
    println!(
        "--- cursor: row={} col={} ---",
        cursor.line.0, cursor.column.0
    );
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
    println!("[term_shell] 读线程已退出（EOF/关闭）");
}

/// stdin 读线程：raw mode 逐字节透传，发 Ev::Input，EOF 发 Ev::InputClosed。
fn stdin_thread(mut stdin: std::io::Stdin, tx: Sender<Ev>) {
    let mut buf = [0u8; 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break, // stdin EOF（Ctrl+D）
            Ok(n) => {
                if tx.send(Ev::Input(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(_) => break,
        }
    }
    let _ = tx.send(Ev::InputClosed);
    drop(tx);
    println!("[term_shell] stdin 线程已退出（EOF）");
}

fn main() -> Result<()> {
    // ---- 1. 打开 PTY 80×24 ----
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
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
    println!("[term_shell] shell: {}", shell);

    // ---- 3. Term + Processor ----
    let config = Config {
        scrolling_history: 3000,
        ..Config::default()
    };
    let mut term: Term<VoidListener> =
        Term::new(config, &ShellSize { cols: 80, rows: 24 }, VoidListener);
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
    // 管道模式（stdin 非 tty）下 raw mode 不可用，降级为行模式继续，
    // 透传逻辑不变（管道输入本来就是整块到达）。
    let raw = enable_raw_mode().is_ok();
    let _guard = RawModeGuard;
    if raw {
        println!("[term_shell] stdin raw mode 已启用（字节透传）");
    } else {
        println!("[term_shell] stdin 非 tty，raw mode 跳过（降级行模式）");
    }
    let stdin = std::io::stdin();
    let stdin_handle = thread::spawn(move || stdin_thread(stdin, tx));
    // 主线程仅接收（两个发送端都已移入线程）

    // ---- 6. 主循环：单点 recv，按事件类型分派 ----
    // writer 用 Option 包裹，InputClosed 时 take 掉并 drop（通知 shell EOF）
    let mut writer: Option<Box<dyn Write + Send>> =
        Some(pair.master.take_writer().context("take_writer 失败")?);
    let mut pty_closed = false;

    while !pty_closed {
        match rx.recv() {
            Ok(Ev::Output(bytes)) => process_and_print(&mut processor, &mut term, &bytes),
            Ok(Ev::Input(bytes)) => {
                if let Some(w) = writer.as_mut()
                    && w.write_all(&bytes).is_err()
                {
                    writer.take(); // PTY 已关闭
                }
                if let Some(w) = writer.as_mut() {
                    w.flush().ok();
                }
            }
            Ok(Ev::OutputClosed) => {
                println!("[term_shell] PTY 已关闭，等待子进程回收");
                pty_closed = true;
            }
            Ok(Ev::InputClosed) => {
                // stdin EOF（Ctrl+D）→ 关闭 PTY writer，通知 shell 退出
                if writer.take().is_some() {
                    println!("[term_shell] stdin EOF，关闭 PTY writer");
                }
            }
            Err(_) => break, // 所有发送端关闭
        }
    }

    // ---- 7. 子进程回收 ----
    match child.wait() {
        Ok(status) => {
            println!(
                "[term_shell] 子进程退出: success={} exit_code={}",
                status.success(),
                status.exit_code()
            );
        }
        Err(e) => println!("[term_shell] wait() 失败: {}", e),
    }

    // ---- 8. 确保两个读线程结束（不泄漏线程）----
    let _ = reader_handle.join();
    let _ = stdin_handle.join();
    println!("[term_shell] 线程已 join，退出");
    Ok(())
}
