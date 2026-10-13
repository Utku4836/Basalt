// The grid as shown: the newest content at its scroll position over the previous
// content, blurred along the motion while it glides, then the cursor on top.

struct Compose {
    size: vec2<f32>,
    // Vertical offset of the newest content, and of the previous one from it.
    newest: f32,
    link: f32,
    // Length of the motion blur in pixels; zero at rest.
    blur: f32,
    background: vec4<f32>,
    // Cursor corners: top left and top right, bottom left and bottom right.
    cursor_top: vec4<f32>,
    cursor_bottom: vec4<f32>,
    cursor_color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> compose: Compose;
@group(0) @binding(1) var newest: texture_2d<f32>;
@group(0) @binding(2) var previous: texture_2d<f32>;

@vertex
fn screen(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // One triangle covering the target.
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner.x * 2.0 - 1.0, 1.0 - corner.y * 2.0, 0.0, 1.0);
}

// One pixel of content, `y` counted from the newest content's top.
fn pixel(x: i32, y: i32) -> vec4<f32> {
    let height = i32(compose.size.y);
    if y >= 0 && y < height {
        return textureLoad(newest, vec2<i32>(x, y), 0);
    }
    let older = y - i32(round(compose.link));
    if older >= 0 && older < height {
        return textureLoad(previous, vec2<i32>(x, older), 0);
    }
    return compose.background;
}

// Content at a fractional height, between the two nearest rows.
fn at(x: i32, y: f32) -> vec4<f32> {
    let from_top = y - compose.newest;
    let above = floor(from_top);
    return mix(pixel(x, i32(above)), pixel(x, i32(above) + 1), from_top - above);
}

@fragment
fn content(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let x = i32(position.x);
    let y = position.y - 0.5;
    if compose.blur < 0.5 {
        return at(x, y);
    }
    var sum = vec4<f32>(0.0);
    for (var tap = 0; tap < 9; tap++) {
        sum += at(x, y + compose.blur * (f32(tap) / 8.0 - 0.5));
    }
    return sum / 9.0;
}

@vertex
fn cursor_shape(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // A triangle strip: top left, top right, bottom left, bottom right.
    var corner: vec2<f32>;
    switch index {
        case 0u: { corner = compose.cursor_top.xy; }
        case 1u: { corner = compose.cursor_top.zw; }
        case 2u: { corner = compose.cursor_bottom.xy; }
        default: { corner = compose.cursor_bottom.zw; }
    }
    return vec4<f32>(corner.x / compose.size.x * 2.0 - 1.0, 1.0 - corner.y / compose.size.y * 2.0, 0.0, 1.0);
}

@fragment
fn cursor() -> @location(0) vec4<f32> {
    return compose.cursor_color;
}
