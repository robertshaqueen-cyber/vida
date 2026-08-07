// M2b-1 终端 WGSL 着色器（搬运自 M2b-0 term_grid example）
// 顶点格式: [x, y, u, v, r, g, b, a]  (xy 为窗口像素坐标)
// uniform: screen_size (窗口物理像素宽高)

struct ScreenSize {
    size: vec2<f32>,
}

@group(0) @binding(0) var<uniform> screen: ScreenSize;

@group(1) @binding(0) var glyph_atlas: texture_2d<f32>;
@group(1) @binding(1) var glyph_sampler: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}

@vertex
fn vs_main(
    @location(0) xy: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> VsOut {
    var out: VsOut;
    // 像素 → NDC：除以 screen_size，y 翻转
    let ndc = vec2<f32>(
        xy.x / screen.size.x * 2.0 - 1.0,
        1.0 - xy.y / screen.size.y * 2.0,
    );
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = uv;
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let alpha = textureSample(glyph_atlas, glyph_sampler, in.uv).a;
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
