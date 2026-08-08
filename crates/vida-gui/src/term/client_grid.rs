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
        self.cells
            .get(row as usize * self.cols as usize + col as usize)
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
        if let Some(last) = self.last_seq
            && frame.seq.wrapping_sub(last) > 1
        {
            tracing::warn!("终端推送丢帧: last_seq={} new_seq={}", last, frame.seq);
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
        let col = line.start_col as usize;
        for (i, cell) in buf
            .into_iter()
            .take(end.saturating_sub(col) + 1)
            .enumerate()
        {
            self.cells[row_base + col + i] = cell;
        }
    }
}

/// Ghostty 默认 256 色表（0-15 为 Ghostty 默认主题，后续为 xterm 色立方与灰阶）。
pub fn indexed_color(idx: u8) -> (u8, u8, u8) {
    const BASIC: [(u8, u8, u8); 16] = [
        (0x1d, 0x1f, 0x21), // 0 black
        (0xcc, 0x66, 0x66), // 1 red
        (0xb5, 0xbd, 0x68), // 2 green
        (0xf0, 0xc6, 0x74), // 3 yellow
        (0x81, 0xa2, 0xbe), // 4 blue
        (0xb2, 0x94, 0xbb), // 5 magenta
        (0x8a, 0xbe, 0xb7), // 6 cyan
        (0xc5, 0xc8, 0xc6), // 7 white
        (0x66, 0x66, 0x66), // 8 bright black
        (0xd5, 0x4e, 0x53), // 9 bright red
        (0xb9, 0xca, 0x4a), // 10 bright green
        (0xe7, 0xc5, 0x47), // 11 bright yellow
        (0x7a, 0xa6, 0xda), // 12 bright blue
        (0xc3, 0x97, 0xd8), // 13 bright magenta
        (0x70, 0xc0, 0xb1), // 14 bright cyan
        (0xea, 0xea, 0xea), // 15 bright white
    ];
    if idx < 16 {
        return BASIC[idx as usize];
    }
    if idx < 232 {
        let n = idx - 16;
        let r = n / 36;
        let g = (n / 6) % 6;
        let b = n % 6;
        let cube = |v: u8| -> u8 { if v == 0 { 0 } else { 55 + v * 40 } };
        return (cube(r), cube(g), cube(b));
    }
    let gray = 8 + (idx - 232) * 10;
    (gray, gray, gray)
}

#[cfg(test)]
mod tests {
    use super::indexed_color;

    #[test]
    fn indexed_palette_matches_ghostty_defaults() {
        assert_eq!(indexed_color(0), (0x1d, 0x1f, 0x21));
        assert_eq!(indexed_color(8), (0x66, 0x66, 0x66));
        assert_eq!(indexed_color(15), (0xea, 0xea, 0xea));
        assert_eq!(indexed_color(16), (0, 0, 0));
        assert_eq!(indexed_color(231), (255, 255, 255));
    }
}
