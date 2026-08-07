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
use cosmic_text::{
    Attrs, Buffer, FontSystem, Metrics, Shaping, SwashCache,
};
use iced::Rectangle;
use iced_graphics::Viewport;
use iced_wgpu::Primitive as IcedPrimitive;

use super::client_grid::{cell_flags, ClientGrid, ColorSpec};
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
#[derive(Clone, Copy)]
struct FontMetrics {
    cell_width: f32,
    cell_height: f32,
    /// 主字体 ascent（基线到顶部距离），全局统一基线。
    ascent: f32,
}

/// 终端默认配色（主题色）。
const DEFAULT_FG: (u8, u8, u8) = (200, 200, 200);
const DEFAULT_BG: (u8, u8, u8) = (13, 13, 20);

/// 字形缓存条目：位图在图集中的 (宽, 高, u0, v0, u1, v1, baseline偏移, placement.top)。
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
}

impl TermPrimitive {
    pub fn new(snapshot: Arc<ClientGrid>, bounds: Rectangle) -> Self {
        Self { snapshot, bounds }
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
    /// 字形缓存：跨帧复用，同字符同粗体只光栅化一次。
    glyph_cache: Mutex<std::collections::HashMap<(char, bool), GlyphEntry>>,
    /// 已构建的 grid 版本号：prepare 里 O(1) 跳过未变化的帧。
    built_version: AtomicU64,
    /// 图集持久数据（增量上传用）。
    atlas_data: Mutex<Vec<u8>>,
    /// 图集打包 cursor (next_x, next_y, row_h)——必须跨帧持久：
    /// 每帧重置会让新字形覆盖已写入的字形（ASCII 靠前最易被覆盖）。
    atlas_cursor: Mutex<(u32, u32, u32)>,
}

impl IcedPrimitive for TermPrimitive {
    type Pipeline = TermPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        let version = self.snapshot.version;
        // O(1) 跳过：grid 未变化不重建任何东西
        if pipeline.built_version.load(Ordering::Relaxed) == version {
            return;
        }

        let scale = viewport.scale_factor();
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

        // 上传 screen size 到 uniform（物理像素）
        let phys = viewport.physical_size();
        queue.write_buffer(
            &pipeline.uniform_buffer,
            0,
            bytemuck::bytes_of(&[phys.width as f32, phys.height as f32]),
        );

        // 上传 quads
        if geometry.quads.is_empty() {
            pipeline.vertex_count = 0;
            pipeline.built_version.store(version, Ordering::Relaxed);
            return;
        }
        // 索引用 u16：quads 超过上限（65535/4≈16383 个 quad）时截断并告警，
        // 防止索引截断后 draw 越界触发 wgpu 校验 panic（连锁 abort）。
        // 正常终端（200×50）远低于此，截断仅作防御。
        let quad_limit = (u16::MAX as usize / 4) * 4;
        let quad_count = geometry.quads.len().min(quad_limit);
        if quad_count < geometry.quads.len() {
            tracing::warn!(
                "终端 quads 超出 u16 索引上限：{} → 截断为 {}",
                geometry.quads.len(),
                quad_count
            );
        }
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("term quads"),
            size: (quad_count * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &vbuf,
            0,
            bytemuck::cast_slice(&geometry.quads[..quad_count]),
        );
        pipeline.vertex_buffer = vbuf;

        let idx: Vec<u16> = (0..quad_count as u16)
            .flat_map(|i| [i * 4, i * 4 + 1, i * 4 + 2, i * 4, i * 4 + 2, i * 4 + 3])
            .collect();
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

