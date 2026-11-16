// PROTEUS render passes: ground, rectangles, glyphs.
// All positions are physical pixels with the origin at the top-left.
// All colours are linear; the target is sRGB so blending is linear too.

struct Globals {
    screen: vec2<f32>,
    time: f32,
    reveal_dur: f32,
    accent: vec4<f32>,
    bg: vec4<f32>,
    cursor: vec4<f32>,
    // x: how far (px) a newborn glyph rises into place.
    misc: vec4<f32>,
    // The light field. x: emission this step, y: how strongly it shows,
    // z: how much survives this step, w: upward drift in field texels.
    field: vec4<f32>,
};

@group(0) @binding(0) var<uniform> G: Globals;
@group(1) @binding(0) var atlas_tex: texture_2d<f32>;
@group(1) @binding(1) var atlas_smp: sampler;
@group(2) @binding(0) var field_tex: texture_2d<f32>;
@group(2) @binding(1) var field_smp: sampler;

fn corner(vi: u32) -> vec2<f32> {
    return vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u));
}

fn to_clip(p: vec2<f32>) -> vec4<f32> {
    let n = p / G.screen * 2.0 - vec2<f32>(1.0, 1.0);
    return vec4<f32>(n.x, -n.y, 0.0, 1.0);
}

// ---------------------------------------------------------------- ground

@vertex
fn vs_bg(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i32(vi & 1u) * 4 - 1);
    let y = f32(i32(vi >> 1u) * 4 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_bg(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = p.xy / G.screen;
    let d = distance(uv, vec2<f32>(0.5, 0.5));
    var col = G.bg.rgb * (1.0 - 0.5 * smoothstep(0.30, 0.95, d));
    // A breath of the accent along the top edge, so the void has a light source.
    let top = exp(-pow(uv.y * 3.0, 2.0)) * exp(-pow((uv.x - 0.5) * 1.5, 2.0));
    col += G.accent.rgb * 0.010 * top;
    // Light cast by text that has just arrived (zero when nothing is new).
    col += textureSampleLevel(field_tex, field_smp, uv, 0.0).rgb * G.field.y;
    // Sub-LSB dither removes banding in the dark gradient.
    let n = fract(sin(dot(p.xy, vec2<f32>(12.9898, 78.233))) * 43758.5453) - 0.5;
    col += vec3<f32>(n * 0.0007);
    return vec4<f32>(max(col, vec3<f32>(0.0)), 1.0);
}

// ------------------------------------------------------------ rectangles

struct RectIn {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    // x: corner radius, y: ring thickness (0 = filled), z: glow extent (0 = hard), w: glow strength
    @location(3) params: vec4<f32>,
};

struct RectOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local: vec2<f32>,
    @location(2) size: vec2<f32>,
    @location(3) params: vec4<f32>,
};

@vertex
fn vs_rect(@builtin(vertex_index) vi: u32, i: RectIn) -> RectOut {
    let c = corner(vi);
    let m = i.params.z;
    let full = i.size + vec2<f32>(2.0 * m, 2.0 * m);
    var o: RectOut;
    o.clip = to_clip(i.pos - vec2<f32>(m, m) + c * full);
    o.local = c * full - vec2<f32>(m, m) - i.size * 0.5;
    o.color = i.color;
    o.size = i.size;
    o.params = i.params;
    return o;
}

@fragment
fn fs_rect(i: RectOut) -> @location(0) vec4<f32> {
    let half = i.size * 0.5;
    let r = min(i.params.x, min(half.x, half.y));
    let q = abs(i.local) - half + vec2<f32>(r, r);
    let d = length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
    var a = clamp(0.5 - d, 0.0, 1.0);
    if (i.params.y > 0.0) {
        let dr = abs(d + i.params.y * 0.5) - i.params.y * 0.5;
        a = clamp(0.5 - dr, 0.0, 1.0);
    }
    if (i.params.z > 0.0) {
        let g = max(d, 0.0) / i.params.z;
        a = exp(-g * g * 4.5) * i.params.w;
    }
    let al = i.color.a * a;
    return vec4<f32>(i.color.rgb * al, al);
}

// ---------------------------------------------------------------- glyphs

struct GlyphIn {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv: vec4<f32>,
    @location(3) color: vec4<f32>,
    // x: birth time (negative = settled), y: energy 0..1
    @location(4) params: vec4<f32>,
};

struct GlyphOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    // x: reveal progress 0..1, y: energy, z: 1 once the glyph's moment has come
    @location(2) anim: vec3<f32>,
};

