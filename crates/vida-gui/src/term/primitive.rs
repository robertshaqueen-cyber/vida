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

        let scale = viewport.scale_factor() as f32;
        let origin_x = self.bounds.x * scale;
        let origin_y = self.bounds.y * scale;

        let geometry = match self.build_geometry(pipeline, origin_x, origin_y) {
            Some(g) => g,
            None => return,
        };

        // 上传图集新增字形区域
        for (x, y, w, h) in &geometry.dirty_regions {
            let region_data = extract_region(&geometry.atlas, *x, *y, *w, *h);
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &pipeline.atlas_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: *x,
                        y: *y,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &region_data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some((*w * 4).max(256)),
                    rows_per_image: Some(*h),
                },
                wgpu::Extent3d {
                    width: *w,
                    height: *h,
                    depth_or_array_layers: 1,
                },
            );
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
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("term quads"),
            size: (geometry.quads.len() * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&vbuf, 0, bytemuck::cast_slice(&geometry.quads));
        pipeline.vertex_buffer = vbuf;

        let idx: Vec<u16> = (0..geometry.quads.len() as u16)
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

fn extract_region(atlas: &[u8], x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    const ATLAS: u32 = 512;
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for py in 0..h {
        let row_start = (((y + py) * ATLAS + x) as usize) * 4;
        out.extend_from_slice(&atlas[row_start..row_start + (w as usize) * 4]);
    }
    out
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
        let mut next_x: u32 = 1;
        let mut next_y: u32 = 0;
        let mut row_h: u32 = 0;
        let mut dirty_regions: Vec<(u32, u32, u32, u32)> = Vec::new();

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
                        &mut atlas,
                        &mut next_x,
                        &mut next_y,
                        &mut row_h,
                        ATLAS,
                        cell.ch,
                        bold,
                        &mut dirty_regions,
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

        // 持久化图集（下次 build 基于此继续累加字形）
        if let Ok(mut guard) = pipeline.atlas_data.lock() {
            *guard = atlas.clone();
        }

        Some(BuiltGeometry {
            atlas,
            dirty_regions,
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

/// 光栅化单个字符到图集（带缓存）。返回 (宽, 高, u0, v0, u1, v1, baseline, top)。
fn rasterize_char(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    glyph_cache: &mut std::collections::HashMap<(char, bool), GlyphEntry>,
    atlas: &mut Vec<u8>,
    next_x: &mut u32,
    next_y: &mut u32,
    row_h: &mut u32,
    atlas_size: u32,
    ch: char,
    bold: bool,
    dirty: &mut Vec<(u32, u32, u32, u32)>,
) -> Option<GlyphEntry> {
    let key = (ch, bold);
    if let Some(cached) = glyph_cache.get(&key) {
        return Some(*cached);
    }

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
