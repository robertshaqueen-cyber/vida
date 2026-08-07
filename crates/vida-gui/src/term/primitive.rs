//! 终端渲染 Primitive（M2b-1）。
//!
//! 搬运自 M2b-0 `term_grid` example 的渲染管线，嵌入 iced_wgpu::Primitive：
//! - `build_frame`（CPU：图集 + quads 生成）→ `TermPrimitive::prepare` 之前的部分
//! - `prepare`：版本号 O(1) 跳过；变化时重建 quads、上传图集新增字形
//! - `draw`：复用现有 RenderPass 绘制（set_pipeline/bind_group/buffers/draw）
//! - 关联 `Pipeline`（TermPipeline）持有 GPU 资源 + 字形缓存（跨帧复用）
//!
//! 硬约束：
//! - 宽字符信息一律来自协议 WIDE flag（客户端不自行判定）
//! - 字符数 ≠ 列数：位置/宽度一律用列号
//! - 渲染路径不得出现 unwrap / expect / unreachable! / unwrap_or_default()

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytemuck::{Pod, Zeroable};
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, SwashCache};
use iced::Rectangle;
use iced_graphics::Viewport;
use iced_wgpu::Primitive as IcedPrimitive;

use super::client_grid::{ClientGrid, ColorSpec, cell_flags};
use super::frame;

/// 顶点：xy = 物理像素坐标，uv = 图集归一化坐标，color = RGBA。
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    xy: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

/// 字体度量（与 M2b-0 一致）。
///
/// 约定（decisions.md）：**内部一律用物理像素**，只有与 iced 布局交互
/// （widget bounds、鼠标坐标）时才做换算。cell_* / ascent 都是物理像素值，
/// scale 变化（跨 DPI 拖动窗口）时重新测量。
#[derive(Clone, Copy)]
struct FontMetrics {
    cell_width: f32,
    cell_height: f32,
    /// 主字体 ascent（基线到顶部距离），全局统一基线。
    ascent: f32,
    /// 光栅化缩放因子（= viewport scale_factor）。
    scale: f32,
    /// 逻辑字号（em 单位）。默认 13（与 Ghostty macOS 默认值一致），
    /// 可用 VIDA_FONT_SIZE 覆盖。
    font_size: f32,
}

/// 终端默认配色（主题色）。
const DEFAULT_FG: (u8, u8, u8) = (255, 255, 255);
const DEFAULT_BG: (u8, u8, u8) = (40, 44, 52);
const DEFAULT_FONT_SIZE: f32 = 13.0;

/// 字形缓存条目：(宽, 高, u0, v0, u1, v1, placement.left, placement.top)。
/// left/top 都是整数（物理像素），glyph_x = cell_x + left、
/// glyph_y = 基线取整 - top，保证字形 quad 落在整数像素边界。
type GlyphEntry = (u32, u32, f32, f32, f32, f32, f32, f32);

/// 一帧渲染所需的全部几何数据（变化时才重建）。
struct BuiltGeometry {
    /// 完整图集（512×512 RGBA），字形缓存命中时不重复光栅化。
    atlas: Vec<u8>,
    /// 本次新增字形的图集区域（用于增量上传）。
    dirty_regions: Vec<(u32, u32, u32, u32)>, // (x, y, w, h)
    quads: Vec<Vertex>,
}

/// 终端 Primitive：持有一个不可变 grid 快照（Arc 共享）。
#[derive(Debug, Clone)]
pub struct TermPrimitive {
    snapshot: Arc<ClientGrid>,
    bounds: Rectangle,
    viewport_metrics: Arc<ViewportMetrics>,
    appearance: TerminalAppearance,
    cursor_on: bool,
}

impl TermPrimitive {
    #[cfg(test)]
    pub fn new(
        snapshot: Arc<ClientGrid>,
        bounds: Rectangle,
        viewport_metrics: Arc<ViewportMetrics>,
    ) -> Self {
        Self {
            snapshot,
            bounds,
            viewport_metrics,
            appearance: TerminalAppearance::default(),
            cursor_on: true,
        }
    }

    pub fn with_appearance(
        snapshot: Arc<ClientGrid>,
        bounds: Rectangle,
        viewport_metrics: Arc<ViewportMetrics>,
        appearance: TerminalAppearance,
        cursor_on: bool,
    ) -> Self {
        Self {
            snapshot,
            bounds,
            viewport_metrics,
            appearance: appearance.normalized(),
            cursor_on,
        }
    }
}

/// 可持久化的终端外观。默认值与本机 Ghostty 接近。
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalAppearance {
    pub font_family: String,
    pub font_size: f32,
    pub cursor_blink: bool,
}

impl Default for TerminalAppearance {
    fn default() -> Self {
        Self {
            font_family: "Menlo".to_string(),
            font_size: DEFAULT_FONT_SIZE,
            cursor_blink: true,
        }
    }
}

