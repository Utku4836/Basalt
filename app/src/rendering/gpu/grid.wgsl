// Terminal grid quads in pixel space: cell backgrounds and decorations as solid
// rectangles, glyphs as atlas samples blended per subpixel with dual-source blending.
enable dual_source_blending;

struct Grid {
    size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> grid: Grid;
@group(1) @binding(0) var atlas: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

struct Quad {
    @location(0) position: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv: vec4<f32>,
    @location(3) color: vec4<f32>,
};

struct Varyings {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) color: vec4<f32>,
};

@vertex
fn vertex(@builtin(vertex_index) corner: u32, quad: Quad) -> Varyings {
    // A triangle strip over the four corners.
    let unit = vec2<f32>(f32(corner & 1u), f32(corner >> 1u));
    let pixel = quad.position + quad.size * unit;
    var out: Varyings;
    out.position = vec4<f32>(pixel.x / grid.size.x * 2.0 - 1.0, 1.0 - pixel.y / grid.size.y * 2.0, 0.0, 1.0);
    out.uv = quad.uv.xy + quad.uv.zw * unit;
    out.color = quad.color;
    return out;
}

@fragment
fn solid(in: Varyings) -> @location(0) vec4<f32> {
    return in.color;
}

struct Subpixel {
    @location(0) @blend_src(0) color: vec4<f32>,
    @location(0) @blend_src(1) coverage: vec4<f32>,
};

@fragment
fn glyph(in: Varyings) -> Subpixel {
    let coverage = textureSample(atlas, atlas_sampler, in.uv).rgb;
    var out: Subpixel;
    out.color = vec4<f32>(in.color.rgb, 1.0);
    out.coverage = vec4<f32>(coverage, coverage.g);
    return out;
}