    fn draw(
        &self,
        pipeline: &Self::Pipeline,
        render_pass: &mut wgpu::RenderPass<'_>,
    ) -> bool {
        if pipeline.vertex_count == 0 {
            return true;
        }
        render_pass.set_pipeline(&pipeline.render_pipeline);
        render_pass.set_bind_group(0, &pipeline.uniform_bind_group, &[]);
        render_pass.set_bind_group(1, &pipeline.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, pipeline.vertex_buffer.slice(..));
        render_pass.set_index_buffer(
            pipeline.index_buffer.slice(..),
            wgpu::IndexFormat::Uint16,
        );
        render_pass.draw_indexed(0..pipeline.vertex_count, 0, 0..1);
        true
    }
}

/// 图集尺寸（行距 512×4=2048 字节，天然满足 wgpu 256 字节对齐）。
pub const ATLAS_SIZE: u32 = 512;

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
            origin: wgpu::Origin3d {
                x: 0,
                y,
                z: 0,
            },
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

/// 把字形级脏区域（x,y,w,h）合并成行区间（y, h），重叠/相邻的合并。
fn merge_row_ranges(dirty: &[(u32, u32, u32, u32)]) -> Vec<(u32, u32)> {
    if dirty.is_empty() {
        return Vec::new();
    }
    let mut ranges: Vec<(u32, u32)> = dirty
        .iter()
        .map(|(_, y, _, h)| (*y, *y + *h))
        .collect();
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
        const ATLAS: u32 = 512;

        let mut glyph_cache = match pipeline.glyph_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut atlas = match pipeline.atlas_data.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if atlas.len() != (ATLAS * ATLAS * 4) as usize {
            atlas = vec![0u8; (ATLAS * ATLAS * 4) as usize];
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
        let mut atlas_state = AtlasState {
            data: atlas,
            next_x,
            next_y,
            row_h,
            dirty: Vec::new(),
        };

        let mut font_system = match pipeline.font_system.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut cache = SwashCache::new();
        let metrics = pipeline.metrics;
        let cw = metrics.cell_width;
        let ch = metrics.cell_height;
        let grid = &self.snapshot;
        let mut quads: Vec<Vertex> = Vec::new();

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

                // 背景 quad：非默认背景或反色（反色时背景 = 前景色）
                let bg = resolve_bg(cell);
                if cell.bg != ColorSpec::Default || cell.flags & frame::flag::REVERSE != 0 {
                    push_quad(
                        &mut quads,
                        [cell_x, row_y, cell_x + cw * col_width as f32, row_y + ch],
                        [0.0, 0.0, 0.0, 0.0],
                        [
                            bg.0 as f32 / 255.0,
                            bg.1 as f32 / 255.0,
                            bg.2 as f32 / 255.0,
                            1.0,
                        ],
                    );
                }

                // 字形：spacer 不画（其前半 cell 已画宽字符）
                if !is_spacer {
                    let bold = cell.flags & frame::flag::BOLD != 0;
                    let glyph = rasterize_char(
                        &mut font_system,
                        &mut cache,
                        &mut glyph_cache,
                        &mut atlas_state,
                        ATLAS,
                        cell.ch,
                        bold,
                    );
                    if let Some((gw, gh, u0, v0, u1, v1, _, top)) = glyph {
                        let fg = if cell.flags & frame::flag::REVERSE != 0 {
                            [0.0, 0.0, 0.0, 1.0]
                        } else {
                            let c = resolve_fg(cell);
                            [
                                c.0 as f32 / 255.0,
                                c.1 as f32 / 255.0,
                                c.2 as f32 / 255.0,
                                1.0,
                            ]
                        };
                        // 统一基线：glyph_y = row_y + ascent - placement.top
                        let y = row_y + metrics.ascent - top;
                        push_quad(
                            &mut quads,
                            [cell_x, y, cell_x + gw as f32, y + gh as f32],
                            [u0, v0, u1, v1],
                            fg,
                        );
                    }
                }

                // 下划线：基线下方 2px，宽度 = cell 列宽
                if cell.flags & frame::flag::UNDERLINE != 0 {
                    let uy = row_y + metrics.ascent + 2.0;
                    let lc = if cell.flags & frame::flag::REVERSE != 0 {
                        [0.0, 0.0, 0.0, 1.0]
                    } else {
                        let c = resolve_fg(cell);
                        [
                            c.0 as f32 / 255.0,
                            c.1 as f32 / 255.0,
                            c.2 as f32 / 255.0,
                            1.0,
                        ]
                    };
                    push_quad(
                        &mut quads,
                        [cell_x, uy, cell_x + cw * col_width as f32, uy + 1.5],
                        [0.0, 0.0, 0.0, 0.0],
                        lc,
                    );
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

/// 推入一个 quad（xy0, xy1 对角，uv0, uv1 对角）。
fn push_quad(
    quads: &mut Vec<Vertex>,
    xy: [f32; 4], // x0, y0, x1, y1
    uv: [f32; 4], // u0, v0, u1, v1
    color: [f32; 4],
) {
    let (x0, y0, x1, y1) = (xy[0], xy[1], xy[2], xy[3]);
    let (u0, v0, u1, v1) = (uv[0], uv[1], uv[2], uv[3]);
    quads.push(Vertex { xy: [x0, y0], uv: [u0, v0], color });
    quads.push(Vertex { xy: [x1, y0], uv: [u1, v0], color });
    quads.push(Vertex { xy: [x1, y1], uv: [u1, v1], color });
    quads.push(Vertex { xy: [x0, y1], uv: [u0, v1], color });
}

/// 图集打包状态（跨帧持久的部分由调用方保存/恢复）。
struct AtlasState {
    data: Vec<u8>,
    next_x: u32,
    next_y: u32,
    row_h: u32,
    /// 本帧新增字形区域（字形级，上传前合并为行区间）。
    dirty: Vec<(u32, u32, u32, u32)>,
}

/// 光栅化单个字符到图集（带缓存）。返回 (宽, 高, u0, v0, u1, v1, baseline, top)。
fn rasterize_char(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    glyph_cache: &mut std::collections::HashMap<(char, bool), GlyphEntry>,
    state: &mut AtlasState,
    atlas_size: u32,
    ch: char,
    bold: bool,
) -> Option<GlyphEntry> {
    let key = (ch, bold);
    if let Some(cached) = glyph_cache.get(&key) {
        return Some(*cached);
    }
    let atlas = &mut state.data;
    let next_x = &mut state.next_x;
    let next_y = &mut state.next_y;
    let row_h = &mut state.row_h;
    let dirty = &mut state.dirty;

    let mut buf = Buffer::new_empty(Metrics::new(16.0, 20.0));
    buf.set_size(font_system, Some(100.0), None);
    let mut attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    if bold {
        attrs = attrs.weight(cosmic_text::Weight::BOLD);
    }
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
            for py in 0..gh {
                for px in 0..gw {
                    let src = (py * gw + px) as usize;
                    let a = img.data[src.min(img.data.len().saturating_sub(1))];
                    let idx = (((*next_y + py) * atlas_size + (*next_x + px)) as usize) * 4;
                    atlas[idx] = 255;
                    atlas[idx + 1] = 255;
                    atlas[idx + 2] = 255;
                    atlas[idx + 3] = a;
                }
            }
            let u0 = *next_x as f32 / atlas_size as f32;
            let v0 = *next_y as f32 / atlas_size as f32;
            let u1 = (*next_x + gw) as f32 / atlas_size as f32;
            let v1 = (*next_y + gh) as f32 / atlas_size as f32;
            let entry = (
                gw, gh, u0, v0, u1, v1,
                physical.y as f32, img.placement.top as f32,
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

impl iced_wgpu::primitive::Pipeline for TermPipeline {
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) -> Self {
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
    let mut font_system = FontSystem::new_with_locale_and_db("zh-Hans".into(), db);
    let metrics = measure_font(&mut font_system);

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("term shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("term_grid.wgsl").into()),
    });
    let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("term screen uniform"),
        size: 8,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("term uniform bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
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
            width: ATLAS,
            height: ATLAS,
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
    let mut atlas_data = vec![0u8; (ATLAS * ATLAS * 4) as usize];
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
            bytes_per_row: Some(ATLAS * 4),
            rows_per_image: Some(ATLAS),
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
    }
}

/// 用 cosmic-text 实测 'M' 的 advance width、行高和 ascent。
/// 度量失败时回退到常用等宽值（显式 match，不用 unwrap_or_default）。
fn measure_font(font_system: &mut FontSystem) -> FontMetrics {
    let metrics = Metrics::new(16.0, 20.0);
    let mut buf = Buffer::new_empty(metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buf.set_size(font_system, Some(100.0), None);
    buf.set_text(font_system, "M", &attrs, Shaping::Advanced, None);
    let fallback = FontMetrics {
        cell_width: 9.6,
        cell_height: 20.0,
        ascent: 14.26,
    };
    let Some(layout) = buf.layout_runs().next() else {
        tracing::warn!("字体度量失败，使用默认值");
        return fallback;
    };
    FontMetrics {
        cell_width: layout.line_w,
        cell_height: metrics.line_height,
        ascent: layout.line_y - layout.line_top,
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

    /// 图集 cursor 跨帧持久：两次 build_geometry（第二帧含新字符）后，
    /// 第一帧写入的字形不得被覆盖。
    #[test]
    fn atlas_cursor_must_persist_across_frames() {
        let (device, queue) = headless_device();
        let mut pipeline = build_pipeline(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);

        // 快照 1：'A'
        let mut grid_a = ClientGrid::new(1, 1);
        grid_a.apply_frame(&frame_with('A'));
        let p1 = TermPrimitive::new(Arc::new(grid_a), Rectangle::default());
        let _ = p1.build_geometry(&mut pipeline, 0.0, 0.0);
        // 'A' 从 (1,0) 写入，宽度约 10px → 占 bytes [4, 44)
        let first_a = pipeline
            .atlas_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .into_iter()
            .skip(4)
            .take(40)
            .collect::<Vec<u8>>();

        // 快照 2：新字符 '你'（第二帧）
        let mut grid_b = ClientGrid::new(1, 1);
        grid_b.apply_frame(&frame_with('你'));
        let p2 = TermPrimitive::new(Arc::new(grid_b), Rectangle::default());
        let _ = p2.build_geometry(&mut pipeline, 0.0, 0.0);
        let after = pipeline
            .atlas_data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .into_iter()
            .skip(4)
            .take(40)
            .collect::<Vec<u8>>();

        assert_eq!(
            first_a, after,
            "'A' 的图集区域被第二帧的新字形覆盖（cursor 未跨帧持久）"
        );
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