impl TerminalAppearance {
    fn normalized(&self) -> Self {
        let font_family = self.font_family.trim();
        Self {
            font_family: if font_family.is_empty() {
                "Menlo".to_string()
            } else {
                font_family.to_string()
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

/// 渲染管线向交互 widget 回传的真实物理像素 cell 尺寸。
///
/// 原子字段避免渲染线程与 UI 线程互相持锁。`scale_bits == 0` 表示渲染器
/// 尚未给出实测值；widget 会暂用与渲染器相同的默认字号公式，下一帧自动校正。
#[derive(Debug)]
pub struct ViewportMetrics {
    /// 单次原子读写保证 width/height/scale 来自同一次测量：
    /// [scale f32 bits:32][height u16:16][width u16:16]。
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
        let measured_scale_bits = (packed >> 32) as u32;
        if measured_scale_bits == scale.to_bits() {
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
            (font_size * 1.25 * scale).round().max(1.0),
        )
    }
}

/// 跨帧共享的渲染状态（iced 对每种 Primitive 类型只建一次）。
pub struct TermPipeline {
    font_system: Mutex<FontSystem>,
    metrics: FontMetrics,
    render_pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    atlas_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    vertex_count: u32,
    /// 字形缓存：跨帧复用，同字符同字重同 scale 只光栅化一次。
    /// key = (char, weight, scale.to_bits())——scale 变化（跨 DPI 拖动）
    /// 时旧字形必须失效重新光栅化。
    glyph_cache: Mutex<std::collections::HashMap<(char, u16, u32), GlyphEntry>>,
    /// 已构建的 grid 版本号：prepare 里 O(1) 跳过未变化的帧。
    built_version: AtomicU64,
    /// 图集持久数据（增量上传用）。
    atlas_data: Mutex<Vec<u8>>,
    /// 图集打包 cursor (next_x, next_y, row_h)——必须跨帧持久：
    /// 每帧重置会让新字形覆盖已写入的字形（ASCII 靠前最易被覆盖）。
    atlas_cursor: Mutex<(u32, u32, u32)>,
    /// surface 是否为 sRGB 格式（决定 fs_main 是否做 linear 转换）。
    is_srgb: bool,
    /// 一次性诊断打印标记（测量用）。
    diag_printed: std::sync::atomic::AtomicU8,
    /// 实际使用的字体族（显式指定 + 回落链解析，不依赖加载顺序）。
    font_family: String,
    /// 当前管线采用的外观；变化时清空图集并重新测量。
    appearance: TerminalAppearance,
    /// 当前闪烁相位；只写入 uniform，不重建几何和 GPU buffer。
    cursor_on: bool,
}

impl IcedPrimitive for TermPrimitive {
    type Pipeline = TermPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        let scale = viewport.scale_factor();
        // [测量] 一次性打印 iced 传入的 bounds/viewport 实况
        if pipeline.diag_printed.load(Ordering::Relaxed) == 0 {
            pipeline.diag_printed.store(1, Ordering::Relaxed);
            tracing::info!(
                "term diag: iced_bounds={:?} self_bounds={:?} physical_size={:?} scale={} metrics={}x{} asc={}",
                bounds,
                self.bounds,
                viewport.physical_size(),
                scale,
                pipeline.metrics.cell_width,
                pipeline.metrics.cell_height,
                pipeline.metrics.ascent
            );
        }
        let appearance = self.appearance.normalized();
        if pipeline.appearance != appearance {
            let family = {
                let mut font_system = match pipeline.font_system.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                resolve_font_family(font_system.db_mut(), &appearance.font_family)
            };
            if family != appearance.font_family {
                tracing::warn!(
                    "字体 {} 不可用，回落为 {}（候选: Menlo → SF Mono → Monaco → 默认）",
                    appearance.font_family,
                    family
                );
            }
            pipeline.font_family = family;
            pipeline.metrics.font_size = appearance.font_size;
            pipeline.appearance = appearance;
            reset_render_cache(pipeline, "字体设置变化");
            pipeline.metrics = measure_font_at(pipeline, scale);
        }
        if pipeline.cursor_on != self.cursor_on {
            pipeline.cursor_on = self.cursor_on;
        }

        // scale 变化（窗口拖到不同 DPI 显示器）：旧字形（按旧 scale 光栅化）
        // 必须全部失效——重置字形缓存/图集/cursor 并强制全量重建。
        // 注意 built_version 比较在 scale 检查之后：scale 变了即使 version
        // 相同也要重建（图集内容变了）。
        if pipeline.metrics.scale != scale {
            reset_render_cache(pipeline, "显示器 scale 变化");
            pipeline.metrics = measure_font_at(pipeline, scale);
        }
        self.viewport_metrics.store(
            pipeline.metrics.cell_width,
            pipeline.metrics.cell_height,
            pipeline.metrics.scale,
        );

        // 光标闪烁只更新 16 字节 uniform。光标 overlay 几何始终存在，由 shader
        // 根据第四个 float 决定是否显示，避免每 500ms 重建整屏 GPU buffer。
        let phys = viewport.physical_size();
        let gamma = if pipeline.is_srgb { 1.0 } else { 0.0 };
        let cursor_on = if pipeline.cursor_on { 1.0 } else { 0.0 };
        queue.write_buffer(
            &pipeline.uniform_buffer,
            0,
            bytemuck::bytes_of(&[phys.width as f32, phys.height as f32, gamma, cursor_on]),
        );

        let version = self.snapshot.version;
        // O(1) 跳过：grid 未变化不重建任何东西
        if pipeline.built_version.load(Ordering::Relaxed) == version {
            return;
        }

        let origin_x = self.bounds.x * scale;
        let origin_y = self.bounds.y * scale;

        let geometry = match self.build_geometry(pipeline, origin_x, origin_y) {
            Some(g) => g,
            None => return,
        };

        // 上传图集新增字形区域：按【行区间】上传，行距恒为图集全宽
        // （ATLAS*4 = 2048，满足 wgpu 的 256 字节对齐要求）。
        // 禁止逐字形上传——字形宽度远小于 256 时 bytes_per_row 无法对齐，
        // 且源缓冲行距与目标纹理行距不一致会导致校验失败 panic。
        for (y, h) in merge_row_ranges(&geometry.dirty_regions) {
            upload_atlas_rows(&pipeline.atlas_texture, queue, &geometry.atlas, y, h);
        }

        // 上传 quads
        if geometry.quads.is_empty() {
            pipeline.vertex_count = 0;
            pipeline.built_version.store(version, Ordering::Relaxed);
            return;
        }
        // 索引用 u16：quads 超过上限（65535/4≈16383 个 quad）时截断并告警，
        // 防止索引截断后 draw 越界触发 wgpu 校验 panic（连锁 abort）。
        // 正常终端（200×50）远低于此，截断仅作防御。
        let vertex_limit = (u16::MAX as usize / 4) * 4;
        let vertex_count = geometry.quads.len().min(vertex_limit);
        if vertex_count < geometry.quads.len() {
            tracing::warn!(
                "终端顶点超出 u16 索引上限：{} → 截断为 {}",
                geometry.quads.len(),
                vertex_count
            );
        }
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("term quads"),
            size: (vertex_count * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &vbuf,
            0,
            bytemuck::cast_slice(&geometry.quads[..vertex_count]),
        );
        pipeline.vertex_buffer = vbuf;

        let idx = quad_indices(vertex_count);
        let ibuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("term indices"),
            size: (idx.len() * 2) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&ibuf, 0, bytemuck::cast_slice(&idx));
        pipeline.index_buffer = ibuf;
        pipeline.vertex_count = idx.len() as u32;

        pipeline.built_version.store(version, Ordering::Relaxed);
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if pipeline.vertex_count == 0 {
            return true;
        }
        render_pass.set_pipeline(&pipeline.render_pipeline);
        render_pass.set_bind_group(0, &pipeline.uniform_bind_group, &[]);
        render_pass.set_bind_group(1, &pipeline.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, pipeline.vertex_buffer.slice(..));
        render_pass.set_index_buffer(pipeline.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
        render_pass.draw_indexed(0..pipeline.vertex_count, 0, 0..1);
        true
    }
}

/// 图集尺寸（行距 1024×4=4096 字节，天然满足 wgpu 256 字节对齐）。
///
/// 2x（Retina）实测：226 个典型字符（95 ASCII + 中文 + 符号）占用
/// 512² 的 58.6%。粗体字形是独立位图（缓存 key 含 bold），真实终端
/// 字符集（vim/输出符号/粗体）会超 512——扩到 1024（4 倍面积）。
/// 纹理内存 1024²×4B = 4MB，可接受。
pub const ATLAS_SIZE: u32 = 1024;

/// 上传图集的一批连续行（[y, y+h)）到 GPU 纹理。
/// 源缓冲为 CPU 完整图集（每行 ATLAS_SIZE*4 字节），bytes_per_row 恒为
/// 2048 —— 与源数据实际布局一致，且满足 COPY_BYTES_PER_ROW_ALIGNMENT。
/// 参数错误会在 wgpu 校验层 panic（启动自检依赖此行为，见 Pipeline::new）。
pub fn upload_atlas_rows(
    texture: &wgpu::Texture,
    queue: &wgpu::Queue,
    atlas: &[u8],
    y: u32,
    h: u32,
) {
    let rows_data = extract_rows(atlas, y, h);
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x: 0, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        &rows_data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(ATLAS_SIZE * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: ATLAS_SIZE,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}

/// 从 CPU 完整图集提取 [0, ATLAS_SIZE) × [y, y+h) 的数据（行距 2048）。
fn extract_rows(atlas: &[u8], y: u32, h: u32) -> Vec<u8> {
    const STRIDE: usize = ATLAS_SIZE as usize * 4;
    let mut out = Vec::with_capacity(STRIDE * h as usize);
    for py in 0..h {
        let row_start = ((y + py) as usize) * STRIDE;
        let end = (row_start + STRIDE).min(atlas.len());
        out.extend_from_slice(&atlas[row_start..end]);
        // 图集数据不足时补零（防御，正常不会发生）
        out.resize(out.len() + (STRIDE - (end - row_start)), 0);
    }
    out
}

/// scale 变化时重置图集相关状态：清字形缓存、图集归零、
/// cursor 重置、全量上传空图集、强制重建（built_version 置 MAX）。
fn reset_render_cache(pipeline: &mut TermPipeline, reason: &str) {
    tracing::info!("终端渲染缓存重置: {}", reason);
    if let Ok(mut guard) = pipeline.glyph_cache.lock() {
        guard.clear();
    }
    let mut fresh = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize];
    fresh[0] = 255;
    fresh[1] = 255;
    fresh[2] = 255;
    fresh[3] = 255;
    if let Ok(mut guard) = pipeline.atlas_data.lock() {
        *guard = fresh.clone();
    }
    if let Ok(mut guard) = pipeline.atlas_cursor.lock() {
        *guard = (1, 0, 0);
    }
    // 不全量上传 4MB 空纹理：旧字形所在 UV 已随 glyph_cache 清空而不可达，
    // 新字形会按 dirty region 覆盖实际使用区域。macOS Metal 对全纹理
    // queue.write_texture 会留下约 198MB owned-unmapped graphics 驱动分配。
    // 强制重建：built_version 置 MAX 使其与任何 version 不等
    pipeline.built_version.store(u64::MAX, Ordering::Relaxed);
}

/// 在真实 scale 下重新测量字体（用 pipeline 的 FontSystem）。
fn measure_font_at(pipeline: &TermPipeline, scale: f32) -> FontMetrics {
    let mut font_system = match pipeline.font_system.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    measure_font(
        &mut font_system,
        scale,
        pipeline.metrics.font_size,
        &pipeline.font_family,
    )
}

/// 把字形级脏区域（x,y,w,h）合并成行区间（y, h），重叠/相邻的合并。
fn merge_row_ranges(dirty: &[(u32, u32, u32, u32)]) -> Vec<(u32, u32)> {
    if dirty.is_empty() {
        return Vec::new();
    }
    let mut ranges: Vec<(u32, u32)> = dirty.iter().map(|(_, y, _, h)| (*y, *y + *h)).collect();
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some((_, last_end)) if start <= *last_end => {
                *last_end = (*last_end).max(end);
            }
            _ => merged.push((start, end)),
        }
    }
    merged.into_iter().map(|(y, end)| (y, end - y)).collect()
}

