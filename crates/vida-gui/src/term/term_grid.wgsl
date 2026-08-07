// M2b-1 终端 WGSL 着色器（搬运自 M2b-0 term_grid example）
// 顶点格式: [x, y, u, v, r, g, b, a]  (xy 为窗口物理像素坐标)
// uniform: screen (窗口物理像素宽高 + sRGB 校正开关)

struct ScreenInfo {
    size: vec2<f32>,
    /// >0.5 表示 surface 为 sRGB 格式：片元颜色需在输出前转 linear，
    /// 由 GPU 在写入时做 linear→sRGB 编码（iced 自身文字管线同样如此）。
    gamma: f32,
    _pad: f32,
}

@group(0) @binding(0) var<uniform> screen: ScreenInfo;

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
    // A/B 版本 B：不做 gamma 校正，直接输出 sRGB 值。
    // 文字抗锯齿在线性空间混合会让深色背景上的字更细（已知现象），
    // 大多数文字渲染器直接在 sRGB 空间混合。对比 A（pow 2.2）后定。
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
