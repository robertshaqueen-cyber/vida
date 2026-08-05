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

use std::io::{BufRead, Read, Write};
use std::process::Command as StdCommand;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

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
    // portable-pty spawn 默认 cwd 是 HOME；显式继承当前目录，
    // 让 ls/pwd 显示与运行目录一致（daemon 接入时由上层决定 cwd）。
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawn {} 失败", shell))?;
    drop(pair.slave);
    println!("[term_shell] shell: {} (pid 见下方 exit 输出)", shell);

    // ---- 3. Term + Processor ----
    let config = Config {
        scrolling_history: 3000,
        ..Config::default()
    };
    let mut term: Term<VoidListener> =
        Term::new(config, &ShellSize { cols: 80, rows: 24 }, VoidListener);
    let mut processor: Processor<StdSyncHandler> = Processor::new();

    // ---- 4. 读线程（阻塞读，专用 OS 线程）----
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("try_clone_reader 失败")?;
    let (tx, rx): (mpsc::Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();

    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024]; // ≥8KB 缓冲
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF：shell 退出 / PTY 关闭
                Ok(n) => {
                    // 整块发送，不逐字节
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break; // 主线程已退出
                    }
                }
                Err(_) => break, // 读错误（PTY 关闭）→ 干净退出
            }
        }
        drop(tx);
        println!("[term_shell] 读线程已退出（EOF/关闭）");
    });

    // ---- 5. 主线程：把 PTY 输出喂进 Term，处理 stdin 输入 ----
    let mut writer = pair.master.take_writer().context("take_writer 失败")?;
    let stdin = std::io::stdin();
    let mut stdin_lines = stdin.lock();

    // 主循环：读 channel 喂 Processor → 打印 grid；stdin 有输入则写入 PTY
    loop {
        // 等待 shell 输出的第一个块
        match rx.recv() {
            Ok(bytes) => process_and_print(&mut processor, &mut term, &bytes),
            Err(_) => {
                // 读线程退出 → shell 已结束
                println!("[term_shell] PTY 已关闭，等待子进程回收");
                break;
            }
        }
        // 排空 channel 中剩余输出块（连续打印，避免阻塞在 stdin 上）
        while let Ok(bytes) = rx.try_recv() {
            process_and_print(&mut processor, &mut term, &bytes);
        }

        // 读 stdin：整行输入写回 PTY
        let mut line = String::new();
        match stdin_lines.read_line(&mut line) {
            Ok(0) => {
                // stdin EOF（Ctrl+D）→ 关闭 PTY writer，通知 shell 退出。
                // 之后继续消费 channel 中剩余的输出块（可能有在途数据），
                // 直到读线程退出（channel 关闭）。
                drop(writer);
                println!("[term_shell] stdin EOF，关闭 PTY，排空剩余输出");
                while let Ok(bytes) = rx.recv() {
                    process_and_print(&mut processor, &mut term, &bytes);
                }
                break;
            }
            Ok(_) => {
                let stripped = line.trim_end_matches(['\n', '\r']);
                writer
                    .write_all(stripped.as_bytes())
                    .and_then(|_| writer.write_all(b"\r\n"))
                    .context("写入 PTY 失败")?;
                writer.flush().ok();
            }
            Err(_) => break,
        }
    }

    // ---- 6. 子进程回收 ----
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

    // 确保读线程结束（不泄漏线程）
    let _ = reader_handle.join();
    println!("[term_shell] 读线程已 join，退出");
    Ok(())
}