impl TermPrimitive {
    /// 构建几何：逐 cell 生成背景/字形/下划线 quads，缺的字形才光栅化。
    /// 失败（如字体度量不可用）返回 None，调用方跳过本帧。
    fn build_geometry(
        &self,
        pipeline: &mut TermPipeline,
        origin_x: f32,
        origin_y: f32,
    ) -> Option<BuiltGeometry> {
        let mut glyph_cache = match pipeline.glyph_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut atlas = match pipeline.atlas_data.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if atlas.len() != (ATLAS_SIZE * ATLAS_SIZE * 4) as usize {
            atlas = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize];
            atlas[0] = 255;
            atlas[1] = 255;
            atlas[2] = 255;
            atlas[3] = 255;
        }
        // 打包 cursor 从 pipeline 取（跨帧持久），结束时写回
        let (next_x, next_y, row_h) = match pipeline.atlas_cursor.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        };
        let metrics = pipeline.metrics;
        let mut atlas_state = AtlasState {
            data: atlas,
            next_x,
            next_y,
            row_h,
            dirty: Vec::new(),
            scale: metrics.scale,
            font_size: metrics.font_size,
            font_family: pipeline.font_family.clone(),
        };

        let mut font_system = match pipeline.font_system.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut cache = SwashCache::new();
        let cw = metrics.cell_width;
        let ch = metrics.cell_height;
        let grid = &self.snapshot;
        let mut quads: Vec<Vertex> = Vec::new();

        // 整数对齐：origin 与 cell 尺寸都是整数物理像素——
        // 任何小数都会让字形 quad 落在非整数像素边界导致发虚。
        let origin_x = origin_x.round();
        let origin_y = origin_y.round();
        for row in 0..grid.rows {
            let row_y = origin_y + row as f32 * ch;
            let mut col: u16 = 0;
            while col < grid.cols {
                let Some(cell) = grid.cell(row, col) else {
                    break;
                };
                let is_spacer = cell.flags & cell_flags::WIDE_SPACER != 0;
                let is_wide = cell.flags & frame::flag::WIDE != 0;
                // 列宽一律由协议 WIDE 位决定（宽=2，普通=1），不自行判定
                let col_width: u16 = if is_wide { 2 } else { 1 };
                let cell_x = origin_x + col as f32 * cw;
                let is_cursor =
                    grid.cursor_visible && row == grid.cursor_row && col == grid.cursor_col;

                // 先正常绘制 cell；光标作为带特殊 alpha 标记的 overlay 最后叠加。
                // shader 在暗相位丢弃 overlay，底下的正常 cell 会自然显现。
                let effective_bg = resolve_bg(cell);
                let effective_fg = resolve_text_fg(cell);
                if cell.bg != ColorSpec::Default || cell.flags & frame::flag::REVERSE != 0 {
                    push_quad(
                        &mut quads,
                        [cell_x, row_y, cell_x + cw * col_width as f32, row_y + ch],
                        [0.0, 0.0, 0.0, 0.0],
                        [
                            effective_bg.0 as f32 / 255.0,
                            effective_bg.1 as f32 / 255.0,
                            effective_bg.2 as f32 / 255.0,
                            1.0,
                        ],
                    );
                }

                // 字形：spacer 不画（其前半 cell 已画宽字符）
                let glyph = if !is_spacer {
                    let bold = cell.flags & frame::flag::BOLD != 0;
                    rasterize_char(
                        &mut font_system,
                        &mut cache,
                        &mut glyph_cache,
                        &mut atlas_state,
                        ATLAS_SIZE,
                        cell.ch,
                        glyph_weight(bold, is_wide),
                    )
                } else {
                    None
                };
                if let Some((gw, gh, u0, v0, u1, v1, left, top)) = glyph {
                    let fg = [
                        effective_fg.0 as f32 / 255.0,
                        effective_fg.1 as f32 / 255.0,
                        effective_fg.2 as f32 / 255.0,
                        1.0,
                    ];
                    // 整数像素对齐：glyph_x = cell_x + placement.left
                    // （left 是整数）；基线取整后 - top（top 是整数）。
                    // 三处全是整数 → 字形 quad 落在整数像素边界。
                    let gx = cell_x + left;
                    let y = (row_y + metrics.ascent).round() - top;
                    push_quad(
                        &mut quads,
                        [gx, y, gx + gw as f32, y + gh as f32],
                        [u0, v0, u1, v1],
                        fg,
                    );
                }

                // 下划线：基线下方 2px，宽度 = cell 列宽。
                // 高度必须取整像素（1px）——1.5px 会落在非整数像素边界。
                if cell.flags & frame::flag::UNDERLINE != 0 {
                    let uy = (row_y + metrics.ascent + 2.0).round();
                    let lc = [
                        effective_fg.0 as f32 / 255.0,
                        effective_fg.1 as f32 / 255.0,
                        effective_fg.2 as f32 / 255.0,
                        1.0,
                    ];
                    push_quad(
                        &mut quads,
                        [cell_x, uy, cell_x + cw * col_width as f32, uy + 1.0],
                        [0.0, 0.0, 0.0, 0.0],
                        lc,
                    );
                }

                if is_cursor {
                    // alpha=-1 是 cursor overlay 标记；shader 输出前取绝对值，
                    // 并用 uniform 的 cursor_on 控制可见性。
                    push_quad(
                        &mut quads,
                        [cell_x, row_y, cell_x + cw * col_width as f32, row_y + ch],
                        [0.0, 0.0, 0.0, 0.0],
                        [
                            effective_fg.0 as f32 / 255.0,
                            effective_fg.1 as f32 / 255.0,
                            effective_fg.2 as f32 / 255.0,
                            -1.0,
                        ],
                    );
                    if let Some((gw, gh, u0, v0, u1, v1, left, top)) = glyph {
                        let gx = cell_x + left;
                        let y = (row_y + metrics.ascent).round() - top;
                        push_quad(
                            &mut quads,
                            [gx, y, gx + gw as f32, y + gh as f32],
                            [u0, v0, u1, v1],
                            [
                                effective_bg.0 as f32 / 255.0,
                                effective_bg.1 as f32 / 255.0,
                                effective_bg.2 as f32 / 255.0,
                                -1.0,
                            ],
                        );
                    }
                }

                col += col_width;
            }
        }

        // 持久化图集与打包 cursor（下次 build 继续累加，不覆盖已有字形）
        if let Ok(mut guard) = pipeline.atlas_data.lock() {
            *guard = atlas_state.data.clone();
        }
        if let Ok(mut guard) = pipeline.atlas_cursor.lock() {
            *guard = (atlas_state.next_x, atlas_state.next_y, atlas_state.row_h);
        }

        Some(BuiltGeometry {
            atlas: atlas_state.data,
            dirty_regions: atlas_state.dirty,
            quads,
        })
    }
}

