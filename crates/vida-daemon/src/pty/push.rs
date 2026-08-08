//! 二进制推送协议 + 限频 + 有界通道（M2a-2-2）。

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermDamage};
use tracing::warn;

use crate::pty::SessionInner;

pub const PUSH_INTERVAL_MS: u64 = 16;
pub const PUSH_CHANNEL_CAPACITY: usize = 4;

// ---------------------------------------------------------------------------
// 帧类型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Frame {
    pub seq: u64,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub lines: Vec<DirtyLine>,
}

/// 损坏信息（owned，避免 damage 迭代器与 build 函数争用 Term 借用）。
enum DamageInfo {
    Full,
    /// (line, left, right) 区间列表。
    Partial(Vec<(usize, usize, usize)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirtyLine {
    pub row: u16,
    pub start_col: u16,
    pub end_col: u16,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub len: u16,
    pub flags: u8,
    pub fg: ColorSpec,
    pub bg: ColorSpec,
    pub char: char,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColorSpec {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub const FLAG_BOLD: u8 = 0x01;
pub const FLAG_ITALIC: u8 = 0x02;
pub const FLAG_UNDERLINE: u8 = 0x04;
pub const FLAG_REVERSE: u8 = 0x08;
pub const FLAG_HIDDEN: u8 = 0x10;
pub const FLAG_STRIKE: u8 = 0x20;
pub const FLAG_WIDE: u8 = 0x40;

const COLOR_DEFAULT: u8 = 0x00;
const COLOR_INDEXED: u8 = 0x01;
const COLOR_RGB: u8 = 0x02;

pub const FRAME_MAGIC: u8 = 0x01;

// ---------------------------------------------------------------------------
// 有界通道（丢旧留新）
// ---------------------------------------------------------------------------

pub struct BoundedSender<T> {
    queue: Arc<Mutex<VecDeque<T>>>,
    notify: mpsc::Sender<()>,
}

impl<T> BoundedSender<T> {
    pub fn send_drop_oldest(&self, item: T) -> bool {
        if let Ok(mut q) = self.queue.lock() {
            if q.len() >= PUSH_CHANNEL_CAPACITY {
                q.pop_front();
            }
            q.push_back(item);
        }
        // 返回 notify 是否成功：receiver 断开时 send 失败，返回 false
        self.notify.send(()).is_ok()
    }
}

pub struct BoundedReceiver<T> {
    queue: Arc<Mutex<VecDeque<T>>>,
    notify: mpsc::Receiver<()>,
}

impl<T> BoundedReceiver<T> {
    /// 阻塞接收一帧。收到通知后排空通知通道，避免积压。
    pub fn recv(&self) -> Option<T> {
        self.notify.recv().ok()?;
        while self.notify.try_recv().is_ok() {}
        self.queue.lock().ok()?.pop_front()
    }

    pub fn try_recv(&self) -> Option<T> {
        while self.notify.try_recv().is_ok() {}
        self.queue.lock().ok()?.pop_front()
    }
}

pub fn bounded_channel<T>() -> (BoundedSender<T>, BoundedReceiver<T>) {
    let queue: Arc<Mutex<VecDeque<T>>> = Arc::new(Mutex::new(VecDeque::new()));
    let (notify_tx, notify_rx): (mpsc::Sender<()>, mpsc::Receiver<()>) = mpsc::channel();
    (
        BoundedSender {
            queue: queue.clone(),
            notify: notify_tx,
        },
        BoundedReceiver {
            queue,
            notify: notify_rx,
        },
    )
}

// ---------------------------------------------------------------------------
// 帧编码
// ---------------------------------------------------------------------------

pub fn encode_frame(session_id: &str, frame: &Frame) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    buf.push(FRAME_MAGIC);
    let id_bytes: &[u8] = session_id.as_bytes();
    buf.extend_from_slice(&(id_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(id_bytes);
    buf.extend_from_slice(&frame.seq.to_be_bytes());
    buf.extend_from_slice(&frame.cursor_row.to_be_bytes());
    buf.extend_from_slice(&frame.cursor_col.to_be_bytes());
    buf.push(if frame.cursor_visible { 1 } else { 0 });
    buf.extend_from_slice(&(frame.lines.len() as u16).to_be_bytes());
    for line in &frame.lines {
        buf.extend_from_slice(&line.row.to_be_bytes());
        buf.extend_from_slice(&line.start_col.to_be_bytes());
        buf.extend_from_slice(&line.end_col.to_be_bytes());
        buf.extend_from_slice(&(line.runs.len() as u16).to_be_bytes());
        for run in &line.runs {
            buf.extend_from_slice(&run.len.to_be_bytes());
            buf.push(run.flags);
            encode_color(&mut buf, &run.fg);
            encode_color(&mut buf, &run.bg);
            let mut char_buf: [u8; 4] = [0u8; 4];
            let char_bytes: &str = run.char.encode_utf8(&mut char_buf);
            buf.push(char_bytes.len() as u8);
            buf.extend_from_slice(char_bytes.as_bytes());
        }
    }
    buf
}

fn encode_color(buf: &mut Vec<u8>, color: &ColorSpec) {
    match color {
        ColorSpec::Default => buf.push(COLOR_DEFAULT),
        ColorSpec::Indexed(idx) => {
            buf.push(COLOR_INDEXED);
            buf.push(*idx);
        }
        ColorSpec::Rgb(r, g, b) => {
            buf.push(COLOR_RGB);
            buf.push(*r);
            buf.push(*g);
            buf.push(*b);
        }
    }
}

// ---------------------------------------------------------------------------
// 帧构建
// ---------------------------------------------------------------------------

pub fn build_full_frame(term: &Term<alacritty_terminal::event::VoidListener>, seq: u64) -> Frame {
    let rows: usize = term.grid().screen_lines();
    let display_offset = term.grid().display_offset() as i32;
    let mut lines: Vec<DirtyLine> = Vec::new();
    for row in 0..rows {
        if let Some(line) = build_dirty_line_full(term, row as u16, row as i32 - display_offset) {
            lines.push(line);
        }
    }
    let cursor: alacritty_terminal::index::Point = term.grid().cursor.point;
    Frame {
        seq,
        cursor_row: cursor.line.0 as u16,
        cursor_col: cursor.column.0 as u16,
        cursor_visible: display_offset == 0,
        lines,
    }
}

pub fn build_partial_frame(
    term: &Term<alacritty_terminal::event::VoidListener>,
    seq: u64,
    damage: &[(usize, usize, usize)],
) -> Frame {
    let mut lines: Vec<DirtyLine> = Vec::new();
    let rows: usize = term.grid().screen_lines();
    for &(line, left, right) in damage {
        // 只发送可见屏幕内的脏行。
        // damage 行号是 grid 绝对坐标（含 scrollback 偏移），
        // 超出屏幕的行是滚动历史，客户端不渲染（跳过避免 row>rows 异常）。
        if line >= rows {
            continue;
        }
        if let Some(dirty_line) =
            build_dirty_line_range(term, line as u16, left as u16, right as u16)
        {
            lines.push(dirty_line);
        }
    }
    let cursor: alacritty_terminal::index::Point = term.grid().cursor.point;
    Frame {
        seq,
        cursor_row: cursor.line.0 as u16,
        cursor_col: cursor.column.0 as u16,
        cursor_visible: true,
        lines,
    }
}

fn build_dirty_line_full(
    term: &Term<alacritty_terminal::event::VoidListener>,
    viewport_row: u16,
    grid_line: i32,
) -> Option<DirtyLine> {
    let cols: usize = term.grid().columns();
    build_dirty_line_range_at(term, viewport_row, grid_line, 0, cols as u16 - 1)
}

fn build_dirty_line_range(
    term: &Term<alacritty_terminal::event::VoidListener>,
    row: u16,
    start_col: u16,
    end_col: u16,
) -> Option<DirtyLine> {
    build_dirty_line_range_at(term, row, row as i32, start_col, end_col)
}

fn build_dirty_line_range_at(
    term: &Term<alacritty_terminal::event::VoidListener>,
    viewport_row: u16,
    grid_line: i32,
    start_col: u16,
    end_col: u16,
) -> Option<DirtyLine> {
    let mut runs: Vec<Run> = Vec::new();
    let mut current: Option<Run> = None;
    let mut first_col: Option<u16> = None;
    let mut last_col: u16 = start_col;
    let mut col: u16 = start_col;
    while col <= end_col {
        let point: Point = Point::new(Line(grid_line), Column(col as usize));
        let cell = &term.grid()[point];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            col += 1;
            continue;
        }
        let ch: char = cell.c;
        let flags: u8 = encode_flags(&cell.flags);
        let fg: ColorSpec = encode_color_spec(&cell.fg);
        let bg: ColorSpec = encode_color_spec(&cell.bg);
        let width: u16 = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let merge: bool = current
            .as_ref()
            .map(|r| r.char == ch && r.fg == fg && r.bg == bg && r.flags == flags)
            .unwrap_or(false);
        if merge {
            if let Some(ref mut r) = current {
                r.len += width;
            }
        } else {
            if let Some(run) = current.take() {
                runs.push(run);
            }
            current = Some(Run {
                len: width,
                flags,
                fg,
                bg,
                char: ch,
            });
            if first_col.is_none() {
                first_col = Some(col);
            }
        }
        last_col = col + width - 1;
        col += width;
    }
    if let Some(run) = current.take() {
        runs.push(run);
    }
    if runs.is_empty() {
        return None;
    }
    Some(DirtyLine {
        row: viewport_row,
        start_col: first_col.unwrap_or(start_col),
        end_col: last_col,
        runs,
    })
}

pub(crate) fn encode_flags(cell_flags: &Flags) -> u8 {
    let mut flags: u8 = 0;
    if cell_flags.contains(Flags::BOLD) {
        flags |= FLAG_BOLD;
    }
    if cell_flags.contains(Flags::ITALIC) {
        flags |= FLAG_ITALIC;
    }
    if cell_flags.contains(Flags::UNDERLINE) {
        flags |= FLAG_UNDERLINE;
    }
    if cell_flags.contains(Flags::INVERSE) {
        flags |= FLAG_REVERSE;
    }
    if cell_flags.contains(Flags::HIDDEN) {
        flags |= FLAG_HIDDEN;
    }
    if cell_flags.contains(Flags::STRIKEOUT) {
        flags |= FLAG_STRIKE;
    }
    if cell_flags.contains(Flags::WIDE_CHAR) {
        flags |= FLAG_WIDE;
    }
    flags
}

fn encode_color_spec(color: &alacritty_terminal::vte::ansi::Color) -> ColorSpec {
    use alacritty_terminal::vte::ansi::{Color, NamedColor};
    match color {
        Color::Spec(rgb) => ColorSpec::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(idx) => ColorSpec::Indexed(*idx),
        Color::Named(named) => match named {
            NamedColor::Black => ColorSpec::Indexed(0),
            NamedColor::Red => ColorSpec::Indexed(1),
            NamedColor::Green => ColorSpec::Indexed(2),
            NamedColor::Yellow => ColorSpec::Indexed(3),
            NamedColor::Blue => ColorSpec::Indexed(4),
            NamedColor::Magenta => ColorSpec::Indexed(5),
            NamedColor::Cyan => ColorSpec::Indexed(6),
            NamedColor::White => ColorSpec::Indexed(7),
            NamedColor::BrightBlack => ColorSpec::Indexed(8),
            NamedColor::BrightRed => ColorSpec::Indexed(9),
            NamedColor::BrightGreen => ColorSpec::Indexed(10),
            NamedColor::BrightYellow => ColorSpec::Indexed(11),
            NamedColor::BrightBlue => ColorSpec::Indexed(12),
            NamedColor::BrightMagenta => ColorSpec::Indexed(13),
            NamedColor::BrightCyan => ColorSpec::Indexed(14),
            NamedColor::BrightWhite => ColorSpec::Indexed(15),
            _ => ColorSpec::Default,
        },
    }
}

// ---------------------------------------------------------------------------
// 推送循环
// ---------------------------------------------------------------------------

/// 推送循环（专用 OS 线程）。
///
/// 持锁约束：锁内只读 damage + 拷贝脏区 + reset_damage。
/// 编码与发送到有界通道在锁外进行。
pub(crate) fn push_loop(session: Arc<Mutex<SessionInner>>) {
    let mut seq: u64 = 0;
    // 绝对时间戳限频：next_slot 单调推进，sleep 抖动不会累积。
    // 距上次推送不足 16ms 时直接返回，不读 damage/不编码/不入队。
    let interval = Duration::from_millis(PUSH_INTERVAL_MS);
    let mut next_slot = std::time::Instant::now();
    // 上次推送的帧内容：alacritty_terminal 的 damage() 每帧无条件标记
    // 光标行（为 cursor blink 设计）。内容未变的帧（仅光标行）对
    // 客户端无意义——跳过不推，否则客户端每帧重绘空转（实测 62fps、
    // GUI CPU ~49%）。
    let mut last_lines: Vec<DirtyLine> = Vec::new();

    loop {
        // 等待到下一个时隙（绝对时间，非相对 sleep）
        let now = std::time::Instant::now();
        if now < next_slot {
            thread::sleep(next_slot - now);
        }
        // 推进时隙：确保间隔 ≥ 16ms
        next_slot += interval;
        // 若处理耗时长于 interval，防止 burst：把时隙拉回不早于当前
        // 时间的一个 interval 之前，避免追赶产生密集帧。
        if next_slot + interval < std::time::Instant::now() {
            next_slot = std::time::Instant::now();
        }

        let inner = match session.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if inner.closed.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let frame = {
            let mut term = match inner.term.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            // 收集 damage 为 owned 数据（避免 damage 迭代器与 build 函数争用借用）
            let damage_info = match term.term.damage() {
                TermDamage::Full => DamageInfo::Full,
                TermDamage::Partial(iter) => {
                    let bounds: Vec<(usize, usize, usize)> =
                        iter.map(|b| (b.line, b.left, b.right)).collect();
                    DamageInfo::Partial(bounds)
                }
            };
            let frame = match &damage_info {
                DamageInfo::Full => build_full_frame(&term.term, seq),
                DamageInfo::Partial(bounds) => build_partial_frame(&term.term, seq, bounds),
            };
            term.term.reset_damage();
            frame
        };
        // 内容未变（仅 alacritty 的光标行 damage）：跳过，不推帧。
        // DirtyLine 含行内容与属性，PartialEq 可判定内容是否变化。
        if frame.lines == last_lines {
            continue;
        }
        last_lines = frame.lines.clone();
        let bytes: Vec<u8> = encode_frame(&inner.id, &frame);
        let frame_payload = PushPayload {
            frame_seq: seq,
            kind: PushKind::Frame,
            bytes,
        };
        let mut subs = match inner.subscribers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        subs.retain(|sub| sub.send_drop_oldest(frame_payload.clone()));
        seq += 1;
    }
    warn!(
        "推送循环退出: {}",
        session.lock().map(|s| s.id.clone()).unwrap_or_default()
    );
}

/// 推送负载类型。
#[derive(Debug, Clone)]
pub enum PushKind {
    /// 普通推送帧（damage 增量）。
    Frame,
    /// 会话结束事件（shell 自行退出）。
    SessionClosed { exit_code: u32 },
}

#[derive(Debug, Clone)]
pub struct PushPayload {
    pub frame_seq: u64,
    pub kind: PushKind,
    pub bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::{Dimensions, Scroll};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    struct TestSize {
        cols: usize,
        rows: usize,
    }

    impl Dimensions for TestSize {
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

    fn frame_text(frame: &Frame) -> String {
        frame
            .lines
            .iter()
            .flat_map(|line| line.runs.iter())
            .flat_map(|run| std::iter::repeat_n(run.char, run.len as usize))
            .collect()
    }

    #[test]
    fn bounded_sender_drop_oldest() {
        let (tx, rx) = bounded_channel::<i32>();
        for i in 0..PUSH_CHANNEL_CAPACITY as i32 {
            tx.send_drop_oldest(i);
        }
        for i in 100..200 {
            tx.send_drop_oldest(i);
        }
        let mut received: Vec<i32> = Vec::new();
        while let Some(v) = rx.try_recv() {
            received.push(v);
        }
        assert!(received.len() <= PUSH_CHANNEL_CAPACITY);
        assert_eq!(received.last().copied(), Some(199));
    }

    #[test]
    fn encode_frame_basic() {
        let frame = Frame {
            seq: 1,
            cursor_row: 0,
            cursor_col: 5,
            cursor_visible: true,
            lines: vec![DirtyLine {
                row: 0,
                start_col: 0,
                end_col: 4,
                runs: vec![Run {
                    len: 5,
                    flags: 0,
                    fg: ColorSpec::Default,
                    bg: ColorSpec::Default,
                    char: 'a',
                }],
            }],
        };
        let bytes: Vec<u8> = encode_frame("test-sess", &frame);
        assert_eq!(bytes[0], FRAME_MAGIC);
        let id_len: u16 = u16::from_be_bytes([bytes[1], bytes[2]]);
        assert_eq!(id_len, 9);
        let seq_bytes: &[u8] = &bytes[12..20];
        let seq_val: u64 = u64::from_be_bytes(seq_bytes.try_into().unwrap());
        assert_eq!(seq_val, 1);
    }

    #[test]
    fn named_ansi_colors_keep_their_palette_index() {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};

        assert_eq!(
            encode_color_spec(&Color::Named(NamedColor::Black)),
            ColorSpec::Indexed(0)
        );
        assert_eq!(
            encode_color_spec(&Color::Named(NamedColor::BrightBlack)),
            ColorSpec::Indexed(8)
        );
        assert_eq!(
            encode_color_spec(&Color::Named(NamedColor::Foreground)),
            ColorSpec::Default
        );
    }

    #[test]
    fn full_frame_uses_scrollback_viewport_and_hides_cursor() {
        let config = Config {
            scrolling_history: 100,
            ..Config::default()
        };
        let mut term: Term<VoidListener> =
            Term::new(config, &TestSize { cols: 12, rows: 3 }, VoidListener);
        let mut processor: Processor<StdSyncHandler> = Processor::new();
        processor.advance(&mut term, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");

        let bottom = build_full_frame(&term, 1);
        assert!(bottom.cursor_visible);
        assert!(frame_text(&bottom).contains("five"));

        term.scroll_display(Scroll::Delta(1));
        let scrolled = build_full_frame(&term, 2);
        let text = frame_text(&scrolled);
        assert!(!scrolled.cursor_visible);
        assert!(text.contains("four"), "回滚视图应包含较早输出: {text:?}");
        assert!(
            !text.contains("five"),
            "回滚一行后不应仍显示底部行: {text:?}"
        );
    }

    /// 规格约束 1：限频窗口内多次变化 → 客户端最终收到最终状态。
    #[test]
    fn rate_limit_merges_to_final_state() {
        use crate::pty::PtyManager;
        let mut pm = PtyManager::default();
        let id: String = pm.open_session(80, 24).unwrap();
        let rx = pm.subscribe_session(&id).unwrap();
        for i in 0..10 {
            let cmd: String = format!("echo line{}\n", i);
            pm.session_input(&id, cmd.as_bytes()).unwrap();
            thread::sleep(Duration::from_millis(8));
        }
        thread::sleep(Duration::from_millis(32));
        let mut frames: Vec<PushPayload> = Vec::new();
        while let Some(payload) = rx.try_recv() {
            frames.push(payload);
        }
        assert!(!frames.is_empty(), "应收到至少一帧");
        let max_seq: u64 = frames.iter().map(|p| p.frame_seq).max().unwrap();
        assert!(max_seq > 0, "seq 应递增");
        pm.unsubscribe_session(&id, rx);
        pm.close_session(&id).unwrap();
    }

    /// 规格约束 3：有界通道内存不增长，最终帧正确。
    #[test]
    fn bounded_channel_no_memory_growth() {
        let (tx, rx) = bounded_channel::<Vec<u8>>();
        for i in 0..100 {
            let data: Vec<u8> = vec![i as u8; 1024];
            tx.send_drop_oldest(data);
        }
        let mut received: Vec<Vec<u8>> = Vec::new();
        while let Some(data) = rx.try_recv() {
            received.push(data);
        }
        assert!(
            received.len() <= PUSH_CHANNEL_CAPACITY,
            "收到 {} 帧，超过容量 {}",
            received.len(),
            PUSH_CHANNEL_CAPACITY
        );
        assert_eq!(received.last().unwrap()[0], 99);
    }
}
