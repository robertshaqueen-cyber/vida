//! 终端帧解码（M2b-1）。
//!
//! 与 daemon 的 `encode_frame`（crates/vida-daemon/src/pty/push.rs）逐字节对应。
//! 帧格式（payload，不含帧头）：
//!
//! ```text
//! [seq: u64 BE][cursor_row: u16][cursor_col: u16][cursor_visible: u8]
//! [line_count: u16]
//!   每行：
//!     [row: u16][start_col: u16][end_col: u16]
//!     [run_count: u16]
//!       每个 run：
//!         [run_len: u16][flags: u8][fg_tag: u8][fg payload]
//!         [bg_tag: u8][bg payload][char_len: u8][char bytes]
//! ```

use crate::term::client_grid::{ClientCell, ColorSpec, cell_flags};

/// 属性标志位（与 daemon FLAG_* 一致）。
/// italic/hidden/strike 目前只在 grid 里保留标志（渲染尚未实现），
/// 与 daemon 协议字段一一对应，故允许暂时未使用。
#[allow(dead_code)]
pub mod flag {
    pub const BOLD: u8 = 0x01;
    pub const ITALIC: u8 = 0x02;
    pub const UNDERLINE: u8 = 0x04;
    pub const REVERSE: u8 = 0x08;
    pub const HIDDEN: u8 = 0x10;
    pub const STRIKEOUT: u8 = 0x20;
    pub const WIDE: u8 = 0x40;
}

/// 颜色 tag（与 daemon encode_color 一致）。
const COLOR_DEFAULT: u8 = 0x00;
const COLOR_INDEXED: u8 = 0x01;
const COLOR_RGB: u8 = 0x02;

/// 解码后的一行区间更新。
#[derive(Debug, Clone, PartialEq)]
pub struct LineUpdate {
    pub row: u16,
    pub start_col: u16,
    pub end_col: u16,
    pub runs: Vec<Run>,
}

/// 解码后的 RLE run。
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub len: u16,
    pub flags: u8,
    pub fg: ColorSpec,
    pub bg: ColorSpec,
    pub ch: char,
}

/// 解码后的终端帧。
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalFrame {
    pub seq: u64,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub lines: Vec<LineUpdate>,
}

/// 字节流解析器（防越界：所有读取先校验剩余长度）。
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return None;
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u64(&mut self) -> Option<u64> {
        let b = self.take(8)?;
        Some(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
}

/// 解码一帧。失败返回 None（帧损坏/截断，静默丢弃并计入丢帧日志）。
pub fn decode_frame(payload: &[u8]) -> Option<TerminalFrame> {
    let mut r = Reader::new(payload);
    let seq = r.u64()?;
    let cursor_row = r.u16()?;
    let cursor_col = r.u16()?;
    let cursor_visible = match r.u8()? {
        1 => true,
        0 => false,
        _ => return None,
    };
    let line_count = r.u16()?;
    let mut lines = Vec::with_capacity(line_count as usize);
    for _ in 0..line_count {
        let row = r.u16()?;
        let start_col = r.u16()?;
        let end_col = r.u16()?;
        let run_count = r.u16()?;
        let mut runs = Vec::with_capacity(run_count as usize);
        for _ in 0..run_count {
            let len = r.u16()?;
            let flags = r.u8()?;
            let fg = decode_color(&mut r)?;
            let bg = decode_color(&mut r)?;
            let char_len = r.u8()? as usize;
            let char_bytes = r.take(char_len)?;
            // 无效 UTF-8 或空字符视为损坏帧
            if char_bytes.is_empty() {
                return None;
            }
            let text = std::str::from_utf8(char_bytes).ok()?;
            let ch = text.chars().next()?;
            runs.push(Run {
                len,
                flags,
                fg,
                bg,
                ch,
            });
        }
        lines.push(LineUpdate {
            row,
            start_col,
            end_col,
            runs,
        });
    }
    Some(TerminalFrame {
        seq,
        cursor_row,
        cursor_col,
        cursor_visible,
        lines,
    })
}

fn decode_color(r: &mut Reader) -> Option<ColorSpec> {
    match r.u8()? {
        COLOR_DEFAULT => Some(ColorSpec::Default),
        COLOR_INDEXED => Some(ColorSpec::Indexed(r.u8()?)),
        COLOR_RGB => {
            let rr = r.u8()?;
            let gg = r.u8()?;
            let bb = r.u8()?;
            Some(ColorSpec::Rgb(rr, gg, bb))
        }
        _ => None,
    }
}

/// 把 run 展开成 cell 序列写入缓冲区。
///
/// 宽字符约定（协议）：flags 含 WIDE 的 cell 占两列，run_len=2；
/// 第二列由客户端填充 spacer（`cell_flags::WIDE_SPACER`），渲染时跳过字形。
/// 展开后返回 (列数, 是否未用尽的 run_len)。
pub fn expand_run(run: &Run, cells: &mut Vec<ClientCell>, cols: u16) {
    let mut col = cells.len() as u16;
    let mut remaining = run.len;
    while remaining > 0 && col < cols {
        let wide = run.flags & flag::WIDE != 0;
        let cell = ClientCell {
            ch: run.ch,
            fg: run.fg,
            bg: run.bg,
            flags: run.flags,
        };
        cells.push(cell);
        col += 1;
        remaining -= 1;
        if wide {
            // 第二列填 spacer（渲染跳过字形，背景照画）
            if col < cols && remaining > 0 {
                cells.push(ClientCell {
                    ch: ' ',
                    fg: run.fg,
                    bg: run.bg,
                    flags: run.flags | cell_flags::WIDE_SPACER,
                });
                col += 1;
                remaining -= 1;
            }
        }
    }
}