/// 解析前景色（Indexed 查 256 色表，Default 用主题色）。
fn resolve_fg(cell: &super::client_grid::ClientCell) -> (u8, u8, u8) {
    match cell.fg {
        ColorSpec::Default => DEFAULT_FG,
        ColorSpec::Indexed(i) => super::client_grid::indexed_color(i),
        ColorSpec::Rgb(r, g, b) => (r, g, b),
    }
}

/// 解析背景色：显式 bg 优先；反色时用前景色（前景/背景对调）。
fn resolve_bg(cell: &super::client_grid::ClientCell) -> (u8, u8, u8) {
    if cell.flags & frame::flag::REVERSE != 0 {
        return resolve_fg(cell);
    }
    match cell.bg {
        ColorSpec::Default => DEFAULT_BG,
        ColorSpec::Indexed(i) => super::client_grid::indexed_color(i),
        ColorSpec::Rgb(r, g, b) => (r, g, b),
    }
}

/// 解析反色后的实际文字颜色。
fn resolve_text_fg(cell: &super::client_grid::ClientCell) -> (u8, u8, u8) {
    if cell.flags & frame::flag::REVERSE == 0 {
        return resolve_fg(cell);
    }
    match cell.bg {
        ColorSpec::Default => DEFAULT_BG,
        ColorSpec::Indexed(i) => super::client_grid::indexed_color(i),
        ColorSpec::Rgb(r, g, b) => (r, g, b),
    }
}

/// 推入一个 quad（xy0, xy1 对角，uv0, uv1 对角）。
fn push_quad(
    quads: &mut Vec<Vertex>,
    xy: [f32; 4], // x0, y0, x1, y1
    uv: [f32; 4], // u0, v0, u1, v1
    color: [f32; 4],
) {
    let (x0, y0, x1, y1) = (xy[0], xy[1], xy[2], xy[3]);
    let (u0, v0, u1, v1) = (uv[0], uv[1], uv[2], uv[3]);
    quads.push(Vertex {
        xy: [x0, y0],
        uv: [u0, v0],
        color,
    });
    quads.push(Vertex {
        xy: [x1, y0],
        uv: [u1, v0],
        color,
    });
    quads.push(Vertex {
        xy: [x1, y1],
        uv: [u1, v1],
        color,
    });
    quads.push(Vertex {
        xy: [x0, y1],
        uv: [u0, v1],
        color,
    });
}

/// 为连续的 quad 顶点生成索引。`vertex_count` 必须是 4 的倍数。
fn quad_indices(vertex_count: usize) -> Vec<u16> {
    debug_assert_eq!(vertex_count % 4, 0);
    (0..vertex_count / 4)
        .flat_map(|quad| {
            let base = (quad * 4) as u16;
            [base, base + 1, base + 2, base, base + 2, base + 3]
        })
        .collect()
}

/// 图集打包状态（跨帧持久的部分由调用方保存/恢复）。
struct AtlasState {
    data: Vec<u8>,
    next_x: u32,
    next_y: u32,
    row_h: u32,
    /// 本帧新增字形区域（字形级，上传前合并为行区间）。
    dirty: Vec<(u32, u32, u32, u32)>,
    /// 光栅化 scale（= viewport scale_factor，物理像素）。
    scale: f32,
    /// 逻辑字号（em）。
    font_size: f32,
    /// 实际字体族名（显式指定，非 fallback 结果）。
    font_family: String,
}

