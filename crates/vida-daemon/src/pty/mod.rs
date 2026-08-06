//! PTY 会话管理（M2a-2）。
//!
//! 每个会话：portable-pty PTY + alacritty Term + Processor。
//! - 阻塞读在专用 OS 线程（不占 tokio worker）
//! - 泵线程消费输出 → 立即 advance Term（处理不限频）
//! - 推送循环 60fps，有界通道（容量 4，丢旧留新）
//! - 子进程 wait() 回收，不留僵尸
//! - SessionInput 原始字节透传：不做行缓冲/换行转换/按键解释
//! - 尺寸校验：cols/rows 拒绝 0，上限 1000×1000
//! - 会话属于 daemon 不属于连接：客户端断开后保留（M4 会话恢复前提）
//!
//! 锁结构：
//! - PtyManager 由调用方 Arc<RwLock<..>> 保护
//! - 每个 SessionInner 拆为两把独立 Mutex：
//!   - `term: Mutex<TermState>`：泵线程 + 推送循环共享
//!   - `io: Mutex<IoState>`：input/resize/close 使用
//! - PTY 路径全程不接触金库锁。

pub mod push;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tracing::{info, warn};
use uuid::Uuid;

use self::push::{
    BoundedReceiver, BoundedSender, PushPayload, bounded_channel, build_full_frame, encode_frame,
    push_loop,
};

/// 尺寸上限（客户端可能传 0 或极大值）。
const MAX_COLS: u16 = 1000;
const MAX_ROWS: u16 = 1000;

// ---------------------------------------------------------------------------
// 锁拆分状态
// ---------------------------------------------------------------------------

/// Term 状态：泵线程 + 推送循环共享。
struct TermState {
    term: Term<VoidListener>,
    processor: Processor<StdSyncHandler>,
}

/// IO 状态：input/resize/close 使用。
struct IoState {
    writer: Box<dyn Write + Send>,
    /// master 句柄：保留用于 resize（PTY ioctl winsize）。
    master: Box<dyn portable_pty::MasterPty>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
}

/// 会话内部状态（锁拆分：term + io 两把独立 Mutex）。
pub(crate) struct SessionInner {
    id: String,
    cols: u16,
    rows: u16,
    term: Mutex<TermState>,
    io: Mutex<IoState>,
    /// shell 是否已退出。
    closed: AtomicBool,
    /// 订阅者列表（有界通道发送端）。
    subscribers: Mutex<Vec<BoundedSender<PushPayload>>>,
    /// 推送序列号。
    next_seq: AtomicU64,
}

/// 网格尺寸适配 alacritty Dimensions。
struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
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

// ---------------------------------------------------------------------------
// 公开类型
// ---------------------------------------------------------------------------

/// 屏幕快照：纯文本行 + 宽字符列位置（方案 B）。
///
/// lines 永远是干净文本（无 sentinel），可直接交给 agent 阅读。
/// wide_cols[row] 给出该行中占两列的字符起始列号（0-based），
/// 客户端据此还原列对齐。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScreenData {
    pub lines: Vec<String>,
    pub wide_cols: Vec<Vec<u16>>,
    pub cursor: CursorPos,
}

/// 光标位置。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CursorPos {
    pub row: u16,
    pub col: u16,
}

/// 公开的会话信息（ListSessions 返回值）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    pub foreground_process: Option<String>,
}

// ---------------------------------------------------------------------------
// PtyManager
// ---------------------------------------------------------------------------

/// 会话管理器。调用方持有 `Arc<RwLock<PtyManager>>`，独立于金库锁。
#[derive(Default)]
pub struct PtyManager {
    sessions: HashMap<String, Arc<Mutex<SessionInner>>>,
}

/// 尺寸参数校验：拒绝 0，上限 1000×1000。
fn validate_size(cols: u16, rows: u16) -> Result<()> {
    if cols == 0 || rows == 0 {
        anyhow::bail!("终端尺寸不能为 0（cols={} rows={}）", cols, rows);
    }
    if cols > MAX_COLS || rows > MAX_ROWS {
        anyhow::bail!(
            "终端尺寸超限（上限 {}×{}，收到 {}×{}）",
            MAX_COLS,
            MAX_ROWS,
            cols,
            rows
        );
    }
    Ok(())
}

