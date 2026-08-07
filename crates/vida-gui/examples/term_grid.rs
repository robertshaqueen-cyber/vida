//! M2b-0: 字体度量 + 静态网格渲染。
//!
//! 渲染硬编码 6 行网格，验证逐 cell 定位与中英文对齐。
//!
//! 核心约束（M2b-spec 3.1）：
//! - 每个 cell 的 x 坐标 = col * cell_width（显式计算）
//! - RLE run 可合并绘制，但每个 run 的起始 x = start_col * cell_width
//! - 不把一行拼成字符串交给文本引擎排版
//!
//! cell_width = 'M' 的 advance（cosmic-text 实测）
//! cell_height = ascent + descent + line_gap（实测）

#![allow(unexpected_cfgs)]

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use cosmic_text::{
    Attrs, Buffer, FontSystem, Metrics, Shaping, SwashCache,
};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

// ---------------------------------------------------------------------------
// 网格内容（M2b-spec 3.3）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Run {
    start_col: u16,
    ch: char,
    run_len: u16,
    fg: (u8, u8, u8),
    bg: Option<(u8, u8, u8)>,
    bold: bool,
    underline: bool,
    reverse: bool,
}

const fn run(start_col: u16, ch: char, run_len: u16) -> Run {
    Run {
        start_col,
        ch,
        run_len,
        fg: (200, 200, 200),
        bg: None,
        bold: false,
        underline: false,
        reverse: false,
    }
}

fn rows() -> Vec<Vec<Run>> {
    vec![
        // row 0: ASCII 基准线
        vec![
            run(0, ' ', 7),
            run(7, 'A', 26),
            run(33, '0', 10),
        ],
        // row 1: 中文对齐测试 你好世界abc你好（abc 起始第 8+8=16 列）
        vec![
            run(0, ' ', 8),
            run(8, '你', 2),
            run(10, '好', 2),
            run(12, '世', 2),
            run(14, '界', 2),
            run(16, 'a', 3),
            run(19, '你', 2),
            run(21, '好', 2),
        ],
        // row 2: 属性
        vec![
            run(0, ' ', 4),
            run(4, '正', 2),
            run(6, '常', 2),
            run(8, ' ', 2),
            Run { bold: true, ..run(10, '粗', 2) },
            Run { bold: true, ..run(12, '体', 2) },
            run(14, ' ', 2),
            Run { underline: true, ..run(16, '下', 2) },
            Run { underline: true, ..run(18, '划', 2) },
            Run { underline: true, ..run(20, '线', 2) },
            run(22, ' ', 2),
            Run { reverse: true, fg: (0, 0, 0), bg: Some((200, 200, 200)), ..run(24, '反', 2) },
            Run { reverse: true, fg: (0, 0, 0), bg: Some((200, 200, 200)), ..run(26, '色', 2) },
        ],
        // row 3: 颜色
        vec![
            run(0, ' ', 4),
            Run { fg: (255, 60, 60), ..run(4, '红', 2) },
            run(6, ' ', 2),
            Run { fg: (60, 255, 60), ..run(8, '绿', 2) },
            run(10, ' ', 2),
            Run { fg: (60, 60, 255), ..run(12, '蓝', 2) },
            run(14, ' ', 2),
            run(16, '默', 2),
            run(18, '认', 2),
            run(20, ' ', 2),
            Run { bg: Some((80, 80, 160)), ..run(22, '背', 2) },
            Run { bg: Some((80, 80, 160)), ..run(24, '景', 2) },
        ],
        // row 4: 边界
        vec![
            run(0, '|', 1),
            run(1, ' ', 78),
            run(79, '|', 1),
        ],
        // row 5: 光标（块状，位于 5,10）
        vec![
            run(0, ' ', 10),
            Run { reverse: true, fg: (0, 0, 0), bg: Some((240, 240, 240)), ..run(10, '█', 1) },
        ],
    ]
}

const COLS: u16 = 80;
const ROWS: u16 = 6;

// ---------------------------------------------------------------------------
// 顶点与图集
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    xy: [f32; 2],   // 窗口坐标 0..1
    uv: [f32; 2],   // 图集坐标
    color: [f32; 4],// 前景/背景色 + alpha
}

// ---------------------------------------------------------------------------
// 字体度量
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct FontMetrics {
    cell_width: f32,
    cell_height: f32,
}