/// 光栅化单个字符到图集（带缓存）。返回 (宽, 高, u0, v0, u1, v1, baseline, top)。
fn rasterize_char(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    glyph_cache: &mut std::collections::HashMap<(char, u16, u32), GlyphEntry>,
    state: &mut AtlasState,
    atlas_size: u32,
    ch: char,
    weight: cosmic_text::Weight,
) -> Option<GlyphEntry> {
    let scale = state.scale;
    let key = (ch, weight.0, scale.to_bits());
    if let Some(cached) = glyph_cache.get(&key) {
        return Some(*cached);
    }
    let atlas = &mut state.data;
    let next_x = &mut state.next_x;
    let next_y = &mut state.next_y;
    let row_h = &mut state.row_h;
    let dirty = &mut state.dirty;

    // 字号 × scale（方式 B）：位图与 advance 都是物理像素。
    // 字体族显式指定（不依赖 fallback 顺序——Courier New 曾排在
    // Menlo 前导致 ASCII 用了旧式打字机衬线体）。
    let mut buf = Buffer::new_empty(Metrics::new(
        state.font_size * scale,
        state.font_size * 1.25 * scale,
    ));
    buf.set_size(font_system, Some(100.0), None);
    let mut attrs = Attrs::new().family(cosmic_text::Family::Name(&state.font_family));
    attrs = attrs.weight(weight);
    let text: String = ch.to_string();
    buf.set_text(font_system, &text, &attrs, Shaping::Advanced, None);

    let mut result = None;
    for line in buf.layout_runs() {
        for glyph in line.glyphs {
            let physical = glyph.physical((0.0, 0.0), 1.0);
            let img = match cache.get_image(font_system, physical.cache_key) {
                Some(img) => img,
                None => continue,
            };
            let gw = img.placement.width;
            let gh = img.placement.height;
            if gw == 0 || gh == 0 {
                continue;
            }
            if *next_x + gw > atlas_size {
                *next_x = 1;
                *next_y += (*row_h).max(1);
                *row_h = 0;
            }
            if *next_y + gh > atlas_size {
                tracing::warn!("图集溢出: ch={:?} w={} h={}", ch, gw, gh);
                continue;
            }
            // 按 img.content 分支处理像素格式（不再假定单通道）：
            // - Mask: 1 字节/像素（cosmic_text 硬编码 Format::Alpha，主路径）
            // - SubpixelMask: 4 字节/像素 RGBA 子像素抗锯齿，取 RGB 均值作
            //   alpha（终端灰度渲染即可，不做子像素）
            // - Color: 彩色字形（emoji），本轮不支持 → warn + 跳过
            // 显式长度校验，不用钳位——钳位会把越界变成「重复读最后一字节」，
            // 让错误信号消失、只表现为画错。
            let mut incomplete = false;
            match img.content {
                cosmic_text::SwashContent::Mask => {
                    for py in 0..gh {
                        for px in 0..gw {
                            let src = (py * gw + px) as usize;
                            let Some(&a) = img.data.get(src) else {
                                tracing::warn!(
                                    "Mask 字形数据不完整: ch={:?} w={} h={} len={}",
                                    ch,
                                    gw,
                                    gh,
                                    img.data.len()
                                );
                                incomplete = true;
                                break;
                            };
                            let idx = (((*next_y + py) * atlas_size + (*next_x + px)) as usize) * 4;
                            atlas[idx] = 255;
                            atlas[idx + 1] = 255;
                            atlas[idx + 2] = 255;
                            atlas[idx + 3] = darken_glyph_alpha(a);
                        }
                    }
                }
                cosmic_text::SwashContent::SubpixelMask => {
                    for py in 0..gh {
                        for px in 0..gw {
                            let src = (py * gw + px) as usize * 4;
                            let Some(slice) = img.data.get(src..src + 4) else {
                                tracing::warn!(
                                    "SubpixelMask 字形数据不完整: ch={:?} w={} h={} len={}",
                                    ch,
                                    gw,
                                    gh,
                                    img.data.len()
                                );
                                incomplete = true;
                                break;
                            };
                            let a = (slice[0] as u32 + slice[1] as u32 + slice[2] as u32) / 3;
                            let idx = (((*next_y + py) * atlas_size + (*next_x + px)) as usize) * 4;
                            atlas[idx] = 255;
                            atlas[idx + 1] = 255;
                            atlas[idx + 2] = 255;
                            atlas[idx + 3] = darken_glyph_alpha(a as u8);
                        }
                    }
                }
                cosmic_text::SwashContent::Color => {
                    // 彩色字形（如 emoji）：本轮不支持，跳过该字形（背景照画）。
                    // decisions.md 已记录已知限制。
                    tracing::warn!("彩色字形暂不支持，跳过: ch={:?}", ch);
                    continue;
                }
            }
            if incomplete {
                // 数据不完整：跳过该字形，不推进 cursor、不记录 dirty
                continue;
            }
            let u0 = *next_x as f32 / atlas_size as f32;
            let v0 = *next_y as f32 / atlas_size as f32;
            let u1 = (*next_x + gw) as f32 / atlas_size as f32;
            let v1 = (*next_y + gh) as f32 / atlas_size as f32;
            let entry = (
                gw,
                gh,
                u0,
                v0,
                u1,
                v1,
                img.placement.left as f32,
                img.placement.top as f32,
            );
            dirty.push((*next_x, *next_y, gw, gh));
            *next_x += gw;
            *row_h = (*row_h).max(gh);
            result = Some(entry);
            break;
        }
        if result.is_some() {
            break;
        }
    }

    if let Some(r) = result {
        glyph_cache.insert(key, r);
    }
    result
}

/// zsh 的输入行通常带 bold，而普通命令输出不带；中文回退字体在小字号
/// Regular 下笔画明显收窄，造成“输入正常、输出被压扁”的观感。宽字符的
/// 普通字重使用 Medium，英文仍保持所选等宽字体的原始 Regular。
fn glyph_weight(bold: bool, wide: bool) -> cosmic_text::Weight {
    if bold {
        cosmic_text::Weight::BOLD
    } else if wide {
        cosmic_text::Weight::MEDIUM
    } else {
        cosmic_text::Weight::NORMAL
    }
}

/// 低 DPI stem darkening 在字形首次进入 CPU 图集时完成，避免把 `pow`
/// 留在每个 GPU 片元上。端点保持不变，仅提升抗锯齿中间覆盖率。
fn darken_glyph_alpha(alpha: u8) -> u8 {
    if alpha == 0 || alpha == u8::MAX {
        return alpha;
    }
    ((f32::from(alpha) / 255.0).powf(0.72) * 255.0).round() as u8
}

impl iced_wgpu::primitive::Pipeline for TermPipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        build_pipeline(device, queue, format)
    }

    fn trim(&mut self) {}
}

/// 建立渲染管线（iced 首次遇到 TermPrimitive 时调用一次）。
fn build_pipeline(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    format: wgpu::TextureFormat,
) -> TermPipeline {
    const ATLAS: u32 = 512;

    // 字体系统：移除系统位图 CJK 字体（无 glyf/cff 轮廓，swash 无法光栅化）
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let bitmap_ids: Vec<_> = db
        .faces()
        .filter(|f| f.post_script_name.contains("Bitmap"))
        .map(|f| f.id)
        .collect();
    for id in bitmap_ids {
        db.remove_face(id);
    }
    // 字体族显式指定（产品决策，不依赖 fallback 顺序）：
    // VIDA_FONT_FAMILY 覆盖，默认 Menlo；缺失时按候选回落并 warn。
    // 必须在 db move 进 FontSystem 之前解析。
    let requested = std::env::var("VIDA_FONT_FAMILY").unwrap_or_else(|_| "Menlo".to_string());
    let family = resolve_font_family(&db, &requested);
    let mut font_system = FontSystem::new_with_locale_and_db("zh-Hans".into(), db);
    // 初始 scale=1.0；首次 prepare 时用真实 viewport scale 重新测量
    // （见 apply_scale_change）。字号可经 VIDA_FONT_SIZE 覆盖（对照实验用）。
    let font_size = std::env::var("VIDA_FONT_SIZE")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|v| *v >= 8.0 && *v <= 48.0)
        .unwrap_or(DEFAULT_FONT_SIZE);
    let appearance = TerminalAppearance {
        font_family: requested.clone(),
        font_size,
        cursor_blink: true,
    }
    .normalized();
    if family != requested {
        tracing::warn!(
            "字体 {} 不可用，回落为 {}（候选: Menlo → SF Mono → Monaco → 默认）",
            requested,
            family
        );
    }
    tracing::info!("终端字体: {}（请求 {}）", family, requested);
    let metrics = measure_font(&mut font_system, 1.0, font_size, &family);
    log_face_names(&mut font_system, font_size, &family);

    tracing::info!("终端渲染 surface format: {:?}", format);
    if format.is_srgb() {
        tracing::info!(
            "surface 为 sRGB 格式：着色器输出将按 linear→sRGB 编码，             颜色需在 fs_main 中先转 linear"
        );
    }
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("term shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("term_grid.wgsl").into()),
    });
    let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("term screen uniform"),
        size: 16, // vec2(size) + f32(gamma) + f32(cursor_on)
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("term uniform bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("term uniform bg"),
        layout: &uniform_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: uniform_buffer.as_entire_binding(),
        }],
    });
    let atlas_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("term atlas bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("term layout"),
        bind_group_layouts: &[&uniform_bgl, &atlas_bgl],
        push_constant_ranges: &[],
    });
    let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("term pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![
                    0 => Float32x2, // xy
                    1 => Float32x2, // uv
                    2 => Float32x4, // color
                ],
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });
    let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("term atlas"),
        size: wgpu::Extent3d {
            width: ATLAS_SIZE,
            height: ATLAS_SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("term atlas sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });
    let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("term atlas bg"),
        layout: &atlas_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&atlas_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&atlas_sampler),
            },
        ],
    });
    let empty_vbuf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("term empty vbuf"),
        size: 1,
        usage: wgpu::BufferUsages::VERTEX,
        mapped_at_creation: false,
    });
    let empty_ibuf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("term empty ibuf"),
        size: 1,
        usage: wgpu::BufferUsages::INDEX,
        mapped_at_creation: false,
    });

    // 初始图集：全透明 + (0,0) 白像素（背景 quad 采样）
    let mut atlas_data = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize];
    atlas_data[0] = 255;
    atlas_data[1] = 255;
    atlas_data[2] = 255;
    atlas_data[3] = 255;
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &atlas_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &atlas_data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(ATLAS_SIZE * 4),
            rows_per_image: Some(ATLAS_SIZE),
        },
        wgpu::Extent3d {
            width: ATLAS,
            height: ATLAS,
            depth_or_array_layers: 1,
        },
    );

    // 启动自检：模拟一次「字形级增量上传」——把 (0,0) 的 1×1 区域所在行
    // 通过 upload_atlas_rows 再传一次。若 write_texture 的 layout 参数有误
    // （行距/对齐/源缓冲大小不匹配），wgpu 校验层会立即 panic——
    // 在启动时暴露，而不是等用户点击终端时崩溃。
    // 注意：这依赖 wgpu 校验错误的 panic 行为（非 Result），见 commit 说明。
    {
        let mut probe = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize];
        probe[3] = 255; // (0,0) 白像素 alpha（与初始图集一致）
        upload_atlas_rows(&atlas_texture, queue, &probe, 0, 1);
    }

    TermPipeline {
        font_system: Mutex::new(font_system),
        metrics,
        render_pipeline,
        uniform_buffer,
        uniform_bind_group,
        atlas_texture,
        atlas_bind_group,
        vertex_buffer: empty_vbuf,
        index_buffer: empty_ibuf,
        vertex_count: 0,
        glyph_cache: Mutex::new(std::collections::HashMap::new()),
        built_version: AtomicU64::new(u64::MAX),
        atlas_data: Mutex::new(atlas_data),
        atlas_cursor: Mutex::new((1, 0, 0)),
        is_srgb: format.is_srgb(),
        diag_printed: std::sync::atomic::AtomicU8::new(0),
        font_family: family,
        appearance,
        cursor_on: true,
    }
}