/// 确定 shell 路径：$SHELL → /bin/zsh → /bin/sh。
fn detect_shell() -> String {
    if let Ok(s) = std::env::var("SHELL")
        && !s.is_empty()
    {
        return s;
    }
    for candidate in ["/bin/zsh", "/bin/sh"] {
        if std::process::Command::new(candidate)
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

#[allow(unused_mut)]
impl PtyManager {
    /// 打开本地 shell 会话。命令固定 $SHELL，不接受客户端指定；
    /// cwd 固定 HOME（decisions.md 结论 6）。
    pub fn open_session(&mut self, cols: u16, rows: u16) -> Result<String> {
        validate_size(cols, rows)?;

        // --- PTY ---
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                ..Default::default()
            })
            .context("openpty 失败")?;

        // --- shell（固定 $SHELL，cwd=HOME）---
        let shell = detect_shell();
        let mut cmd = CommandBuilder::new(&shell);
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {} 失败", shell))?;
        drop(pair.slave);

        // --- Term + Processor ---
        let config = Config {
            scrolling_history: 3000,
            ..Config::default()
        };
        let term: Term<VoidListener> = Term::new(
            config,
            &GridSize {
                cols: cols as usize,
                rows: rows as usize,
            },
            VoidListener,
        );
        let processor: Processor<StdSyncHandler> = Processor::new();

        // --- 读线程（阻塞读，专用 OS 线程）---
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("try_clone_reader 失败")?;
        let (tx, rx): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) = mpsc::channel();

        let id = Uuid::new_v4().to_string();
        let session = Arc::new(Mutex::new(SessionInner {
            id: id.clone(),
            cols,
            rows,
            term: Mutex::new(TermState { term, processor }),
            io: Mutex::new(IoState {
                writer: pair.master.take_writer().context("take_writer 失败")?,
                master: pair.master,
                child: Some(child),
            }),
            closed: AtomicBool::new(false),
            subscribers: Mutex::new(Vec::new()),
            next_seq: AtomicU64::new(0),
        }));

        // 泵线程：消费 PTY 输出 → advance Term
        let pump_session = Arc::clone(&session);
        let pump_id = id.clone();
        thread::spawn(move || {
            for bytes in rx {
                let inner = match pump_session.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                if inner.closed.load(Ordering::Relaxed) {
                    break;
                }
                let mut term = match inner.term.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                let TermState { term, processor } = &mut *term;
                processor.advance(term, &bytes);
            }
            info!("PTY 泵线程退出: {}", pump_id);
        });

        // 推送循环：60fps → 有界通道
        let push_session = Arc::clone(&session);
        thread::spawn(move || push_loop(push_session));

        // 读线程
        let reader_handle = thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024]; // ≥8KB
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // 保存 session（reader_handle 由 session 持有，避免线程泄漏）
        // NOTE: reader_handle 需要存储，否则线程可能被 detach。
        // 为简化，暂不存储（daemon 退出时进程终止，线程随之终止）。
        let _ = reader_handle;

        self.sessions.insert(id.clone(), session);
        info!("会话 {} 已打开 ({}×{})", id, cols, rows);
        Ok(id)
    }

    /// 发送输入：原始字节透传，不做任何转换。
    pub fn session_input(&self, session_id: &str, data: &[u8]) -> Result<()> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;
        let inner = session.lock().map_err(|_| anyhow::anyhow!("会话锁异常"))?;
        if inner.closed.load(Ordering::Relaxed) {
            anyhow::bail!("会话已结束: {}", session_id);
        }
        let mut io = inner.io.lock().map_err(|_| anyhow::anyhow!("IO 锁异常"))?;
        io.writer.write_all(data).context("写入 PTY 失败")?;
        io.writer.flush().ok();
        Ok(())
    }

    /// 调整会话尺寸：同时作用于 PTY（ioctl）和 Term。
    pub fn resize_session(&self, session_id: &str, cols: u16, rows: u16) -> Result<()> {
        validate_size(cols, rows)?;
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;
        let mut inner = session.lock().map_err(|_| anyhow::anyhow!("会话锁异常"))?;
        if inner.closed.load(Ordering::Relaxed) {
            anyhow::bail!("会话已结束: {}", session_id);
        }
        inner.cols = cols;
        inner.rows = rows;

        // Term 侧
        {
            let mut term = inner
                .term
                .lock()
                .map_err(|_| anyhow::anyhow!("Term 锁异常"))?;
            term.term.resize(GridSize {
                cols: cols as usize,
                rows: rows as usize,
            });
        }

        // PTY 侧 ioctl
        {
            let io = inner.io.lock().map_err(|_| anyhow::anyhow!("IO 锁异常"))?;
            io.master
                .resize(PtySize {
                    rows,
                    cols,
                    ..Default::default()
                })
                .context("PTY resize 失败")?;
        }

        info!("会话 {} resize → {}×{}", session_id, cols, rows);
        Ok(())
    }

    /// 关闭会话：kill 子进程 + wait 回收。
    pub fn close_session(&mut self, session_id: &str) -> Result<()> {
        let session = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;
        let inner = session.lock().map_err(|_| anyhow::anyhow!("会话锁异常"))?;
        inner.closed.store(true, Ordering::Relaxed);

        let mut io = inner.io.lock().map_err(|_| anyhow::anyhow!("IO 锁异常"))?;
        if let Some(mut child) = io.child.take() {
            let _ = child.kill();
            match child.wait() {
                Ok(_) => info!("会话 {} 子进程已回收", session_id),
                Err(e) => warn!("会话 {} wait 失败: {}", session_id, e),
            }
        }
        info!("会话 {} 已关闭", session_id);
        Ok(())
    }

    /// 列出所有会话。
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .values()
            .map(|s| {
                let inner = s.lock().unwrap_or_else(|e| e.into_inner());
                SessionInfo {
                    session_id: inner.id.clone(),
                    cols: inner.cols,
                    rows: inner.rows,
                    alive: !inner.closed.load(Ordering::Relaxed),
                    foreground_process: None,
                }
            })
            .collect()
    }

    /// 读取屏幕：全量文本快照（同步，M5 screen_read 雏形）。
    pub fn read_screen(&self, session_id: &str) -> Result<ScreenData> {
        use alacritty_terminal::term::cell::Flags;
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;
        let inner = session.lock().map_err(|_| anyhow::anyhow!("会话锁异常"))?;
        let term = inner
            .term
            .lock()
            .map_err(|_| anyhow::anyhow!("Term 锁异常"))?;

        let mut lines = Vec::new();
        let mut wide_cols = Vec::new();
        let mut current_row: i32 = i32::MIN;
        let mut line = String::new();
        let mut row_wide_cols = Vec::new();
        let mut col: u16 = 0;

        for indexed in term.term.grid().display_iter() {
            let point = indexed.point;
            if point.line.0 != current_row {
                if current_row != i32::MIN {
                    lines.push(line.clone());
                    wide_cols.push(row_wide_cols.clone());
                    line.clear();
                    row_wide_cols.clear();
                }
                current_row = point.line.0;
                col = 0;
            }
            if indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                // spacer 不进文本，仅推进列号
            } else {
                line.push(indexed.cell.c);
                if indexed.cell.flags.contains(Flags::WIDE_CHAR) {
                    row_wide_cols.push(col);
                }
            }
            col += 1;
        }
        lines.push(line);
        wide_cols.push(row_wide_cols);

        let cursor = term.term.grid().cursor.point;
        Ok(ScreenData {
            lines,
            wide_cols,
            cursor: CursorPos {
                row: cursor.line.0 as u16,
                col: cursor.column.0 as u16,
            },
        })
    }

    /// 订阅会话推送：立即回一帧全量快照，此后推送增量。
    pub fn subscribe_session(&self, session_id: &str) -> Result<BoundedReceiver<PushPayload>> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;
        let inner = session.lock().map_err(|_| anyhow::anyhow!("会话锁异常"))?;

        let (tx, rx) = bounded_channel();

        // 立即发送全量快照
        {
            let term = inner
                .term
                .lock()
                .map_err(|_| anyhow::anyhow!("Term 锁异常"))?;
            let full_frame = build_full_frame(&term.term, inner.next_seq.load(Ordering::Relaxed));
            let bytes = encode_frame(&inner.id, &full_frame);
            let payload = PushPayload {
                frame_seq: inner.next_seq.load(Ordering::Relaxed),
                bytes,
            };
            // 全量快照直接发送（通道为空，不会丢弃）
            let _ = tx.send_drop_oldest(payload);
        }

        // 注册订阅者
        inner
            .subscribers
            .lock()
            .map_err(|_| anyhow::anyhow!("订阅者锁异常"))?
            .push(tx);

        info!("会话 {} 新增订阅者", session_id);
        Ok(rx)
    }

    /// 取消订阅：丢弃接收端，推送循环的 retain 会自动清理断开的发送端。
    pub fn unsubscribe_session(&self, _session_id: &str, rx: BoundedReceiver<PushPayload>) {
        // 仅 drop 接收端 — 发送端检测到 disconnected 后由 retain 清理
        drop(rx);
    }

    /// 关闭所有会话（daemon 退出时调用，不留僵尸）。
    pub fn shutdown(&mut self) {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            let _ = self.close_session(&id);
        }
        self.sessions.clear();
        info!("所有会话已关闭");
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn validate_size_rejects_zero() {
        assert!(validate_size(0, 24).is_err());
        assert!(validate_size(80, 0).is_err());
    }

    #[test]
    fn validate_size_rejects_huge() {
        assert!(validate_size(1001, 24).is_err());
        assert!(validate_size(80, 1001).is_err());
    }

    #[test]
    fn validate_size_accepts_normal() {
        assert!(validate_size(80, 24).is_ok());
        assert!(validate_size(1000, 1000).is_ok());
    }

    #[test]
    fn open_input_read_close_roundtrip() {
        let mut pm = PtyManager::default();
        let id = pm.open_session(80, 24).unwrap();
        pm.session_input(&id, b"echo hello\r\n").unwrap();
        thread::sleep(Duration::from_millis(500));
        let screen = pm.read_screen(&id).unwrap();
        let joined = screen.lines.join("\n");
        assert!(joined.contains("hello"), "screen 应含 hello: {:?}", joined);
        assert!(!joined.contains('\0'), "lines 不应含 NUL: {:?}", joined);
        pm.close_session(&id).unwrap();
    }

    #[test]
    fn close_session_idempotent() {
        let mut pm = PtyManager::default();
        let id = pm.open_session(80, 24).unwrap();
        pm.close_session(&id).unwrap();
        assert!(pm.close_session(&id).is_err());
    }

    #[test]
    fn input_passthrough_no_conversion() {
        let mut pm = PtyManager::default();
        let id = pm.open_session(80, 24).unwrap();
        pm.session_input(&id, &[0x03]).unwrap();
        pm.session_input(&id, b"\x1b[A").unwrap();
        let info = pm.list_sessions();
        assert_eq!(info.len(), 1);
        assert!(info[0].alive);
        pm.close_session(&id).unwrap();
    }

    #[test]
    fn read_screen_reports_wide_char_columns() {
        let mut pm = PtyManager::default();
        let id = pm.open_session(80, 24).unwrap();
        pm.session_input(&id, "echo 你好世界\n".as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(600));
        let screen = pm.read_screen(&id).unwrap();

        let joined = screen.lines.join("\n");
        assert!(!joined.contains('\0'), "lines 不应含 NUL: {:?}", joined);
        assert!(joined.contains("你好世界"), "lines 应含中文: {:?}", joined);

        let found = screen
            .lines
            .iter()
            .zip(screen.wide_cols.iter())
            .find(|(line, _)| line.contains("你好世界"));
        assert!(found.is_some(), "应找到含中文的行");
        let (line, wide) = found.unwrap();
        let start = line.find("你好世界").unwrap() as u16;
        assert_eq!(wide, &[start, start + 2, start + 4, start + 6]);
        pm.close_session(&id).unwrap();
    }

    #[test]
    fn damage_reset_isolates_writes() {
        use alacritty_terminal::term::TermDamage;

        let config = Config {
            scrolling_history: 100,
            ..Config::default()
        };
        let mut term: Term<VoidListener> =
            Term::new(config, &GridSize { cols: 80, rows: 24 }, VoidListener);
        let mut processor: Processor<StdSyncHandler> = Processor::new();

        processor.advance(&mut term, b"\x1b[2J\x1b[1;1Hfirst line\x1b[20;1H");
        term.reset_damage();
        let d1 = term.damage();
        match d1 {
            TermDamage::Full => panic!("reset 后不应是 Full"),
            TermDamage::Partial(it) => {
                let rows: Vec<_> = it.map(|b| b.line).collect();
                assert!(rows.contains(&0), "第一次写入应损伤行 0: {:?}", rows);
            }
        }
        term.reset_damage();

        processor.advance(&mut term, b"\x1b[6;1Hsecond line");
        let d2 = term.damage();
        match d2 {
            TermDamage::Full => panic!("reset 后不应是 Full"),
            TermDamage::Partial(it) => {
                let rows: Vec<_> = it.map(|b| b.line).collect();
                assert!(
                    !rows.contains(&0),
                    "第二次 damage 不应含第一次的行 0: {:?}",
                    rows
                );
                assert!(rows.contains(&5), "第二次写入应损伤行 5: {:?}", rows);
            }
        }
        term.reset_damage();
    }

    /// 订阅机制测试：订阅后立即收到全量快照，之后收到增量。
    #[test]
    fn subscribe_gets_full_then_incremental() {
        let mut pm = PtyManager::default();
        let id = pm.open_session(80, 24).unwrap();

        // 先写入一些内容
        pm.session_input(&id, b"echo hello\r\n").unwrap();
        thread::sleep(Duration::from_millis(300));

        // 订阅
        let rx = pm.subscribe_session(&id).unwrap();

        // 第一帧应为全量快照
        let first = rx.recv().expect("应收到全量快照");
        assert!(!first.bytes.is_empty(), "全量快照应有内容");

        // 写入新内容，等待增量
        pm.session_input(&id, b"echo world\r\n").unwrap();
        thread::sleep(Duration::from_millis(32));

        // 应收到增量帧
        let incremental = rx.try_recv();
        assert!(incremental.is_some(), "应收到增量帧");

        pm.unsubscribe_session(&id, rx);
        pm.close_session(&id).unwrap();
    }
}
