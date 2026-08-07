// term_grid WGSL 着色器
// 顶点格式: [x, y, u, v, r, g, b, a]
//   - 字形 quad: uv 指向图集, color 为前景色, 片元 = color * atlas_alpha
//   - 背景 quad: uv = (0,0), color 为背景色, atlas_alpha = 1（不采样）

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
    out.pos = vec4<f32>(
        xy.x * 2.0 - 1.0,
        1.0 - xy.y * 2.0,
        0.0,
        1.0,
    );
    out.uv = uv;
    out.color = color;
    return out;
}

@group(0) @binding(0) var glyph_atlas: texture_2d<f32>;
@group(0) @binding(1) var glyph_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let alpha = textureSample(glyph_atlas, glyph_sampler, in.uv).r;
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