/// 用 cosmic-text 实测 'M' 的 advance width 和行高。
fn measure_font(font_system: &mut FontSystem) -> FontMetrics {
    let metrics = Metrics::new(16.0, 20.0);
    let mut buf = Buffer::new_empty(metrics);
    let attrs = Attrs::new().family(cosmic_text::Family::Monospace);
    buf.set_size(font_system, Some(100.0), None);
    buf.set_text(font_system, "M", &attrs, Shaping::Advanced, None);
    let layout = buf.layout_runs().next().unwrap();
    // line_w = 'M' 的 advance width
    // line_height = 行高（metrics 设定）
    FontMetrics {
        cell_width: layout.line_w,
        cell_height: metrics.line_height,
    }
}

// ---------------------------------------------------------------------------
// 应用
// ---------------------------------------------------------------------------

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    font_system: FontSystem,
    metrics: FontMetrics,
    /// 字形缓存：(字符, 粗体) → 位图在图集中的位置
    glyph_cache: std::collections::HashMap<(char, bool), (u32, u32, f32, f32, f32, f32, f32)>,
}

struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    atlas_texture: Option<wgpu::Texture>,
    atlas_view: Option<wgpu::TextureView>,
    atlas_sampler: wgpu::Sampler,
    bind_group: Option<wgpu::BindGroup>,
}

impl App {
    fn new() -> Self {
        let mut font_system = FontSystem::new();
        let metrics = measure_font(&mut font_system);
        eprintln!(
            "cell_width={:.2}px cell_height={:.2}px",
            metrics.cell_width, metrics.cell_height
        );
        Self {
            window: None,
            renderer: None,
            font_system,
            metrics,
            glyph_cache: std::collections::HashMap::new(),
        }
    }

    /// 构建字形图集 + 生成所有 quads。
    /// 图集 512x512：位置 (0,0) 是 1x1 白像素（背景 quad 采样），
    /// 之后是各字形位图（Mask = 单通道 alpha）。
    /// 每个 run 起始 x = start_col * cell_width（核心约束）。
    fn build_frame(&mut self) -> (Vec<u8>, Vec<Vertex>) {
        const ATLAS: u32 = 512;
        let mut atlas = vec![0u8; (ATLAS * ATLAS) as usize * 4];
        // (0,0) 白像素
        atlas[0] = 255;
        atlas[1] = 255;
        atlas[2] = 255;
        atlas[3] = 255;
        let mut next_x = 1u32;
        let mut next_y = 0u32;
        let mut row_h = 0u32;

        let mut cache = SwashCache::new();
        let mut quads: Vec<Vertex> = Vec::new();
        let cw = self.metrics.cell_width;
        let ch = self.metrics.cell_height;

        for (row_idx, row) in rows().iter().enumerate() {
            let row_y = row_idx as f32 * ch;
            for r in row {
                // 背景 quad
                if let Some(bg) = r.bg {
                    let x0 = r.start_col as f32 * cw;
                    let x1 = (r.start_col as f32 + r.run_len as f32) * cw;
                    let c = [
                        bg.0 as f32 / 255.0,
                        bg.1 as f32 / 255.0,
                        bg.2 as f32 / 255.0,
                        1.0,
                    ];
                    quads.push(Vertex { xy: [x0, row_y], uv: [0.0, 0.0], color: c });
                    quads.push(Vertex { xy: [x1, row_y], uv: [0.0, 0.0], color: c });
                    quads.push(Vertex { xy: [x1, row_y + ch], uv: [0.0, 0.0], color: c });
                    quads.push(Vertex { xy: [x0, row_y + ch], uv: [0.0, 0.0], color: c });
                }

                // 字形：run 内每个 cell 单独排版单个字符，x 显式计算。
                // 第 i 个 cell 的 x = (start_col + advance_before_i) * cell_width
                // advance_before_i = 前 i 个字符的列宽之和（普通 1，宽 2）。
                // 不使用 glyph.x（文本引擎的推进结果）。
                let mut col = r.start_col;
                for i in 0..r.run_len {
                    let cell_x = (r.start_col + i) as f32 * cw;
                    let glyph = self.rasterize_char(r.ch, r.bold, &mut cache, &mut atlas, &mut next_x, &mut next_y, &mut row_h, ATLAS);
                    if let Some((gw, gh, u0, v0, u1, v1, gy)) = glyph {
                        let fg = if r.reverse {
                            [0.0, 0.0, 0.0, 1.0]
                        } else {
                            [
                                r.fg.0 as f32 / 255.0,
                                r.fg.1 as f32 / 255.0,
                                r.fg.2 as f32 / 255.0,
                                1.0,
                            ]
                        };
                        let (tw, th) = (gw as f32, gh as f32);
                        // 宽字符占 2 列，但字形本身画在 cell_x 处（宽度 1 或 2 cell）
                        quads.push(Vertex { xy: [cell_x, gy], uv: [u0, v0], color: fg });
                        quads.push(Vertex { xy: [cell_x + tw, gy], uv: [u1, v0], color: fg });
                        quads.push(Vertex {
                            xy: [cell_x + tw, gy + th],
                            uv: [u1, v1],
                            color: fg,
                        });
                        quads.push(Vertex {
                            xy: [cell_x, gy + th],
                            uv: [u0, v1],
                            color: fg,
                        });
                    }
                    // 推进列：普通 1，宽 2
                    let advance = if r.ch as u32 > 0xFF { 2 } else { 1 };
                    col += advance;

}
                let _ = col;

                // 下划线：run 级一次绘制（整 run 宽度下方 1px 线）
                if r.underline {
                    let ux0 = r.start_col as f32 * cw;
                    let ux1 = (r.start_col as f32 + r.run_len as f32) * cw;
                    let uy = row_y + ch - 2.0; // 近底部的细线
                    let lh = 1.5;
                    let lc = if r.reverse {
                        [0.0, 0.0, 0.0, 1.0]
                    } else {
                        [
                            r.fg.0 as f32 / 255.0,
                            r.fg.1 as f32 / 255.0,
                            r.fg.2 as f32 / 255.0,
                            1.0,
                        ]
                    };
                    quads.push(Vertex { xy: [ux0, uy], uv: [0.0, 0.0], color: lc });
                    quads.push(Vertex { xy: [ux1, uy], uv: [0.0, 0.0], color: lc });
                    quads.push(Vertex { xy: [ux1, uy + lh], uv: [0.0, 0.0], color: lc });
                    quads.push(Vertex { xy: [ux0, uy + lh], uv: [0.0, 0.0], color: lc });
                }
            }
        }

        (atlas, quads)
    }