fn progress(birth: f32) -> f32 {
    if (birth < 0.0 || G.reveal_dur <= 0.0) {
        return 1.0;
    }
    return clamp((G.time - birth) / G.reveal_dur, 0.0, 1.0);
}

@vertex
fn vs_glyph(@builtin(vertex_index) vi: u32, i: GlyphIn) -> GlyphOut {
    let c = corner(vi);
    let t = progress(i.params.x);
    let ease = 1.0 - pow(1.0 - t, 3.0);
    let energy = i.params.y;
    // A newborn glyph is slightly small and low, then settles into its cell.
    let scale = 1.0 - (1.0 - ease) * 0.12 * energy;
    let rise = (1.0 - ease) * energy * G.misc.x;
    let center = i.pos + i.size * 0.5;
    var o: GlyphOut;
    o.clip = to_clip(center + (c - vec2<f32>(0.5, 0.5)) * i.size * scale + vec2<f32>(0.0, rise));
    o.uv = mix(i.uv.xy, i.uv.zw, c);
    o.color = i.color;
    var live = 1.0;
    if (i.params.x > G.time) {
        live = 0.0;
    }
    o.anim = vec3<f32>(t, energy, live);
    return o;
}

@fragment
fn fs_glyph(i: GlyphOut) -> @location(0) vec4<f32> {
    let m = textureSample(atlas_tex, atlas_smp, i.uv).r;
    // Linear blending thins light text on dark ground; compensate by how
    // bright the text is, leaving dark-on-light untouched.
    let lum = dot(i.color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let cov = mix(m, pow(m, 0.70), clamp(lum * 1.5, 0.0, 1.0));
    let t = i.anim.x;
    let energy = i.anim.y;
    let ease = 1.0 - pow(1.0 - t, 3.0);
    // It arrives as light in the accent colour and cools to its own colour.
    let heat = (1.0 - t) * (1.0 - t) * energy;
    let rgb = mix(i.color.rgb, G.accent.rgb * 1.5 + i.color.rgb * 0.5, heat * 0.9);
    let a = cov * i.color.a * (1.0 - energy * (1.0 - ease));
    return vec4<f32>(rgb * a, a);
}

// ------------------------------------------------------------ light field
// A quarter-resolution buffer of light. Text that has just arrived emits
// into it; every step it spreads, rises a little and fades. With nothing
// new on screen it is black and is not simulated at all.

@fragment
fn fs_field_step(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    let px = vec2<f32>(1.0, 1.0) / vec2<f32>(textureDimensions(field_tex));
    let uv = p.xy * px + vec2<f32>(0.0, G.field.w * px.y);
    let r = 1.4;
    var s = textureSampleLevel(field_tex, field_smp, uv, 0.0) * 0.28;
    s += textureSampleLevel(field_tex, field_smp, uv + vec2<f32>(px.x * r, 0.0), 0.0) * 0.12;
    s += textureSampleLevel(field_tex, field_smp, uv - vec2<f32>(px.x * r, 0.0), 0.0) * 0.12;
    s += textureSampleLevel(field_tex, field_smp, uv + vec2<f32>(0.0, px.y * r), 0.0) * 0.12;
    s += textureSampleLevel(field_tex, field_smp, uv - vec2<f32>(0.0, px.y * r), 0.0) * 0.12;
    s += textureSampleLevel(field_tex, field_smp, uv + px * r, 0.0) * 0.06;
    s += textureSampleLevel(field_tex, field_smp, uv - px * r, 0.0) * 0.06;
    s += textureSampleLevel(field_tex, field_smp, uv + vec2<f32>(px.x, -px.y) * r, 0.0) * 0.06;
    s += textureSampleLevel(field_tex, field_smp, uv + vec2<f32>(-px.x, px.y) * r, 0.0) * 0.06;
    return s * G.field.z;
}

// A glyph is a light source for as long as it is still hot from arriving,
// in its own colour: an error glows red as it lands, a success green.
@fragment
fn fs_field_glyph(i: GlyphOut) -> @location(0) vec4<f32> {
    let m = textureSampleLevel(atlas_tex, atlas_smp, i.uv, 0.0).r;
    let t = i.anim.x;
    let heat = (1.0 - t) * (1.0 - t) * i.anim.y * i.anim.z;
    // Only the ink emits, so the light has the shape of the text and not of
    // the cells it sits in.
    let e = m * heat * G.field.x;
    let tint = mix(i.color.rgb, G.accent.rgb, 0.25);
    return vec4<f32>(tint * e, e);
}
