//! M2a-0 API 验证探针。
//!
//! 目的：实测 alacritty_terminal 0.26 的核心 API，把真实签名记入
//! docs/decisions.md（读文档不可靠，必须编译运行证实）。
//!
//! 验证内容：
//! 1. `Term<VoidListener>` 构造（Config + Dimensions + listener）
//! 2. `vte::ansi::Processor` 构造与 `advance`
//! 3. 喂入 ANSI 字节流后的 grid 状态（文本/颜色/光标移动/清屏）
//! 4. grid 遍历（display_iter）与 Cell 字段结构
//! 5. 光标位置获取
//! 6. `Term::resize` 签名与行为
//! 7. damage 脏行接口（增量推送的关键）
//! 8. `size_of::<Cell>()`

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Term, TermDamage};
use alacritty_terminal::vte::ansi::{Color, Processor};

/// 80×24 的尺寸参数（Dimensions trait 的最小实现）。
struct ProbeSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for ProbeSize {
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

fn main() {
    let config = Config {
        // 回滚容量：规格要求「有上限，从金库 Settings.scrollback_lines 读，
        // 默认 5000」。此处探针先验证 5000 配置生效。
        scrolling_history: 5000,
        ..Config::default()
    };

    let size = ProbeSize { cols: 80, rows: 24 };
    let mut term: Term<VoidListener> = Term::new(config, &size, VoidListener);
    let mut processor: Processor<alacritty_terminal::vte::ansi::StdSyncHandler> = Processor::new();

    // ---- 1. 喂入混合字节流 ----
    let bytes = b"\x1b[31mRED\x1b[0m plain text\x1b[2;5H\x1b[32mGREEN@5,2\x1b[0m\x1b[2J\x1b[1;1Hhello world\x1b[3;1Hline three\nline four";
    processor.advance(&mut term, bytes);

    // ---- 2. grid 遍历：打印屏幕 ----
    println!("=== grid (display_iter) ===");
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
        let cell = indexed.cell;
        print!("{}", cell.c);
    }
    println!();
    println!("=== end grid ===");

    // ---- 3. 光标位置 ----
    let cursor_point = term.grid().cursor.point;
    println!(
        "cursor: row={} col={}",
        cursor_point.line.0, cursor_point.column.0
    );

    // ---- 4. Cell 字段 ----
    let cell = term.grid()[alacritty_terminal::index::Point::new(
        alacritty_terminal::index::Line(0),
        alacritty_terminal::index::Column(0),
    )]
    .clone();
    println!(
        "cell[0,0]: c={:?} fg={:?} bg={:?} flags={:?}",
        cell.c, cell.fg, cell.bg, cell.flags
    );
    println!("size_of::<Cell>() = {}", std::mem::size_of::<Cell>());
    println!("size_of::<Color>() = {}", std::mem::size_of::<Color>());
    println!("size_of::<Colors>() = {}", std::mem::size_of::<Colors>());

    // ---- 5. resize ----
    let before = term.grid().screen_lines();
    term.resize(ProbeSize { cols: 40, rows: 12 });
    let after = term.grid().screen_lines();
    println!("resize: {} rows -> {} rows", before, after);

    // ---- 6. damage 脏行接口 ----
    // 重新喂入一段文本，然后读 damage。第一次构造后未 reset，
    // damage 初始状态应为 Full。
    let damage1 = term.damage();
    println!(
        "damage after clear+write: {:?}",
        match damage1 {
            TermDamage::Full => "Full".to_string(),
            TermDamage::Partial(it) => {
                let bounds: Vec<_> = it.map(|b| (b.line, b.left, b.right)).collect();
                format!("Partial({:?})", bounds)
            }
        }
    );
    term.reset_damage();
    let damage2 = term.damage();
    println!(
        "damage after reset: {:?}",
        match damage2 {
            TermDamage::Full => "Full".to_string(),
            TermDamage::Partial(it) => {
                let bounds: Vec<_> = it.map(|b| (b.line, b.left, b.right)).collect();
                format!("Partial({:?})", bounds)
            }
        }
    );
    term.reset_damage();

    // ---- 7. damage 跟踪新写入的脏行（增量推送的关键验证） ----
    processor.advance(&mut term, b"write on row 0\r\n");
    let damage3 = term.damage();
    println!(
        "damage after new write: {:?}",
        match damage3 {
            TermDamage::Full => "Full".to_string(),
            TermDamage::Partial(it) => {
                let bounds: Vec<_> = it.map(|b| (b.line, b.left, b.right)).collect();
                format!("Partial({:?})", bounds)
            }
        }
    );
    term.reset_damage();
}