    /// 光栅化单个字符到图集（带缓存：同字符+同粗体只光栅化一次）。
    /// 返回 (位图宽, 高, u0, v0, u1, v1, 垂直位置)。
    /// 不使用文本引擎的 glyph.x——位置由调用方显式计算。
    fn rasterize_char(
        &mut self,
        ch: char,
        bold: bool,
        cache: &mut SwashCache,
        atlas: &mut Vec<u8>,
        next_x: &mut u32,
        next_y: &mut u32,
        row_h: &mut u32,
        atlas_size: u32,
    ) -> Option<(u32, u32, f32, f32, f32, f32, f32)> {
        let key = (ch, bold);
        // 用 self 内的字形缓存
        if let Some(cached) = self.glyph_cache.get(&key) {
            return Some(*cached);
        }

        // 单个字符排版（cosmic-text 只为拿 glyph 位图）
        let mut buf = Buffer::new_empty(Metrics::new(16.0, 20.0));
        buf.set_size(&mut self.font_system, Some(100.0), None);
        let mut attrs = Attrs::new().family(cosmic_text::Family::Monospace);
        if bold {
            attrs = attrs.weight(cosmic_text::Weight::BOLD);
        }
        let text: String = ch.to_string();
        buf.set_text(&mut self.font_system, &text, &attrs, Shaping::Advanced, None);

        // 取第一个 glyph 的位图
        let mut result = None;
        for line in buf.layout_runs() {
            for glyph in line.glyphs {
                let physical = glyph.physical((0.0, 0.0), 1.0);
                let img = match cache.get_image(&mut self.font_system, physical.cache_key) {
                    Some(img) => img,
                    None => continue,
                };
                let gw = img.placement.width;
                let gh = img.placement.height;
                if gw == 0 || gh == 0 {
                    continue;
                }
                // 图集换行
                if *next_x + gw > atlas_size {
                    *next_x = 1;
                    *next_y += (*row_h).max(1);
                    *row_h = 0;
                }
                if *next_y + gh > atlas_size {
                    eprintln!("atlas overflow: ch={:?} w={} h={}", ch, gw, gh);
                    continue;
                }
                // 复制位图（Mask 单通道 alpha → RGBA 白字）
                for py in 0..gh {
                    for px in 0..gw {
                        let src = (py * gw + px) as usize;
                        let a = img.data[src.min(img.data.len() - 1)];
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
                *next_x += gw;
                *row_h = (*row_h).max(gh);
                result = Some((gw, gh, u0, v0, u1, v1, physical.y as f32));
                break;
            }
            if result.is_some() {
                break;
            }
        }

        if let Some(r) = result {
            self.glyph_cache.insert(key, r);
        }
        result
    }

    fn render(&mut self, renderer: &mut Renderer) {
        let (atlas, quads) = self.build_frame();
        const ATLAS: u32 = 512;

        // 上传图集纹理
        if let Some(old) = renderer.atlas_texture.take() {
            drop(old);
        }
        let tex = renderer.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
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
        renderer.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &atlas,
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
        // 上传 screen size 到 uniform（像素 → NDC）
        let w = renderer.config.width as f32;
        let h = renderer.config.height as f32;
        renderer
            .queue
            .write_buffer(&renderer.uniform_buffer, 0, bytemuck::bytes_of(&[w, h]));

        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = renderer.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas bg"),
            layout: &renderer.pipeline.get_bind_group_layout(1),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&renderer.atlas_sampler),
                },
            ],
        });
        renderer.atlas_texture = Some(tex);
        renderer.atlas_view = Some(view);
        renderer.bind_group = Some(bind_group);

        // 上传 quads
        if quads.is_empty() {
            return;
        }
        let vbuf = renderer.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quads"),
            size: (quads.len() * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        renderer.queue.write_buffer(&vbuf, 0, bytemuck::cast_slice(&quads));
        renderer.vertex_buffer = vbuf;

        // 索引：每 quad 6 个索引
        let idx: Vec<u16> = (0..quads.len() as u16)
            .flat_map(|i| [i * 4, i * 4 + 1, i * 4 + 2, i * 4, i * 4 + 2, i * 4 + 3])
            .collect();
        let ibuf = renderer.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("indices"),
            size: (idx.len() * 2) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        renderer.queue.write_buffer(&ibuf, 0, bytemuck::cast_slice(&idx));
        renderer.index_buffer = ibuf;

        // 渲染
        let output = renderer.surface.get_current_texture().expect("surface");
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = renderer
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("term_grid encoder"),
            });
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("term_grid pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.08,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            rpass.set_pipeline(&renderer.pipeline);
            rpass.set_bind_group(0, &renderer.uniform_bind_group, &[]);
            rpass.set_bind_group(1, renderer.bind_group.as_ref().unwrap(), &[]);
            // 顶点布局: xy(f32x2) uv(f32x2) color(f32x4)
            rpass.set_vertex_buffer(0, renderer.vertex_buffer.slice(..));
            rpass.set_index_buffer(renderer.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
            rpass.draw_indexed(0..(idx.len() as u32), 0, 0..1);
        }

        renderer.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("vida M2b-0 term_grid")
            .with_inner_size(LogicalSize::new(
                COLS as f64 * 12.0,
                ROWS as f64 * 20.0,
            ));
        let window = Arc::new(event_loop.create_window(attrs).unwrap());

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..wgpu::InstanceDescriptor::from_env_or_default()
        });
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("term_grid"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .unwrap();

        let size = window.inner_size();
        let cap = surface.get_capabilities(&adapter);
        let format = cap.formats[0];
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: cap.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // 管线（顶点 3 属性: xy/uv/color）
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("term_grid shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("term_grid.wgsl").into()),
        });
        // uniform buffer：screen size（像素 → NDC 用）
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screen uniform"),
            size: 8, // vec2<f32>
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uniform bgl"),
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
            label: Some("uniform bg"),
            layout: &uniform_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });
        let atlas_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atlas bgl"),
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
            label: Some("term_grid layout"),
            bind_group_layouts: &[&uniform_bgl, &atlas_bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("term_grid pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,  // xy
                        1 => Float32x2,  // uv
                        2 => Float32x4,  // color
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let empty_vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("empty vbuf"),
            size: 1,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });
        let empty_ibuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("empty ibuf"),
            size: 1,
            usage: wgpu::BufferUsages::INDEX,
            mapped_at_creation: false,
        });

        self.window = Some(window);
        self.renderer = Some(Renderer {
            device,
            queue,
            surface,
            config,
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            vertex_buffer: empty_vbuf,
            index_buffer: empty_ibuf,
            atlas_texture: None,
            atlas_view: None,
            atlas_sampler: sampler,
            bind_group: None,
        });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                if let Some(r) = &mut self.renderer {
                    r.config.width = new_size.width.max(1);
                    r.config.height = new_size.height.max(1);
                    r.surface.configure(&r.device, &r.config);
                }
            }
            WindowEvent::RedrawRequested => {
                // 取出 renderer 避免双重借用（render 需要 &mut self）
                if let Some(mut r) = self.renderer.take() {
                    self.render(&mut r);
                    self.renderer = Some(r);
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new();
    event_loop.run_app(&mut app).unwrap();
}
