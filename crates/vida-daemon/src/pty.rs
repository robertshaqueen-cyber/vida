//! PTY 会话管理（M2a-2）。
//!
//! 每个会话：portable-pty PTY + alacritty Term + Processor。
//! - 阻塞读在专用 OS 线程（不占 tokio worker）
//! - 泵线程消费输出 → 立即 advance Term（处理不限频，推送限频在 M2a-2-2）
//! - 子进程 wait() 回收，不留僵尸
//! - SessionInput 原始字节透传：不做行缓冲/换行转换/按键解释
//! - 尺寸校验：cols/rows 拒绝 0，上限 1000×1000
//! - 会话属于 daemon 不属于连接：客户端断开后保留（M4 会话恢复前提）
//!
//! 锁结构：PtyManager 由调用方 Arc<RwLock<..>> 保护；每个 Session
//! 内部另有 std::sync::Mutex（瞬时锁，无 await 期间持有），
//! PTY 路径全程不接触金库锁。

use std::collections::HashMap;
use std::io::{Read, Write};
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

/// 尺寸上限（客户端可能传 0 或极大值）。
const MAX_COLS: u16 = 1000;
const MAX_ROWS: u16 = 1000;

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

struct SessionInner {
    id: String,
    cols: u16,
    rows: u16,
    term: Term<VoidListener>,
    processor: Processor<StdSyncHandler>,
    writer: Box<dyn Write + Send>,
    /// master 句柄：保留用于 resize（PTY ioctl winsize）。
    master: Box<dyn portable_pty::MasterPty>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// shell 是否已退出（泵线程检测到 channel 关闭时置位）。
    closed: bool,
}

impl SessionInner {
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.cols = cols;
        self.rows = rows;
        // Term 侧
        self.term.resize(GridSize {
            cols: cols as usize,
            rows: rows as usize,
        });
        // PTY 侧 ioctl（winsize）
        self.master
            .resize(PtySize {
                rows,
                cols,
                ..Default::default()
            })
            .context("PTY resize 失败")?;
        Ok(())
    }
}

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

impl PtyManager {
    /// 打开本地 shell 会话。命令固定 $SHELL，不接受客户端指定；
    /// cwd 固定 HOME（decisions.md 结论 6）。
    pub fn open_session(&mut self, cols: u16, rows: u16) -> Result<String> {
        validate_size(cols, rows)?;

        // --- PTY（每次 open 创建新的 PtySystem；dyn PtySystem 非 Sync，
        //    不能跨线程存储）---
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
        // cwd 固定 HOME（portable-pty 默认即 HOME）
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {} 失败", shell))?;
        drop(pair.slave);

        // --- Term + Processor ---
        let config = Config {
            scrolling_history: 3000, // 金库默认（M2a-2 后续从 Settings 读）
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
            info!("PTY 读线程退出（EOF/关闭）");
        });

        let id = Uuid::new_v4().to_string();
        let session = Arc::new(Mutex::new(SessionInner {
            id: id.clone(),
            cols,
            rows,
            term,
            processor,
            writer: pair.master.take_writer().context("take_writer 失败")?,
            master: pair.master,
            child: Some(child),
            closed: false,
        }));

        // --- 泵线程：消费输出 → 立即 advance Term ---
        let pump_session = Arc::clone(&session);
        thread::spawn(move || pump_loop(pump_session, rx, reader_handle));