/// 解析实际字体族：请求的字体不可用时按候选回落。
/// 候选顺序：请求值 → Menlo → SF Mono → Monaco → 默认 monospace。
fn resolve_font_family(db: &fontdb::Database, requested: &str) -> String {
    let candidates = [requested, "Menlo", "SF Mono", "Monaco"];
    for name in candidates {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(name)],
            ..Default::default()
        };
        if db.query(&query).is_some() {
            return name.to_string();
        }
    }
    "Monospace".to_string()
}

/// 打印 'A' 与 '你' 实际使用的字体名（启动日志，便于一眼确认）。
fn log_face_names(font_system: &mut FontSystem, font_size: f32, family: &str) {
    for ch in ['A', '你'] {
        let metrics = Metrics::new(font_size, font_size * 1.25);
        let mut buf = Buffer::new_empty(metrics);
        buf.set_size(font_system, Some(100.0), None);
        let attrs = Attrs::new()
            .family(cosmic_text::Family::Name(family))
            .weight(glyph_weight(false, ch == '你'));
        let text: String = ch.to_string();
        buf.set_text(font_system, &text, &attrs, Shaping::Advanced, None);
        for line in buf.layout_runs() {
            for glyph in line.glyphs {
                let physical = glyph.physical((0.0, 0.0), 1.0);
                let name = font_system
                    .db_mut()
                    .face(physical.cache_key.font_id)
                    .map(|f| f.post_script_name.clone())
                    .unwrap_or_else(|| "未知".to_string());
                tracing::info!("终端字形 '{}' 使用字体: {}", ch, name);
            }
        }
    }
}

