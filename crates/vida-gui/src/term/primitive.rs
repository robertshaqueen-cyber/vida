//! 终端外观与 viewport 度量。
//!
//! M2b-2 最初在这里维护了一套独立的 wgpu 字形图集。实际验证表明，自建
//! CoreText 位图、基线换算和纹理采样会让细横线消失，并让 CJK 回退字形产生
//! 比例偏差。现在文字层与 Oryxis 一样交给 iced canvas/cosmic-text；本模块只
//! 保留终端外观、共享字体和 PTY resize 所需的实测 cell 尺寸。

use std::sync::atomic::{AtomicU64, Ordering};

use super::client_grid::{ClientCell, ColorSpec};
use super::frame;

pub(crate) const DEFAULT_FG: (u8, u8, u8) = (255, 255, 255);
pub(crate) const DEFAULT_BG: (u8, u8, u8) = (40, 44, 52);
const DEFAULT_FONT_SIZE: f32 = 13.0;
pub(crate) const DEFAULT_TERMINAL_FONT_FAMILY: &str = "JetBrains Mono";
pub(crate) const BUNDLED_REGULAR: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");
pub(crate) const BUNDLED_BOLD: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Bold.ttf");

pub(crate) fn load_bundled_terminal_fonts(database: &mut fontdb::Database) {
    database.load_font_data(BUNDLED_REGULAR.to_vec());
    database.load_font_data(BUNDLED_BOLD.to_vec());
}

/// 可持久化的终端外观。
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalAppearance {
    pub font_family: String,
    pub font_size: f32,
    pub cursor_blink: bool,
}

impl Default for TerminalAppearance {
    fn default() -> Self {
        Self {
            font_family: DEFAULT_TERMINAL_FONT_FAMILY.to_string(),
            font_size: DEFAULT_FONT_SIZE,
            cursor_blink: true,
        }
    }
}

impl TerminalAppearance {
    pub(crate) fn normalized(&self) -> Self {
        let family = self.font_family.trim();
        Self {
            font_family: if family.is_empty() {
                DEFAULT_TERMINAL_FONT_FAMILY.to_string()
            } else {
                family.to_string()
            },
            font_size: if self.font_size.is_finite() {
                self.font_size.clamp(8.0, 48.0)
            } else {
                DEFAULT_FONT_SIZE
            },
            cursor_blink: self.cursor_blink,
        }
    }
}

/// 渲染线程向交互 widget 回传的真实物理像素 cell 尺寸。
#[derive(Debug)]
pub struct ViewportMetrics {
    /// [scale f32 bits:32][height u16:16][width u16:16]
    packed: AtomicU64,
}

impl Default for ViewportMetrics {
    fn default() -> Self {
        Self {
            packed: AtomicU64::new(0),
        }
    }
}

impl ViewportMetrics {
    pub fn store(&self, cell_width: f32, cell_height: f32, scale: f32) {
        let width = cell_width.round().clamp(1.0, u16::MAX as f32) as u64;
        let height = cell_height.round().clamp(1.0, u16::MAX as f32) as u64;
        let packed = (u64::from(scale.to_bits()) << 32) | (height << 16) | width;
        self.packed.store(packed, Ordering::Release);
    }

    pub fn cell_size_for(&self, scale: f32) -> (f32, f32) {
        let packed = self.packed.load(Ordering::Acquire);
        if (packed >> 32) as u32 == scale.to_bits() {
            let width = (packed & 0xffff) as u16;
            let height = ((packed >> 16) & 0xffff) as u16;
            if width > 0 && height > 0 {
                return (f32::from(width), f32::from(height));
            }
        }
        let font_size = std::env::var("VIDA_FONT_SIZE")
            .ok()
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|value| *value >= 8.0 && *value <= 48.0)
            .unwrap_or(DEFAULT_FONT_SIZE);
        (
            (font_size * 0.6 * scale).round().max(1.0),
            (font_size * 1.15 * scale).round().max(1.0),
        )
    }
}

fn resolve_fg(cell: &ClientCell) -> (u8, u8, u8) {
    match cell.fg {
        ColorSpec::Default => DEFAULT_FG,
        ColorSpec::Indexed(index) => super::client_grid::indexed_color(index),
        ColorSpec::Rgb(r, g, b) => (r, g, b),
    }
}

pub(crate) fn resolve_bg(cell: &ClientCell) -> (u8, u8, u8) {
    if cell.flags & frame::flag::REVERSE != 0 {
        return resolve_fg(cell);
    }
    match cell.bg {
        ColorSpec::Default => DEFAULT_BG,
        ColorSpec::Indexed(index) => super::client_grid::indexed_color(index),
        ColorSpec::Rgb(r, g, b) => (r, g, b),
    }
}

pub(crate) fn resolve_text_fg(cell: &ClientCell) -> (u8, u8, u8) {
    if cell.flags & frame::flag::REVERSE == 0 {
        return resolve_fg(cell);
    }
    match cell.bg {
        ColorSpec::Default => DEFAULT_BG,
        ColorSpec::Indexed(index) => super::client_grid::indexed_color(index),
        ColorSpec::Rgb(r, g, b) => (r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_is_normalized() {
        let appearance = TerminalAppearance {
            font_family: "  ".into(),
            font_size: f32::NAN,
            cursor_blink: false,
        }
        .normalized();
        assert_eq!(appearance.font_family, DEFAULT_TERMINAL_FONT_FAMILY);
        assert_eq!(appearance.font_size, DEFAULT_FONT_SIZE);
        assert!(!appearance.cursor_blink);
    }

    #[test]
    fn reverse_swaps_default_colors() {
        let cell = ClientCell {
            ch: 'A',
            fg: ColorSpec::Default,
            bg: ColorSpec::Default,
            flags: frame::flag::REVERSE,
        };
        assert_eq!(resolve_bg(&cell), DEFAULT_FG);
        assert_eq!(resolve_text_fg(&cell), DEFAULT_BG);
    }

    #[test]
    fn viewport_metrics_round_trip_for_same_scale() {
        let metrics = ViewportMetrics::default();
        metrics.store(7.8, 14.95, 2.0);
        assert_eq!(metrics.cell_size_for(2.0), (8.0, 15.0));
    }
}