        self.sessions.insert(id.clone(), session);
        info!("会话 {} 已打开 ({}×{})", id, cols, rows);
        Ok(id)
    }

    /// 发送输入：原始字节透传，不做任何转换。
    pub fn session_input(&mut self, session_id: &str, data: &[u8]) -> Result<()> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;
        let mut inner = session.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            anyhow::bail!("会话已结束: {}", session_id);
        }
        inner.writer.write_all(data).context("写入 PTY 失败")?;
        inner.writer.flush().ok();
        Ok(())
    }

    /// 调整会话尺寸：同时作用于 PTY（ioctl）和 Term。
    pub fn resize_session(&mut self, session_id: &str, cols: u16, rows: u16) -> Result<()> {
        validate_size(cols, rows)?;
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;
        let mut inner = session.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            anyhow::bail!("会话已结束: {}", session_id);
        }
        inner.resize(cols, rows)?;
        info!("会话 {} resize → {}×{}", session_id, cols, rows);
        Ok(())
    }

    /// 关闭会话：kill 子进程 + wait 回收。
    pub fn close_session(&mut self, session_id: &str) -> Result<()> {
        let session = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;
        let mut inner = session.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = inner.child.take() {
            let _ = child.kill();
            match child.wait() {
                Ok(_) => info!("会话 {} 子进程已回收", session_id),
                Err(e) => warn!("会话 {} wait 失败: {}", session_id, e),
            }
        }
        inner.closed = true;
        // 泵线程：channel 关闭后自行退出（读线程 EOF）
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
                    alive: !inner.closed,
                    // 进程名探测（M2a-2 先返回 None，M3 再做进程树查询）
                    foreground_process: None,
                }
            })
            .collect()
    }

    /// 读取屏幕：全量文本快照（同步，M5 screen_read 雏形）。
    ///
    /// 宽字符处理（方案 B）：lines 只含干净文本（无 sentinel）；
    /// wide_cols[row] 给出该行中占两列的字符起始列号（0-based）。
    /// 客户端按 wide_cols 还原列对齐，文本可直接阅读。
    pub fn read_screen(&self, session_id: &str) -> Result<ScreenData> {
        use alacritty_terminal::term::cell::Flags;
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;
        let inner = session.lock().unwrap_or_else(|e| e.into_inner());
        let mut lines = Vec::new();
        let mut wide_cols = Vec::new();
        let mut current_row: i32 = i32::MIN;
        let mut line = String::new();
        let mut row_wide_cols = Vec::new();
        let mut col: u16 = 0;
        for indexed in inner.term.grid().display_iter() {
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
                // spacer 位：不进文本，仅推进列号
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
        let cursor = inner.term.grid().cursor.point;
        Ok(ScreenData {
            lines,
            wide_cols,
            cursor: CursorPos {
                row: cursor.line.0 as u16,
                col: cursor.column.0 as u16,
            },
        })
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
// 泵线程
// ---------------------------------------------------------------------------

/// 消费 PTY 输出：收到字节立即 advance Term（处理不限频）。
/// 推送限频在 M2a-2-2 加入（damage → 有界通道）。
///
// ── M2a-2-2 推送路径持锁约束（预先约定）──
//
// 推送路径在 term 锁下只做三件事（持锁时间最小化）：
//   1. 读 damage → 确定脏行区间
//   2. 把脏区的 cell 拷贝进临时结构 Vec<(row, start, end, Vec<Cell>)>
//   3. reset_damage
// 然后立即释放锁。RLE 编码与二进制序列化在锁外进行。
//
// 理由：编码是路径上最耗时的一步，且不需要访问 Term。持锁编码
// 会在 yes 场景下持续阻塞 session_input 和 read_screen。
//
// 结构拆分方案（必改 2）：SessionInner 拆为两把独立 Mutex：
//   - `term: Mutex<(Term, Processor)>`：泵线程独占，推送循环只读
//   - `io: Mutex<IoState>{writer, master, child}`：input/resize/close
// 泵持 term 锁，输入持 io 锁 → 互不阻塞。
///
// ── 持锁分析（必改 2）──
//
// 当前：整个 `processor.advance(term, &bytes)` 期间持有
// SessionInner 的 Mutex。单次 advance 通常 <1ms，但 `yes` 类
// 持续高吞吐场景下泵线程几乎持续持锁。
//
// 影响：
//   a. advance 期间持锁，直到返回才释放。
//   b. read_screen / session_input / resize 锁同一把 Mutex →
//      高吞吐时阻塞，响应延迟不可控。
//   c. M2a-2-2 推送循环需在同一把锁下读 damage + 遍历 grid，
//      持锁时间随列数×行数增加。
//
// M2a-2-2 改进方案：SessionInner 拆为两把独立 Mutex：
//   - `term: Mutex<(Term, Processor)>`：泵线程独占，推送循环只读。
//   - `io: Mutex<IoState>{writer, master, child}`：input/resize/close。
//   泵持 term 锁，输入持 io 锁 → 互不阻塞。read_screen 共享
//   term 锁（低频，推送 60fps 限频可控）。
fn pump_loop(
    session: Arc<Mutex<SessionInner>>,
    rx: mpsc::Receiver<Vec<u8>>,
    reader_handle: thread::JoinHandle<()>,
) {
    for bytes in rx {
        let mut inner = session.lock().unwrap_or_else(|e| e.into_inner());
        let SessionInner {
            term, processor, ..
        } = &mut *inner;
        processor.advance(term, &bytes);
    }
    // 读线程已 EOF：回收句柄
    let _ = reader_handle.join();
    // 标记会话结束
    let mut inner = session.lock().unwrap_or_else(|e| e.into_inner());
    inner.closed = true;
    info!("会话 {} 泵线程退出（shell 已结束）", inner.id);
}

// ---------------------------------------------------------------------------
// shell 探测
// ---------------------------------------------------------------------------

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
        // 等 shell 输出
        std::thread::sleep(Duration::from_millis(500));
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
        // 重复关闭 → 错误而非 panic
        assert!(pm.close_session(&id).is_err());
    }

    #[test]
    fn input_passthrough_no_conversion() {
        let mut pm = PtyManager::default();
        let id = pm.open_session(80, 24).unwrap();
        // 0x03 (Ctrl+C) 和 ESC 序列原样透传（无行处理）
        pm.session_input(&id, &[0x03]).unwrap();
        pm.session_input(&id, b"\x1b[A").unwrap();
        // 无 panic、无转换，会话仍存活
        let info = pm.list_sessions();
        assert_eq!(info.len(), 1);
        assert!(info[0].alive);
        pm.close_session(&id).unwrap();
    }

    /// 必改 3：read_screen 保留宽字符列位置（方案 B：wide_cols）。
    /// 中文「你好世界」每个字占两列，wide_cols 记录起始列号。
    #[test]
    fn read_screen_reports_wide_char_columns() {
        let mut pm = PtyManager::default();
        let id = pm.open_session(80, 24).unwrap();
        // 写入中文 + 换行
        pm.session_input(&id, "echo 你好世界\n".as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        let screen = pm.read_screen(&id).unwrap();

        // lines 是干净文本（无 NUL sentinel）
        let joined = screen.lines.join("\n");
        assert!(!joined.contains('\0'), "lines 不应含 NUL: {:?}", joined);
        assert!(joined.contains("你好世界"), "lines 应含中文: {:?}", joined);

        // wide_cols：找到含「你好世界」的行，验证列位置
        // 「你」占 col 0-1, 「好」占 2-3, 「世」占 4-5, 「界」占 6-7
        let found = screen
            .lines
            .iter()
            .zip(screen.wide_cols.iter())
            .find(|(line, _)| line.contains("你好世界"));
        assert!(found.is_some(), "应找到含中文的行");
        let (line, wide) = found.unwrap();
        let start = line.find("你好世界").unwrap() as u16;
        assert_eq!(
            wide,
            &[start, start + 2, start + 4, start + 6],
            "宽字符起始列号应为 0,2,4,6 (relative to line start)"
        );
        pm.close_session(&id).unwrap();
    }

    /// 规格约束 2：damage 连续两次写入之间 reset，
    /// 第二次 damage 不含第一次的行。
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

        // 第一次写入第 0 行，然后把光标移走（避免 damage 的
        // previous_cursor 标记干扰断言），再 reset
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

        // 第二次写入第 5 行（光标从行 20 移来，previous_cursor 在行 20）
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
}
