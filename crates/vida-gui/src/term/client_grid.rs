//! 客户端 grid 模型（M2b-1 规格 4.1）。
//!
//! GUI 只持有可见屏幕，不持有 scrollback。内存预算：
//! 200 × 50 × ~16 字节 ≈ 160 KB/标签。

/// 颜色编码（与 daemon `ColorSpec` 对应）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorSpec {
    /// 终端默认前景/背景色（渲染时用主题色）。
    Default,
    /// 256 色索引。
    Indexed(u8),
    /// 24 位真彩色。
    Rgb(u8, u8, u8),
}

/// 客户端内部标志位（叠加在协议 flags 之上）。
pub mod cell_flags {
    /// 宽字符占两列中的第二列（客户端填充，渲染跳过字形）。
    pub const WIDE_SPACER: u8 = 0x80;
}

/// 单个可见 cell。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClientCell {
    pub ch: char,
    pub fg: ColorSpec,
    pub bg: ColorSpec,
    /// 协议 flags（BOLD/ITALIC/UNDERLINE/REVERSE/HIDDEN/STRIKE/WIDE）+ 客户端 spacer 位。
    pub flags: u8,
}

/// 可见屏幕 grid。
#[derive(Debug, Clone, PartialEq)]
pub struct ClientGrid {
    pub rows: u16,
    pub cols: u16,
    cells: Vec<ClientCell>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    /// 已应用帧的序列号（丢帧检测：不连续时记录一次）。
    pub last_seq: Option<u64>,
    /// 单调递增版本号：grid 内容变化时 +1，渲染层据此跳过未变化的帧。
    pub version: u64,
}

impl ClientGrid {
    /// 创建一个空白 grid（全默认 cell）。
    pub fn new(rows: u16, cols: u16) -> Self {
        let cell = ClientCell {
            ch: ' ',
            fg: ColorSpec::Default,
            bg: ColorSpec::Default,
            flags: 0,
        };
        Self {
            rows,
            cols,
            cells: vec![cell; rows as usize * cols as usize],
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            last_seq: None,
            version: 0,
        }
    }

    /// 按 (row, col) 取 cell。
    pub fn cell(&self, row: u16, col: u16) -> Option<&ClientCell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells.get(row as usize * self.cols as usize + col as usize)
    }

    /// 全量帧：重置整个 grid。
    pub fn reset(&mut self, rows: u16, cols: u16) {
        if rows != self.rows || cols != self.cols {
            *self = Self::new(rows, cols);
            return;
        }
        let blank = ClientCell {
            ch: ' ',
            fg: ColorSpec::Default,
            bg: ColorSpec::Default,
            flags: 0,
        };
        self.cells.fill(blank);
        self.cursor_visible = false;
        self.version = self.version.wrapping_add(1);
    }

    /// 应用一帧（全量帧由调用方先 reset 再 apply_line；增量帧直接 apply_line）。
    pub fn apply_frame(&mut self, frame: &crate::term::frame::TerminalFrame) {
        // 丢帧检测：seq 不连续时记录一次（状态栏/日志），直接应用最新帧。
        if let Some(last) = self.last_seq {
            if frame.seq.wrapping_sub(last) > 1 {
                tracing::warn!(
                    "终端推送丢帧: last_seq={} new_seq={}",
                    last,
                    frame.seq
                );
            }
        }
        self.last_seq = Some(frame.seq);
        self.cursor_row = frame.cursor_row;
        self.cursor_col = frame.cursor_col;
        self.cursor_visible = frame.cursor_visible;
        for line in &frame.lines {
            self.apply_line(line);
        }
        self.version = self.version.wrapping_add(1);
    }

    /// 按 (row, start_col, end_col) 更新一行区间。
    fn apply_line(&mut self, line: &crate::term::frame::LineUpdate) {
        if line.row >= self.rows {
            return;
        }
        let row_base = line.row as usize * self.cols as usize;
        // 区间外（行内）的 cell 清空为默认——行级 damage 语义。
        let blank = ClientCell {
            ch: ' ',
            fg: ColorSpec::Default,
            bg: ColorSpec::Default,
            flags: 0,
        };
        let end = (line.end_col as usize).min(self.cols as usize - 1);
        if line.start_col as usize > end {
            return;
        }
        for col in line.start_col as usize..=end {
            self.cells[row_base + col] = blank;
        }
        // 展开 runs 到区间
        let mut buf: Vec<ClientCell> = Vec::new();
        for run in &line.runs {
            crate::term::frame::expand_run(run, &mut buf, self.cols);
        }
        let mut col = line.start_col as usize;
        for cell in buf {
            if col > end {
                break;
            }
            self.cells[row_base + col] = cell;
            col += 1;
        }
    }
}

/// 标准 xterm 256 色表（0-15 基础色 + 16-231 立方体 + 232-255 灰度）。
/// 与 alacritty 默认配色一致，Indexed 颜色索引查此表。
pub fn indexed_color(idx: u8) -> (u8, u8, u8) {
    const BASIC: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00), // 0 black
        (0x80, 0x00, 0x00), // 1 red
        (0x00, 0x80, 0x00), // 2 green
        (0x80, 0x80, 0x00), // 3 yellow
        (0x00, 0x00, 0x80), // 4 blue
        (0x80, 0x00, 0x80), // 5 magenta
        (0x00, 0x80, 0x80), // 6 cyan
        (0xc0, 0xc0, 0xc0), // 7 white
        (0x80, 0x80, 0x80), // 8 bright black
        (0xff, 0x00, 0x00), // 9 bright red
        (0x00, 0xff, 0x00), // 10 bright green
        (0xff, 0xff, 0x00), // 11 bright yellow
        (0x00, 0x00, 0xff), // 12 bright blue
        (0xff, 0x00, 0xff), // 13 bright magenta
        (0x00, 0xff, 0xff), // 14 bright cyan
        (0xff, 0xff, 0xff), // 15 bright white
    ];
    if idx < 16 {
        return BASIC[idx as usize];
    }
    if idx < 232 {
        let n = idx - 16;
        let r = (n / 36) as u8;
        let g = ((n / 6) % 6) as u8;
        let b = (n % 6) as u8;
        let cube = |v: u8| -> u8 {
            if v == 0 {
                0
            } else {
                55 + v * 40
            }
        };
        return (cube(r), cube(g), cube(b));
    }
    let gray = 8 + (idx - 232) * 10;
    (gray, gray, gray)
}