/// 用 cosmic-text 实测 'M' 的 advance width、行高和 ascent。
/// 度量失败时回退到常用等宽值（显式 match，不用 unwrap_or_default）。
/// 按给定 scale 测量字体（物理像素值）。
/// 方式 B：Metrics 字号 × scale（而非 physical() 的 scale 参数）——
/// 所有测量值（advance/ascent/位图尺寸）出自同一来源同一单位，
/// 无手工换算；physical() 的 scale 参数与字号分离时容易漏乘。
///
/// **cell 尺寸必须取整为整数物理像素**：真实终端（Alacritty/
/// WezTerm/iTerm）都是整数 cell。advance 实测 19.2 若直接用，每列
/// x = 0/19.2/38.4/57.6... 落在非整数像素边界，quad 覆盖半个物理
/// 像素 → 字形边缘被重新采样 → 笔画发虚，且每列小数部分不同，
/// 整行参差不齐。round() 最接近真实 advance，列累计误差最小。
fn measure_font(
    font_system: &mut FontSystem,
    scale: f32,
    font_size: f32,
    family: &str,
) -> FontMetrics {
    // 行高 = 字号 × 1.25（16→20 的既有比例）
    let line_height = font_size * 1.25;
    let metrics = Metrics::new(font_size * scale, line_height * scale);
    let mut buf = Buffer::new_empty(metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Name(family));
    buf.set_size(font_system, Some(100.0), None);
    buf.set_text(font_system, "M", &attrs, Shaping::Advanced, None);
    let fallback = FontMetrics {
        cell_width: (9.6 * scale).round().max(1.0),
        cell_height: (line_height * scale).round().max(1.0),
        ascent: (14.26 * scale).round(),
        scale,
        font_size,
    };
    let Some(layout) = buf.layout_runs().next() else {
        tracing::warn!("字体度量失败，使用默认值");
        return fallback;
    };
    FontMetrics {
        cell_width: layout.line_w.round().max(1.0),
        cell_height: metrics.line_height.round().max(1.0),
        ascent: (layout.line_y - layout.line_top).round(),
        scale,
        font_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 upload_atlas_rows 的 layout 参数（行距 2048、对齐、源缓冲大小）
    /// 在真实 wgpu 校验层可通过。参数错误会 panic，测试失败即暴露。
    /// 用 headless 实例（无窗口），与 GUI 用同一 wgpu 版本（27）。
    #[test]
    fn atlas_row_upload_passes_validation() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("应能获取 adapter（headless）");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("atlas upload test"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .expect("应能获取 device");

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let mut atlas = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize];
        atlas[3] = 255;

        // 单行区间（自检路径）
        upload_atlas_rows(&texture, &queue, &atlas, 0, 1);
        // 多行区间（首帧字形打包后通常跨多行）
        upload_atlas_rows(&texture, &queue, &atlas, 3, 7);
        queue.submit([]);
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(2)),
            })
            .expect("提交应成功");
    }

    /// 合并逻辑：重叠/相邻的脏区域应合并为同一行区间。
    #[test]
    fn merge_row_ranges_merges_overlap_and_adjacent() {
        let dirty = vec![(1u32, 4u32, 8u32, 2u32), (5, 5, 8, 1), (9, 10, 8, 1)];
        let merged = merge_row_ranges(&dirty);
        // [4,6) 与 [5,6) 合并 → [4,6)；[10,11) 独立
        assert_eq!(merged, vec![(4, 2), (10, 1)]);
    }

    /// 空输入返回空。
    #[test]
    fn merge_row_ranges_empty() {
        assert!(merge_row_ranges(&[]).is_empty());
    }

    #[test]
    fn stem_darkening_preserves_endpoints_and_strengthens_edges() {
        assert_eq!(darken_glyph_alpha(0), 0);
        assert_eq!(darken_glyph_alpha(255), 255);
        assert!(darken_glyph_alpha(64) > 64);
        assert!(darken_glyph_alpha(128) > 128);
    }

    #[test]
    fn regular_wide_glyph_uses_medium_weight() {
        assert_eq!(glyph_weight(false, false), cosmic_text::Weight::NORMAL);
        assert_eq!(glyph_weight(false, true), cosmic_text::Weight::MEDIUM);
        assert_eq!(glyph_weight(true, true), cosmic_text::Weight::BOLD);
    }

    #[test]
    fn quad_indices_address_each_vertex_once_per_quad() {
        assert_eq!(quad_indices(8), vec![0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7]);
    }

    /// 图集 cursor 跨帧持久：两次 build_geometry（第二帧含新字符）后，
    /// 第一帧写入的字形不得被覆盖。
    #[test]
    fn atlas_cursor_must_persist_across_frames() {
        let (device, queue) = headless_device();
        let mut pipeline = build_pipeline(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);

        // 快照 1：'A'
        let mut grid_a = ClientGrid::new(1, 1);
        grid_a.apply_frame(&frame_with('A'));
        let p1 = TermPrimitive::new(
            Arc::new(grid_a),
            Rectangle::default(),
            Arc::new(ViewportMetrics::default()),
        );
        let first_geometry = p1
            .build_geometry(&mut pipeline, 0.0, 0.0)
            .expect("首帧几何应成功");
        let &(a_x, a_y, a_w, a_h) = first_geometry.dirty_regions.first().expect("A 应写入图集");
        let first_atlas = pipeline
            .atlas_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let first_a = atlas_region(&first_atlas, a_x, a_y, a_w, a_h);

        // 快照 2：新字符 '你'（第二帧）
        let mut grid_b = ClientGrid::new(1, 1);
        grid_b.apply_frame(&frame_with('你'));
        let p2 = TermPrimitive::new(
            Arc::new(grid_b),
            Rectangle::default(),
            Arc::new(ViewportMetrics::default()),
        );
        let _ = p2.build_geometry(&mut pipeline, 0.0, 0.0);
        let after_atlas = pipeline
            .atlas_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let after = atlas_region(&after_atlas, a_x, a_y, a_w, a_h);

        assert_eq!(
            first_a, after,
            "'A' 的图集区域被第二帧的新字形覆盖（cursor 未跨帧持久）"
        );
    }

    fn atlas_region(atlas: &[u8], x: u32, y: u32, width: u32, height: u32) -> Vec<u8> {
        let mut region = Vec::with_capacity((width * height * 4) as usize);
        for row in y..y + height {
            let start = ((row * ATLAS_SIZE + x) * 4) as usize;
            let end = start + (width * 4) as usize;
            region.extend_from_slice(&atlas[start..end]);
        }
        region
    }

    /// 构造一个全量帧：单行单列放指定字符。
    fn frame_with(ch: char) -> crate::term::frame::TerminalFrame {
        crate::term::frame::TerminalFrame {
            seq: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            lines: vec![crate::term::frame::LineUpdate {
                row: 0,
                start_col: 0,
                end_col: 0,
                runs: vec![crate::term::frame::Run {
                    len: 1,
                    flags: 0,
                    fg: ColorSpec::Default,
                    bg: ColorSpec::Default,
                    ch,
                }],
            }],
        }
    }

    /// 可见光标必须真的进入渲染几何；协议字段存在但 primitive 不消费时，
    /// 所有输入虽正常，用户却完全看不到当前位置。
    #[test]
    fn visible_cursor_builds_shader_controlled_overlay() {
        let (device, queue) = headless_device();
        let mut pipeline = build_pipeline(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);
        let mut grid = ClientGrid::new(2, 3);
        grid.cursor_row = 1;
        grid.cursor_col = 2;
        grid.cursor_visible = true;
        let primitive = TermPrimitive::new(
            Arc::new(grid),
            Rectangle::default(),
            Arc::new(ViewportMetrics::default()),
        );

        let geometry = primitive
            .build_geometry(&mut pipeline, 0.0, 0.0)
            .expect("光标几何应成功");
        assert_eq!(geometry.quads.len(), 4, "空白屏应只绘制一个光标 quad");

        let vertices = &geometry.quads;
        let metrics = pipeline.metrics;
        assert_eq!(
            vertices[0].xy,
            [metrics.cell_width * 2.0, metrics.cell_height]
        );
        assert_eq!(
            vertices[2].xy,
            [metrics.cell_width * 3.0, metrics.cell_height * 2.0]
        );
        assert_eq!(vertices[0].color, [1.0, 1.0, 1.0, -1.0]);

        let cursor_off = TermPrimitive::with_appearance(
            primitive.snapshot.clone(),
            Rectangle::default(),
            Arc::new(ViewportMetrics::default()),
            TerminalAppearance::default(),
            false,
        )
        .build_geometry(&mut pipeline, 0.0, 0.0)
        .expect("暗相位几何应成功");
        assert_eq!(
            cursor_off.quads.len(),
            geometry.quads.len(),
            "闪烁相位不能重建不同几何；可见性只由 uniform 控制"
        );
    }

    /// 字号对照实验：16/18/20/22 下 cell 尺寸与 'A' 实际字体名。
    /// 判定：若字号增大观感改善 → 非渲染 bug，是 16px 的固有效果；
    /// 若各字号同样发虚 → 渲染问题。
    #[test]
    fn diagnostic_font_sizes_and_face() {
        let (device, queue) = headless_device();
        let mut pipeline = build_pipeline(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);
        for size in [16.0f32, 18.0, 20.0, 22.0] {
            pipeline.metrics = measure_font_at_size(&pipeline, 1.0, size);
            let metrics = pipeline.metrics;
            let mut font_system = pipeline
                .font_system
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut cache = SwashCache::new();
            let mut glyph_cache: std::collections::HashMap<(char, u16, u32), GlyphEntry> =
                std::collections::HashMap::new();
            let mut state = AtlasState {
                data: vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize],
                next_x: 1,
                next_y: 0,
                row_h: 0,
                dirty: Vec::new(),
                scale: 1.0,
                font_size: size,
                font_family: pipeline.font_family.clone(),
            };
            for ch in ['A', '你'] {
                let r = rasterize_char(
                    &mut font_system,
                    &mut cache,
                    &mut glyph_cache,
                    &mut state,
                    ATLAS_SIZE,
                    ch,
                    glyph_weight(false, ch == '你'),
                );
                // 字体名：从 cache_key 的 font_id 查
                let face_name = rasterize_face_name(
                    &mut font_system,
                    ch,
                    false,
                    ch == '你',
                    size,
                    &pipeline.font_family,
                );
                match r {
                    Some((gw, gh, ..)) => {
                        eprintln!(
                            "[diag] 字号 {}: cell={}x{} asc={} '{}'位图={}x{} 字体={}",
                            size,
                            metrics.cell_width,
                            metrics.cell_height,
                            metrics.ascent,
                            ch,
                            gw,
                            gh,
                            face_name
                        );
                    }
                    None => eprintln!("[diag] 字号 {}: '{}' 光栅化失败", size, ch),
                }
            }
        }
    }

    /// 查 'A' 光栅化时实际使用的字体名（post script name）。
    fn rasterize_face_name(
        font_system: &mut FontSystem,
        ch: char,
        bold: bool,
        wide: bool,
        font_size: f32,
        family: &str,
    ) -> String {
        let metrics = Metrics::new(font_size, font_size * 1.25);
        let mut buf = Buffer::new_empty(metrics);
        buf.set_size(font_system, Some(100.0), None);
        let mut attrs = Attrs::new().family(cosmic_text::Family::Name(family));
        attrs = attrs.weight(glyph_weight(bold, wide));
        let text: String = ch.to_string();
        buf.set_text(font_system, &text, &attrs, Shaping::Advanced, None);
        let mut name = String::new();
        for line in buf.layout_runs() {
            for glyph in line.glyphs {
                let physical = glyph.physical((0.0, 0.0), 1.0);
                name = font_system
                    .db_mut()
                    .face(physical.cache_key.font_id)
                    .map(|f| f.post_script_name.clone())
                    .unwrap_or_else(|| "未知".to_string());
            }
        }
        name
    }

    /// 用指定字号测量（诊断用）。
    fn measure_font_at_size(pipeline: &TermPipeline, scale: f32, font_size: f32) -> FontMetrics {
        let mut font_system = pipeline
            .font_system
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        measure_font(&mut font_system, scale, font_size, &pipeline.font_family)
    }

    /// 测量第一步：'A' 在 scale=1 与 scale=2 下的实际位图尺寸。
    /// gw≈18-20 表示 2x 光栅化生效；gw≈9-10 表示仍是 1x。
    #[test]
    fn diagnostic_glyph_a_size() {
        let (device, queue) = headless_device();
        let mut pipeline = build_pipeline(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);
        for scale in [1.0f32, 2.0f32] {
            pipeline.metrics = measure_font_at(&pipeline, scale);
            let metrics = pipeline.metrics;
            let mut font_system = pipeline
                .font_system
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut cache = SwashCache::new();
            let mut glyph_cache: std::collections::HashMap<(char, u16, u32), GlyphEntry> =
                std::collections::HashMap::new();
            let mut state = AtlasState {
                data: vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize],
                next_x: 1,
                next_y: 0,
                row_h: 0,
                dirty: Vec::new(),
                scale,
                font_size: 16.0,
                font_family: pipeline.font_family.clone(),
            };
            let r = rasterize_char(
                &mut font_system,
                &mut cache,
                &mut glyph_cache,
                &mut state,
                ATLAS_SIZE,
                'A',
                glyph_weight(false, false),
            );
            match r {
                Some((gw, gh, ..)) => {
                    eprintln!(
                        "[diag] 'A' scale={}: gw={} gh={} cell={}x{} ascent={}",
                        scale, gw, gh, metrics.cell_width, metrics.cell_height, metrics.ascent
                    );
                }
                None => eprintln!("[diag] 'A' scale={}: 光栅化失败", scale),
            }
        }
    }

    /// 评估 scale=2（Retina）下图集占用率：光栅化典型终端字符集
    /// （95 ASCII + 常用中文 + 常见符号），报告占用比例。
    /// 2x 后字形面积 4 倍，512×512 可能不够——据此决定是否扩到 1024。
    #[test]
    fn atlas_usage_at_scale_2() {
        let (device, queue) = headless_device();
        let mut pipeline = build_pipeline(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);
        let scale = 2.0f32;
        pipeline.metrics = measure_font_at(&pipeline, scale);
        let mut font_system = pipeline
            .font_system
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut cache = SwashCache::new();
        let mut glyph_cache: std::collections::HashMap<(char, u16, u32), GlyphEntry> =
            std::collections::HashMap::new();
        let mut state = AtlasState {
            data: vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize],
            next_x: 1,
            next_y: 0,
            row_h: 0,
            dirty: Vec::new(),
            scale,
            font_size: 16.0,
            font_family: pipeline.font_family.clone(),
        };

        // 典型字符集：95 个 ASCII + 常用中文 + 符号
        let mut chars: Vec<char> = (32u8..127).map(|c| c as char).collect();
        chars.extend("你好世界数据库备份生产服务器测试环境开发部署运维监控日志配置密码密钥网络主机集群容器镜像版本更新删除创建修改查询添加移除管理权限用户组角色策略规则异常错误警告信息成功失败正在等待中".chars());
        chars.extend("→←↑↓✓✗●○■□▲△★☆═║╔╗╚╝│─┌┐└┘·…！？，。；：、（）【】《》".chars());
        chars.dedup();

        let mut skipped = 0usize;
        for ch in &chars {
            let weight = glyph_weight(false, !ch.is_ascii());
            let key = (*ch, weight.0, scale.to_bits());
            if glyph_cache.contains_key(&key) {
                continue;
            }
            let r = rasterize_char(
                &mut font_system,
                &mut cache,
                &mut glyph_cache,
                &mut state,
                ATLAS_SIZE,
                *ch,
                weight,
            );
            if r.is_none() {
                skipped += 1;
            }
        }

        let used_rows = state.next_y + state.row_h;
        let usage = (used_rows as f64) / (ATLAS_SIZE as f64) * 100.0;
        eprintln!(
            "[atlas-usage] scale={} 字符={} 跳过={} 图集使用到行 {} / {} = {:.1}%",
            scale,
            chars.len(),
            skipped,
            used_rows,
            ATLAS_SIZE,
            usage
        );
        assert!(
            usage < 100.0,
            "scale=2 下图集溢出（使用 {:.1}%），需要扩大 ATLAS_SIZE",
            usage
        );
    }

    /// 所有 quad 顶点坐标必须是整数物理像素（小数边界 = 发虚）。
    #[test]
    fn quad_coordinates_are_integer_pixels() {
        let (device, queue) = headless_device();
        let mut pipeline = build_pipeline(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);
        // 2x scale（Retina）
        pipeline.metrics = measure_font_at(&pipeline, 2.0);

        // 含中文/属性/背景的 grid
        let mut grid = ClientGrid::new(4, 8);
        grid.apply_frame(&crate::term::frame::TerminalFrame {
            seq: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            lines: vec![
                crate::term::frame::LineUpdate {
                    row: 0,
                    start_col: 0,
                    end_col: 1,
                    runs: vec![crate::term::frame::Run {
                        len: 2,
                        flags: 0x40, // WIDE
                        fg: ColorSpec::Default,
                        bg: ColorSpec::Default,
                        ch: '你',
                    }],
                },
                crate::term::frame::LineUpdate {
                    row: 1,
                    start_col: 0,
                    end_col: 0,
                    runs: vec![crate::term::frame::Run {
                        len: 1,
                        flags: 0x01 | 0x04, // bold + underline
                        fg: ColorSpec::Rgb(255, 0, 0),
                        bg: ColorSpec::Rgb(0, 0, 255),
                        ch: 'A',
                    }],
                },
            ],
        });
        let prim = TermPrimitive::new(
            Arc::new(grid),
            Rectangle::default(),
            Arc::new(ViewportMetrics::default()),
        );
        let geom = prim
            .build_geometry(&mut pipeline, 10.7, 5.3)
            .expect("build_geometry 应成功");
        assert!(!geom.quads.is_empty(), "应有 quads");
        assert!(
            geom.quads[1].xy[0] - geom.quads[0].xy[0] < pipeline.metrics.cell_width * 2.0,
            "WIDE 字形应保持字体原始宽高比，不能强行拉伸到两个 cell"
        );

        for v in &geom.quads {
            for coord in v.xy {
                assert!(
                    (coord - coord.round()).abs() < 1e-4,
                    "quad 顶点坐标必须是整数: {}",
                    coord
                );
            }
        }
    }

    fn headless_device() -> (wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("adapter");
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("atlas cursor test"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .expect("device")
    }
}
