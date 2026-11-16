//! The renderer. The grid is turned into GPU instances only when its content
//! changes; between changes a frame costs one uniform write and a few draw
//! calls, and when nothing is moving no frame is drawn at all.
//!
//! Every glyph instance carries the moment it appeared, so text arriving on
//! screen animates entirely in the shader.
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{point_to_viewport, Term};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use bytemuck::{Pod, Zeroable};

use crate::atlas::{Atlas, GlyphKey};
use crate::fonts::FontSet;
use crate::term::Listener;
use crate::theme::{lin, mix, Theme, C3};

/// Drawing a `world::Scene` with these same pipelines.
mod scene;
/// Link underlines, the hover strip, and reduced motion's switch.
mod link;
pub use link::Strip;

/// How long a glyph takes to settle after it appears, in seconds.
const REVEAL_DUR: f32 = 0.24;
/// Delay between neighbouring glyphs born in the same update: text unfolds
/// left to right instead of popping in as a block.
const STAGGER: f32 = 0.0032;
const STAGGER_MAX: f32 = 0.16;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Globals {
    screen: [f32; 2],
    time: f32,
    reveal_dur: f32,
    accent: [f32; 4],
    bg: [f32; 4],
    cursor: [f32; 4],
    misc: [f32; 4],
    field: [f32; 4],
}

/// The light field is simulated at this fraction of the surface size.
const FIELD_SCALE: u32 = 4;
/// Time for field light to fall to about a third, in seconds.
const FIELD_TAU: f32 = 0.16;
/// How long after the last emission the field is still worth simulating
/// (about 2% of the light is left by then).
const FIELD_TAIL: f32 = 0.6;
/// Emission strength per second of a fully hot glyph.
const FIELD_EMIT: f32 = 20.0;
const FIELD_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct RectInst {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
    params: [f32; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct GlyphInst {
    pos: [f32; 2],
    size: [f32; 2],
    uv: [f32; 4],
    color: [f32; 4],
    params: [f32; 4],
}

struct Pending {
    idx: u32,
    row: u16,
    col: u16,
    ch: char,
    style: u8,
    span: u8,
    fg: C3,
}

#[derive(Clone, Copy)]
struct CursorTarget {
    x: f32,
    y: f32,
    shape: CursorShape,
}

pub struct Renderer {
    pub format: wgpu::TextureFormat,
    pub fonts: FontSet,
    pub theme: Theme,
    atlas: Atlas,

    globals_buf: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,
    atlas_bg: wgpu::BindGroup,
    bg_pipe: wgpu::RenderPipeline,
    rect_pipe: wgpu::RenderPipeline,
    glyph_pipe: wgpu::RenderPipeline,

    rect_buf: wgpu::Buffer,
    rect_cap: usize,
    glyph_buf: wgpu::Buffer,
    glyph_cap: usize,
    cursor_buf: wgpu::Buffer,

    rects_under: Vec<RectInst>,
    rects_over: Vec<RectInst>,
    glyphs: Vec<GlyphInst>,
    pending: Vec<Pending>,
    n_under: u32,
    n_over: u32,
    n_glyphs: u32,
    n_cursor: u32,

    pub screen: [f32; 2],
    pub pad: [f32; 2],

    // Reveal bookkeeping: what each cell showed last time and when it changed.
    keys: Vec<u32>,
    /// Red (-1), green (1) or neither for the glyph in each cell this build.
    tint: Vec<i8>,
    prev_keys: Vec<u32>,
    birth: Vec<f32>,
    energy: Vec<f32>,
    row_hash: Vec<u64>,
    prev_dims: (usize, usize),
    prev_offset: usize,
    /// Skip the reveal for the next build (resize, scrollback, replay).
    pub suppress_reveal: bool,
    pub reveal_enabled: bool,
    anim_until: f32,
    time_base: f64,

    cursor: Option<CursorTarget>,
    cursor_pos: Option<[f32; 2]>,
    cursor_energy: f32,
    last_t: f32,
    cursor_moving: bool,
    /// Where the cursor is drawn this frame (position, size, corner radius).
    cursor_geom: Option<([f32; 2], [f32; 2], f32)>,

    // Light field: two buffers, read one and write the other each step.
    field_bgl: wgpu::BindGroupLayout,
    field_sampler: wgpu::Sampler,
    field_views: [wgpu::TextureView; 2],
    field_bgs: [wgpu::BindGroup; 2],
    field_cur: usize,
    field_live: bool,
    field_until: f32,
    field_t: f32,
    step_pipe: wgpu::RenderPipeline,
    emit_glyph_pipe: wgpu::RenderPipeline,
    emit_rect_pipe: wgpu::RenderPipeline,
    /// Strength of the light field; 0 turns it off entirely.
    pub light: f32,
    /// What the last build found newly arrived (drives sound).
    pub arrivals: Arrivals,
    /// Drawn over the grid by the engine itself (see `Overlay`).
    overlay: OverlayGpu,
    /// Links (`link.rs`): cells of declared links in the last build, the
    /// cells of the link under the pointer, and the strip naming its target.
    link_cells: Vec<link::LinkCell>,
    pub hover: Vec<crate::links::Span>,
    strip: OverlayGpu,
    strip_src: Option<Strip>,
    strip_gen: u64,
    /// Reduced motion (`motion.rs`): nothing unfolds, glows, glides or pulses.
    pub still: bool,
}

/// Shapes and text the engine draws over the grid, in physical pixels: the
/// scrub bar. Nothing here animates; it is rebuilt when what it shows changes.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Overlay {
    pub rects: Vec<OverlayRect>,
    pub texts: Vec<OverlayText>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverlayRect {
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub color: C3,
    pub alpha: f32,
    pub radius: f32,
    /// How far a soft glow reaches beyond the edge (0 = a plain rectangle).
    pub glow: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverlayText {
    /// Top-left of the first character's cell.
    pub pos: [f32; 2],
    pub text: String,
    pub color: C3,
    pub alpha: f32,
    pub bold: bool,
}

#[derive(Default)]
struct OverlayGpu {
    rects: Option<wgpu::Buffer>,
    glyphs: Option<wgpu::Buffer>,
    n_rects: u32,
    n_glyphs: u32,
}

/// Text that landed in empty cells in one build. Changes in place, scrolled
/// rows, resizes and replayed history are not arrivals.
#[derive(Clone, Copy, Default, Debug)]
pub struct Arrivals {
    pub cells: u32,
    /// Of those, how many landed beside the cursor (what the person typed).
    pub typed: u32,
    /// How many arrived in red, and in green.
    pub red: u32,
    pub green: u32,
    pub bulk: bool,
    /// Mean column of the arrivals, -1 (left edge) to 1 (right edge).
    pub pan: f32,
}

fn make_field(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    w: u32,
    h: u32,
) -> ([wgpu::TextureView; 2], [wgpu::BindGroup; 2]) {
    let one = |label: &str| {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FIELD_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        });
        (view, bg)
    };
    let (v0, b0) = one("field a");
    let (v1, b1) = one("field b");
    ([v0, v1], [b0, b1])
}

fn resolve(c: Color, colors: &Colors, th: &Theme) -> C3 {
    match c {
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        Color::Indexed(i) => colors[i as usize].map(|c| [c.r, c.g, c.b]).unwrap_or_else(|| th.indexed(i)),
        Color::Named(n) => {
            if let Some(c) = colors[n] {
                return [c.r, c.g, c.b];
            }
            let i = n as usize;
            match n {
                NamedColor::Foreground | NamedColor::BrightForeground => th.fg,
                NamedColor::Background => th.bg,
                NamedColor::Cursor => th.cursor,
                NamedColor::DimForeground => mix(th.fg, th.bg, 0.40),
                _ if i < 16 => th.ansi[i],
                // Dim variants of the eight base colours.
                _ if (259..=266).contains(&i) => mix(th.ansi[i - 259], th.bg, 0.40),
                _ => th.fg,
            }
        }
    }
}

fn key_hash(ch: char, fg: C3, style: u8) -> u32 {
    let mut h = 0x811c_9dc5u32;
    for b in [ch as u32, u32::from_le_bytes([fg[0], fg[1], fg[2], style])] {
        h = (h ^ b).wrapping_mul(0x0100_0193);
        h ^= h >> 15;
    }
    h | 1
}

const BLANK_ROW: u64 = 0xcbf2_9ce4_8422_2325;

fn hash_row(keys: &[u32]) -> u64 {
    let mut h = BLANK_ROW;
    for &k in keys {
        if k != 0 {
            h = (h ^ k as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
        h = h.rotate_left(5);
    }
    if keys.iter().all(|&k| k == 0) {
        BLANK_ROW
    } else {
        h
    }
}

/// How many rows the content moved up since last time (0 if it did not
/// scroll). Without this a single new line would re-animate the whole screen.
fn detect_scroll(old: &[u64], new: &[u64]) -> usize {
    let rows = new.len().min(old.len());
    let score = |k: usize| (0..rows - k).filter(|&i| new[i] == old[i + k] && new[i] != BLANK_ROW).count();
    if rows < 2 {
        return 0;
    }
    let s0 = score(0);
    let (mut best_k, mut best) = (0, 0);
    for k in 1..rows {
        let s = score(k);
        if s > best {
            best = s;
            best_k = k;
        }
    }
    if best > s0 && best >= 2 {
        best_k
    } else {
        0
    }
}

/// For each row of the new screen, the row of the old screen it continues
/// (so its glyphs keep their history instead of counting as new), or `None`
/// for a row that has just come into view. `k` is how far the scrolled part
/// moved up; `same(new_row, old_row)` counts matching cells and is consulted
/// only for rows that changed while moving or staying.
fn row_sources(old: &[u64], new: &[u64], k: usize, same: impl Fn(usize, usize) -> usize) -> Vec<Option<usize>> {
    let rows = new.len().min(old.len());
    (0..rows)
        .map(|i| {
            if new[i] == old[i] {
                return Some(i); // stayed where it was
            }
            let moved = i + k < rows;
            if moved && new[i] == old[i + k] {
                return Some(i + k); // moved up intact
            }
            if !moved {
                return None; // scrolled into view
            }
            let (here, there) = (same(i, i), same(i, i + k));
            if there >= here && there > 0 {
                Some(i + k)
            } else if here > 0 {
                Some(i)
            } else {
                None
            }
        })
        .collect()
}

fn instance_layout<'a>(stride: u64, attrs: &'a [wgpu::VertexAttribute]) -> wgpu::VertexBufferLayout<'a> {
    wgpu::VertexBufferLayout { array_stride: stride, step_mode: wgpu::VertexStepMode::Instance, attributes: attrs }
}

fn make_buffer(device: &wgpu::Device, label: &str, bytes: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.max(64),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

impl Renderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, font_px: f32, theme: Theme) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("proteus"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let globals_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let atlas_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atlas"),
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

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &globals_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() }],
        });

        let atlas = Atlas::new(device, 4096);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let atlas_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas"),
            layout: &atlas_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&atlas.view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        // The field is sampled exactly like the atlas: one texture, one sampler.
        let field_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("field"),
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
        let field_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("field"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let (field_views, field_bgs) = make_field(device, &field_bgl, &field_sampler, 1, 1);

        // One layout for every pipeline, so bind groups set once stay valid.
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("proteus"),
            bind_group_layouts: &[&globals_bgl, &atlas_bgl, &field_bgl],
            push_constant_ranges: &[],
        });
        let (flat_layout, glyph_layout) = (&layout, &layout);

        let target = [Some(wgpu::ColorTargetState {
            format,
            blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let add = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        // Into the field: the step overwrites, emitters add light.
        let field_replace = [Some(wgpu::ColorTargetState { format: FIELD_FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })];
        let field_add = [Some(wgpu::ColorTargetState {
            format: FIELD_FORMAT,
            blend: Some(wgpu::BlendState { color: add, alpha: add }),
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let pipeline_to = |label: &str,
                           layout: &wgpu::PipelineLayout,
                           vs: &str,
                           fs: &str,
                           buffers: &[wgpu::VertexBufferLayout],
                           topology: wgpu::PrimitiveTopology,
                           target: &[Option<wgpu::ColorTargetState>]| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState { topology, ..Default::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: target,
                }),
                multiview: None,
                cache: None,
            })
        };
        let pipeline = |label: &str,
                        layout: &wgpu::PipelineLayout,
                        vs: &str,
                        fs: &str,
                        buffers: &[wgpu::VertexBufferLayout],
                        topology: wgpu::PrimitiveTopology| pipeline_to(label, layout, vs, fs, buffers, topology, &target);

        let rect_attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32x4];
        let glyph_attrs =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4];
        let rect_buffers = [instance_layout(std::mem::size_of::<RectInst>() as u64, &rect_attrs)];
        let glyph_buffers = [instance_layout(std::mem::size_of::<GlyphInst>() as u64, &glyph_attrs)];
        let strip = wgpu::PrimitiveTopology::TriangleStrip;
        let bg_pipe = pipeline("ground", flat_layout, "vs_bg", "fs_bg", &[], wgpu::PrimitiveTopology::TriangleList);
        let rect_pipe = pipeline("rects", flat_layout, "vs_rect", "fs_rect", &rect_buffers, strip);
        let glyph_pipe = pipeline("glyphs", glyph_layout, "vs_glyph", "fs_glyph", &glyph_buffers, strip);
        let step_pipe =
            pipeline_to("field step", &layout, "vs_bg", "fs_field_step", &[], wgpu::PrimitiveTopology::TriangleList, &field_replace);
        let emit_glyph_pipe = pipeline_to("field glyphs", &layout, "vs_glyph", "fs_field_glyph", &glyph_buffers, strip, &field_add);
        let emit_rect_pipe = pipeline_to("field rects", &layout, "vs_rect", "fs_rect", &rect_buffers, strip, &field_add);

        let rect_cap = 1024;
        let glyph_cap = 8192;
        Self {
            format,
            fonts: FontSet::new(font_px),
            theme,
            atlas,
            globals_buf,
            globals_bg,
            atlas_bg,
            bg_pipe,
            rect_pipe,
            glyph_pipe,
            rect_buf: make_buffer(device, "rects", (rect_cap * std::mem::size_of::<RectInst>()) as u64),
            rect_cap,
            glyph_buf: make_buffer(device, "glyphs", (glyph_cap * std::mem::size_of::<GlyphInst>()) as u64),
            glyph_cap,
            cursor_buf: make_buffer(device, "cursor", (4 * std::mem::size_of::<RectInst>()) as u64),
            rects_under: Vec::new(),
            rects_over: Vec::new(),
            glyphs: Vec::new(),
            pending: Vec::new(),
            n_under: 0,
            n_over: 0,
            n_glyphs: 0,
            n_cursor: 0,
            screen: [1.0, 1.0],
            pad: [0.0, 0.0],
            keys: Vec::new(),
            prev_keys: Vec::new(),
            tint: Vec::new(),
            birth: Vec::new(),
            energy: Vec::new(),
            row_hash: Vec::new(),
            prev_dims: (0, 0),
            prev_offset: 0,
            suppress_reveal: true,
            reveal_enabled: true,
            anim_until: 0.0,
            time_base: 0.0,
            cursor: None,
            cursor_pos: None,
            cursor_energy: 0.0,
            last_t: 0.0,
            cursor_moving: false,
            cursor_geom: None,
            field_bgl,
            field_sampler,
            field_views,
            field_bgs,
            field_cur: 0,
            field_live: false,
            field_until: 0.0,
            field_t: 0.0,
            step_pipe,
            emit_glyph_pipe,
            emit_rect_pipe,
            light: 1.0,
            arrivals: Arrivals::default(),
            overlay: OverlayGpu::default(),
            link_cells: Vec::new(),
            hover: Vec::new(),
            strip: OverlayGpu::default(),
            strip_src: None,
            strip_gen: 0,
            still: false,
        }
    }

    /// Replace what is drawn over the grid (`None` removes it). Returns true
    /// if making room for its glyphs wiped the glyph atlas, in which case the
    /// grid must be built again before the next frame.
    pub fn set_overlay(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, overlay: Option<&Overlay>) -> bool {
        let Some(o) = overlay else {
            self.overlay.n_rects = 0;
            self.overlay.n_glyphs = 0;
            return false;
        };
        let m = self.fonts.metrics;
        let rects: Vec<RectInst> = o
            .rects
            .iter()
            .map(|r| RectInst { pos: r.pos, size: r.size, color: lin(r.color, r.alpha), params: [r.radius, 0.0, r.glow, if r.glow > 0.0 { 1.0 } else { 0.0 }] })
            .collect();
        let generation = self.atlas.generation;
        let mut glyphs = Vec::new();
        for t in &o.texts {
            for (i, ch) in t.text.chars().enumerate().filter(|(_, c)| *c != ' ') {
                let slot = self.atlas.get(queue, &mut self.fonts, GlyphKey { ch, style: t.bold as u8, span: 1 });
                if !slot.empty {
                    glyphs.push(GlyphInst {
                        pos: [t.pos[0] + i as f32 * m.cell_w + slot.off[0], t.pos[1] + slot.off[1]],
                        size: slot.size,
                        uv: slot.uv,
                        color: lin(t.color, t.alpha),
                        params: [-1.0, 0.0, 0.0, 0.0],
                    });
                }
            }
        }
        let upload = |buf: &mut Option<wgpu::Buffer>, label: &str, bytes: &[u8]| {
            if buf.as_ref().is_none_or(|b| b.size() < bytes.len() as u64) {
                *buf = Some(make_buffer(device, label, (bytes.len() as u64).next_power_of_two()));
            }
            if let (Some(b), false) = (buf.as_ref(), bytes.is_empty()) {
                queue.write_buffer(b, 0, bytes);
            }
        };
        upload(&mut self.overlay.rects, "overlay rects", bytemuck::cast_slice(&rects));
        upload(&mut self.overlay.glyphs, "overlay glyphs", bytemuck::cast_slice(&glyphs));
        self.overlay.n_rects = rects.len() as u32;
        self.overlay.n_glyphs = glyphs.len() as u32;
        self.atlas.generation != generation
    }

    pub fn resize(&mut self, device: &wgpu::Device, w: u32, h: u32, pad: f32) {
        self.screen = [w.max(1) as f32, h.max(1) as f32];
        self.pad = [pad.round(), pad.round()];
        self.suppress_reveal = true;
        let (views, bgs) = make_field(device, &self.field_bgl, &self.field_sampler, w.max(1).div_ceil(FIELD_SCALE), h.max(1).div_ceil(FIELD_SCALE));
        self.field_views = views;
        self.field_bgs = bgs;
        self.field_live = false;
        self.field_until = 0.0;
    }

    /// Columns and rows that fit the current surface.
    pub fn grid(&self) -> (usize, usize) {
        let m = self.fonts.metrics;
        let cols = ((self.screen[0] - 2.0 * self.pad[0]) / m.cell_w).floor().max(2.0) as usize;
        let rows = ((self.screen[1] - 2.0 * self.pad[1]) / m.cell_h).floor().max(1.0) as usize;
        (cols, rows)
    }

    /// Which cell a pixel falls in, and whether it is in the cell's right half.
    pub fn cell_at(&self, x: f32, y: f32, cols: usize, rows: usize) -> (usize, usize, bool) {
        let m = self.fonts.metrics;
        let fx = ((x - self.pad[0]) / m.cell_w).max(0.0);
        let fy = ((y - self.pad[1]) / m.cell_h).max(0.0);
        let col = (fx as usize).min(cols.saturating_sub(1));
        let row = (fy as usize).min(rows.saturating_sub(1));
        (col, row, fx.fract() >= 0.5)
    }

    pub fn set_font_px(&mut self, queue: &wgpu::Queue, px: f32) {
        self.fonts.set_px(px);
        self.atlas.reset(queue);
        self.suppress_reveal = true;
    }

    /// A keystroke happened: the cursor's light flares briefly.
    pub fn pulse(&mut self) {
        if !self.still {
            self.cursor_energy = (self.cursor_energy + 0.55).min(1.4);
        }
    }

    fn t32(&self, now: f64) -> f32 {
        (now - self.time_base) as f32
    }

    /// True while something on screen is still in motion.
    pub fn animating(&self, now: f64) -> bool {
        // A hidden cursor has no light to fade, so it must not keep frames coming.
        let t = self.t32(now);
        t < self.anim_until
            || self.cursor_moving
            || (self.cursor.is_some() && self.cursor_energy > 0.02)
            || (self.light > 0.0 && !self.still && t < self.field_until)
    }

    /// Rebuild GPU instances from terminal state. Call only when content,
    /// selection, scroll position, size or focus changed.
    pub fn build(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, term: &Term<Listener>, now: f64, focused: bool) {
        // Keep shader time small so 32-bit floats stay precise for weeks.
        if self.t32(now) > 3600.0 && self.t32(now) >= self.anim_until {
            self.time_base = now;
            self.anim_until = 0.0;
            self.last_t = 0.0;
            self.field_until = 0.0;
            self.field_live = false;
            self.field_t = 0.0;
            self.birth.iter_mut().for_each(|b| *b = -1.0);
        }
        let t = self.t32(now);
        let m = self.fonts.metrics;
        let th = self.theme.clone();
        let pad = self.pad;
        let cols = term.columns();
        let rows = term.screen_lines();
        let n = cols * rows;

        let content = term.renderable_content();
        let offset = content.display_offset;
        let colors = content.colors;
        let cursor_point = content.cursor.point;
        let cursor_shape = content.cursor.shape;
        let selection = content.selection;

        self.rects_under.clear();
        self.rects_over.clear();
        self.pending.clear();
        self.keys.clear();
        self.keys.resize(n, 0);
        self.tint.clear();
        self.tint.resize(n, 0);
        self.link_cells.clear();

        let cursor_vp = point_to_viewport(offset, cursor_point).filter(|p| p.line < rows && p.column.0 < cols);
        let block_cursor_idx = match (cursor_vp, cursor_shape, focused) {
            (Some(p), CursorShape::Block, true) => Some(p.line * cols + p.column.0),
            _ => None,
        };

        // Background runs: neighbouring cells of one colour become one quad.
        let mut run: Option<(usize, usize, usize, C3)> = None;
        let flush = |v: &mut Vec<RectInst>, r: Option<(usize, usize, usize, C3)>| {
            if let Some((row, c0, c1, col)) = r {
                v.push(RectInst {
                    pos: [pad[0] + c0 as f32 * m.cell_w, pad[1] + row as f32 * m.cell_h],
                    size: [(c1 - c0 + 1) as f32 * m.cell_w, m.cell_h],
                    color: lin(col, 1.0),
                    params: [0.0; 4],
                });
            }
        };

        for cell in content.display_iter {
            let Some(vp) = point_to_viewport(offset, cell.point) else { continue };
            let (r, c) = (vp.line, vp.column.0);
            if r >= rows || c >= cols {
                continue;
            }
            let flags = cell.flags;
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = resolve(cell.fg, colors, &th);
            let mut bg = match cell.bg {
                Color::Named(NamedColor::Background) => None,
                other => Some(resolve(other, colors, &th)),
            };
            if flags.contains(Flags::DIM) {
                fg = mix(fg, bg.unwrap_or(th.bg), 0.42);
            }
            if flags.contains(Flags::INVERSE) {
                let new_fg = bg.unwrap_or(th.bg);
                bg = Some(fg);
                fg = new_fg;
            }
            if selection.as_ref().is_some_and(|s| s.contains_cell(&cell, cursor_point, cursor_shape)) {
                bg = Some(th.selection_bg);
            }
            let span = if flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
            let end = (c + span - 1).min(cols - 1);

            match bg {
                Some(col) => {
                    let extends = matches!(run, Some((rr, _, c1, rc)) if rr == r && rc == col && c1 + 1 == c);
                    if extends {
                        if let Some(x) = run.as_mut() {
                            x.2 = end;
                        }
                    } else {
                        flush(&mut self.rects_under, run.take());
                        run = Some((r, c, end, col));
                    }
                }
                None => flush(&mut self.rects_under, run.take()),
            }

            let idx = r * cols + c;
            let ch = cell.c;
            if ch != ' ' && ch != '\0' && ch != '\t' && !flags.contains(Flags::HIDDEN) {
                let style = flags.contains(Flags::BOLD) as u8 | ((flags.contains(Flags::ITALIC) as u8) << 1);
                if Some(idx) == block_cursor_idx {
                    fg = th.bg;
                }
                self.keys[idx] = key_hash(ch, fg, style);
                self.tint[idx] = crate::theme::tint(fg);
                self.pending.push(Pending { idx: idx as u32, row: r as u16, col: c as u16, ch, style, span: span as u8, fg });
                // Combining marks (accents and the like) ride on the cell's character.
                for &mark in cell.zerowidth().into_iter().flatten().filter(|m| !crate::fonts::is_invisible(**m)) {
                    self.pending.push(Pending { idx: idx as u32, row: r as u16, col: c as u16, ch: mark, style: style | crate::fonts::MARK, span: span as u8, fg });
                }
            }

            if flags.intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT) {
                let x = pad[0] + c as f32 * m.cell_w;
                let y = pad[1] + r as f32 * m.cell_h;
                let w = span as f32 * m.cell_w;
                let ul = cell.underline_color().map(|c| resolve(c, colors, &th)).unwrap_or(fg);
                let line = |v: &mut Vec<RectInst>, x: f32, yy: f32, w: f32, h: f32, col: C3, a: f32| {
                    v.push(RectInst { pos: [x, yy], size: [w, h], color: lin(col, a), params: [0.0; 4] });
                };
                if flags.contains(Flags::STRIKEOUT) {
                    line(&mut self.rects_over, x, y + m.strike_y, w, m.ul_h, fg, 1.0);
                }
                if flags.contains(Flags::DOUBLE_UNDERLINE) {
                    line(&mut self.rects_over, x, y + m.ul_y - m.ul_h, w, m.ul_h, ul, 1.0);
                    line(&mut self.rects_over, x, y + m.ul_y + m.ul_h, w, m.ul_h, ul, 1.0);
                } else if let Some(shape) = crate::deco::Deco::of(flags) {
                    // The curl, dots and dashes are shapes, drawn once into the
                    // atlas (`deco.rs`) and placed like glyphs, one tile per
                    // cell; they arrive with the text they sit under.
                    for k in 0..span.min(cols - c) {
                        self.pending.push(Pending { idx: idx as u32, row: r as u16, col: (c + k) as u16, ch: '\0', style: shape.style(), span: 1, fg: ul });
                    }
                } else if flags.contains(Flags::UNDERLINE) {
                    line(&mut self.rects_over, x, y + m.ul_y, w, m.ul_h, ul, 1.0);
                }
            }
            if let Some(h) = cell.hyperlink() {
                self.link_cells.push(link::LinkCell::new(r, c, span, fg, &h, flags));
            }
        }
        flush(&mut self.rects_under, run.take());
        self.link_lines();

        // ---- reveal: decide which cells are newly born ----
        if self.birth.len() != n {
            self.birth = vec![-1.0; n];
            self.energy = vec![0.0; n];
            self.prev_keys = vec![0; n];
            self.row_hash = vec![BLANK_ROW; rows];
        }
        let new_hash: Vec<u64> = (0..rows).map(|r| hash_row(&self.keys[r * cols..(r + 1) * cols])).collect();
        // With the reveal turned off the bookkeeping still runs, so arrivals
        // are still counted (and heard); they are just not animated.
        let fresh = self.prev_dims != (cols, rows) || self.prev_offset != offset || self.suppress_reveal;
        self.arrivals = Arrivals::default();
        if fresh {
            self.birth.iter_mut().for_each(|b| *b = -1.0);
            self.energy.iter_mut().for_each(|e| *e = 0.0);
        } else {
            let k = detect_scroll(&self.row_hash, &new_hash);
            if k > 0 {
                // Part of the screen moved up while other rows (a status line,
                // an input box) may have stayed put: carry each row's history
                // from wherever that row was before.
                let (keys, prev) = (&self.keys, &self.prev_keys);
                let same = |row: usize, src: usize| (0..cols).filter(|&c| keys[row * cols + c] == prev[src * cols + c]).count();
                let sources = row_sources(&self.row_hash, &new_hash, k, same);
                let (mut pk, mut pb, mut pe) = (vec![0u32; n], vec![-1.0f32; n], vec![0.0f32; n]);
                for (i, src) in sources.iter().enumerate() {
                    if let Some(s) = *src {
                        let (d, f) = (i * cols, s * cols);
                        pk[d..d + cols].copy_from_slice(&self.prev_keys[f..f + cols]);
                        pb[d..d + cols].copy_from_slice(&self.birth[f..f + cols]);
                        pe[d..d + cols].copy_from_slice(&self.energy[f..f + cols]);
                    }
                }
                self.prev_keys = pk;
                self.birth = pb;
                self.energy = pe;
            }
            // Only text landing in an empty cell is an arrival. A cell whose
            // content changes in place (a spinner turning, a highlight moving,
            // a line being edited) swaps instantly: no motion, no light, and
            // so no extra frames however long an agent keeps a spinner going.
            let born = (0..n).filter(|&i| self.keys[i] != 0 && self.prev_keys[i] == 0).count();
            // A screenful arriving at once should not flash: calm it right down.
            let bulk = born as f32 > n as f32 * 0.30;
            let cur = cursor_vp.map(|p| (p.line, p.column.0));
            let mut latest = t;
            let (mut typed_n, mut col_sum, mut red_n, mut green_n) = (0u32, 0u64, 0u32, 0u32);
            for r in 0..rows {
                let mut order = 0u32;
                for c in 0..cols {
                    let i = r * cols + c;
                    let key = self.keys[i];
                    if key == self.prev_keys[i] {
                        continue;
                    }
                    if key == 0 || self.prev_keys[i] != 0 {
                        self.birth[i] = -1.0;
                        self.energy[i] = 0.0;
                        continue;
                    }
                    // Characters landing beside the cursor are what the
                    // person just typed: instant and at full strength.
                    let typed = cur.is_some_and(|(cr, cc)| cr == r && (cc as isize - c as isize).abs() <= 2);
                    typed_n += typed as u32;
                    red_n += (self.tint[i] < 0) as u32;
                    green_n += (self.tint[i] > 0) as u32;
                    col_sum += c as u64;
                    if !self.reveal_enabled || self.still {
                        self.birth[i] = -1.0;
                        self.energy[i] = 0.0;
                        continue;
                    }
                    let (energy, delay) = if bulk {
                        (0.20, 0.0)
                    } else if typed {
                        (1.0, 0.0)
                    } else {
                        (0.75, (order as f32 * STAGGER).min(STAGGER_MAX))
                    };
                    self.birth[i] = t + delay;
                    self.energy[i] = energy;
                    latest = latest.max(t + delay);
                    order += 1;
                }
            }
            if born > 0 {
                let mean = col_sum as f32 / born as f32;
                self.arrivals = Arrivals { cells: born as u32, typed: typed_n, red: red_n, green: green_n, bulk, pan: if cols > 1 { mean / (cols - 1) as f32 * 2.0 - 1.0 } else { 0.0 } };
            }
            if born > 0 && self.reveal_enabled && !self.still {
                self.anim_until = self.anim_until.max(latest + REVEAL_DUR);
                // The light those glyphs cast outlives their own reveal.
                self.field_until = self.field_until.max(latest + REVEAL_DUR + FIELD_TAIL);
            }
        }
        self.prev_keys.copy_from_slice(&self.keys);
        self.row_hash = new_hash;
        self.prev_dims = (cols, rows);
        self.prev_offset = offset;
        self.suppress_reveal = false;

        // ---- glyph instances ----
        self.refresh_strip(device, queue);
        let pending = std::mem::take(&mut self.pending);
        for _attempt in 0..2 {
            self.glyphs.clear();
            let generation = self.atlas.generation;
            for p in &pending {
                let slot = self.atlas.get(queue, &mut self.fonts, GlyphKey { ch: p.ch, style: p.style, span: p.span });
                if slot.empty {
                    continue;
                }
                let i = p.idx as usize;
                self.glyphs.push(GlyphInst {
                    pos: [pad[0] + p.col as f32 * m.cell_w + slot.off[0], pad[1] + p.row as f32 * m.cell_h + slot.off[1]],
                    size: slot.size,
                    uv: slot.uv,
                    color: lin(p.fg, 1.0),
                    params: [self.birth[i], self.energy[i], 0.0, 0.0],
                });
            }
            // The atlas was wiped mid-build: earlier slots are stale, go again.
            if self.atlas.generation == generation {
                break;
            }
        }
        self.pending = pending;
        // If the grid's glyphs wiped the atlas, the strip's are gone too.
        self.refresh_strip(device, queue);

        // ---- cursor target ----
        self.cursor = match (cursor_vp, cursor_shape) {
            (_, CursorShape::Hidden) | (None, _) => None,
            (Some(p), shape) => Some(CursorTarget {
                x: pad[0] + p.column.0 as f32 * m.cell_w,
                y: pad[1] + p.line as f32 * m.cell_h,
                shape: if focused { shape } else { CursorShape::HollowBlock },
            }),
        };

        // ---- upload ----
        let n_rects = self.rects_under.len() + self.rects_over.len();
        if n_rects > self.rect_cap {
            self.rect_cap = n_rects.next_power_of_two();
            self.rect_buf = make_buffer(device, "rects", (self.rect_cap * std::mem::size_of::<RectInst>()) as u64);
        }
        if self.glyphs.len() > self.glyph_cap {
            self.glyph_cap = self.glyphs.len().next_power_of_two();
            self.glyph_buf = make_buffer(device, "glyphs", (self.glyph_cap * std::mem::size_of::<GlyphInst>()) as u64);
        }
        if !self.rects_under.is_empty() {
            queue.write_buffer(&self.rect_buf, 0, bytemuck::cast_slice(&self.rects_under));
        }
        if !self.rects_over.is_empty() {
            queue.write_buffer(
                &self.rect_buf,
                (self.rects_under.len() * std::mem::size_of::<RectInst>()) as u64,
                bytemuck::cast_slice(&self.rects_over),
            );
        }
        if !self.glyphs.is_empty() {
            queue.write_buffer(&self.glyph_buf, 0, bytemuck::cast_slice(&self.glyphs));
        }
        self.n_under = self.rects_under.len() as u32;
        self.n_over = self.rects_over.len() as u32;
        self.n_glyphs = self.glyphs.len() as u32;
    }

    /// Advance the cursor's motion and produce its instances.
    fn update_cursor(&mut self, queue: &wgpu::Queue, t: f32) -> bool {
        let dt = (t - self.last_t).clamp(0.0, 0.1);
        self.last_t = t;
        self.cursor_energy *= (-dt * 6.5).exp();
        let m = self.fonts.metrics;
        let Some(target) = self.cursor else {
            self.n_cursor = 0;
            self.cursor_pos = None;
            self.cursor_moving = false;
            self.cursor_geom = None;
            return false;
        };
        // Glide to the new position instead of teleporting.
        let pos = match self.cursor_pos {
            None => [target.x, target.y],
            Some(_) if self.still => [target.x, target.y],
            Some(p) => {
                let k = 1.0 - (-dt * 34.0).exp();
                let np = [p[0] + (target.x - p[0]) * k, p[1] + (target.y - p[1]) * k];
                if (np[0] - target.x).abs() + (np[1] - target.y).abs() < 0.35 {
                    [target.x, target.y]
                } else {
                    np
                }
            }
        };
        self.cursor_moving = pos != [target.x, target.y];
        self.cursor_pos = Some(pos);

        let col = lin(self.theme.cursor, 1.0);
        let beam_w = (m.cell_w * 0.12).round().max(2.0);
        let (p, size, radius, ring, under) = match target.shape {
            CursorShape::Block => (pos, [m.cell_w, m.cell_h], 2.0, 0.0, true),
            CursorShape::HollowBlock => (pos, [m.cell_w, m.cell_h], 2.0, m.ul_h.max(1.0), false),
            CursorShape::Underline => ([pos[0], pos[1] + m.cell_h - m.ul_h * 2.0], [m.cell_w, m.ul_h * 2.0], 1.0, 0.0, false),
            _ => (pos, [beam_w, m.cell_h], beam_w * 0.5, 0.0, false),
        };
        let glow = 0.10 + 0.34 * self.cursor_energy.min(1.0);
        let insts = [
            RectInst { pos: p, size, color: col, params: [radius, 0.0, m.cell_h * 0.7, glow] },
            RectInst { pos: p, size, color: col, params: [radius, ring, 0.0, 0.0] },
        ];
        queue.write_buffer(&self.cursor_buf, 0, bytemuck::cast_slice(&insts));
        self.n_cursor = 2;
        self.cursor_geom = Some((p, size, radius));
        under
    }

    fn write_globals(&self, queue: &wgpu::Queue, t: f32, field: [f32; 4]) {
        let m = self.fonts.metrics;
        let globals = Globals {
            screen: self.screen,
            time: t,
            reveal_dur: REVEAL_DUR,
            accent: lin(self.theme.accent, 1.0),
            bg: lin(self.theme.bg, 1.0),
            cursor: lin(self.theme.cursor, 1.0),
            misc: [m.cell_h * 0.16, 0.0, 0.0, 0.0],
            field,
        };
        queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
    }

    fn clear_field(&self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("field clear") });
        for view in &self.field_views {
            let _ = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("field clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }
        queue.submit([enc.finish()]);
    }

    /// One step of the light field: last step's light spreads and fades into
    /// the other buffer, then everything still hot adds its emission.
    fn field_pass(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, t: f32, dt: f32) {
        // Written per 120 Hz step; scale so other frame rates look the same.
        let rate = (dt * 120.0).min(3.0);
        self.write_globals(queue, t, [FIELD_EMIT * dt, 0.0, (-dt / FIELD_TAU).exp(), 0.22 * rate]);
        let mut cursor_emits = false;
        if let Some((pos, size, radius)) = self.cursor_geom {
            if self.cursor_energy > 0.02 {
                let inst = RectInst {
                    pos,
                    size,
                    color: lin(self.theme.cursor, 1.0),
                    params: [radius, 0.0, self.fonts.metrics.cell_h * 1.1, 0.55 * self.cursor_energy.min(1.2) * rate],
                };
                queue.write_buffer(&self.cursor_buf, 2 * std::mem::size_of::<RectInst>() as u64, bytemuck::bytes_of(&inst));
                cursor_emits = true;
            }
        }
        let next = 1 - self.field_cur;
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("field") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("field"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.field_views[next],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_bind_group(0, &self.globals_bg, &[]);
            pass.set_bind_group(1, &self.atlas_bg, &[]);
            pass.set_bind_group(2, &self.field_bgs[self.field_cur], &[]);
            pass.set_pipeline(&self.step_pipe);
            pass.draw(0..3, 0..1);
            if self.n_glyphs > 0 {
                pass.set_pipeline(&self.emit_glyph_pipe);
                pass.set_vertex_buffer(0, self.glyph_buf.slice(..));
                pass.draw(0..4, 0..self.n_glyphs);
            }
            if cursor_emits {
                pass.set_pipeline(&self.emit_rect_pipe);
                pass.set_vertex_buffer(0, self.cursor_buf.slice(..));
                pass.draw(0..4, 2..3);
            }
        }
        queue.submit([enc.finish()]);
        self.field_cur = next;
    }

    /// Advance the field for this frame, or leave it dark and untouched.
    fn step_field(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, t: f32) {
        if self.light <= 0.0 || self.still {
            self.field_live = false;
            return;
        }
        if self.cursor.is_some() && self.cursor_energy > 0.05 {
            self.field_until = self.field_until.max(t + FIELD_TAIL);
        }
        if t >= self.field_until {
            self.field_live = false;
            return;
        }
        if !self.field_live {
            self.clear_field(device, queue);
            self.field_live = true;
            self.field_t = t;
        }
        let dt = (t - self.field_t).clamp(0.0, 0.05);
        self.field_t = t;
        self.field_pass(device, queue, t, dt);
    }

    /// Run the field forward at a fixed step, for screenshots: the same
    /// input always produces the same picture.
    pub fn simulate_field(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, from: f64, to: f64) {
        let (t0, t1) = (self.t32(from), self.t32(to));
        if self.light <= 0.0 || self.still || t0 >= self.field_until || t1 <= t0 {
            return;
        }
        self.clear_field(device, queue);
        self.field_live = true;
        let mut t = t0;
        while t < t1 - 1e-5 {
            let dt = (1.0f32 / 120.0).min(t1 - t);
            t += dt;
            self.field_pass(device, queue, t, dt);
        }
        self.field_t = t1;
    }

    pub fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, view: &wgpu::TextureView, now: f64) {
        let t = self.t32(now);
        let cursor_under = self.update_cursor(queue, t);
        self.step_field(device, queue, t);
        let shown = if self.field_live { self.light * 0.9 } else { 0.0 };
        self.write_globals(queue, t, [0.0, shown, 1.0, 0.0]);

        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_bind_group(0, &self.globals_bg, &[]);
            pass.set_bind_group(1, &self.atlas_bg, &[]);
            pass.set_bind_group(2, &self.field_bgs[self.field_cur], &[]);
            pass.set_pipeline(&self.bg_pipe);
            pass.draw(0..3, 0..1);

            pass.set_pipeline(&self.rect_pipe);
            if self.n_under > 0 {
                pass.set_vertex_buffer(0, self.rect_buf.slice(..));
                pass.draw(0..4, 0..self.n_under);
            }
            if cursor_under && self.n_cursor > 0 {
                pass.set_vertex_buffer(0, self.cursor_buf.slice(..));
                pass.draw(0..4, 0..self.n_cursor);
            }
            if self.n_glyphs > 0 {
                pass.set_pipeline(&self.glyph_pipe);
                pass.set_vertex_buffer(0, self.glyph_buf.slice(..));
                pass.draw(0..4, 0..self.n_glyphs);
                pass.set_pipeline(&self.rect_pipe);
            }
            if self.n_over > 0 {
                pass.set_vertex_buffer(0, self.rect_buf.slice(..));
                pass.draw(0..4, self.n_under..self.n_under + self.n_over);
            }
            if !cursor_under && self.n_cursor > 0 {
                pass.set_vertex_buffer(0, self.cursor_buf.slice(..));
                pass.draw(0..4, 0..self.n_cursor);
            }
            // The overlay goes over everything: its shapes, then its text.
            if let (Some(b), true) = (&self.overlay.rects, self.overlay.n_rects > 0) {
                pass.set_pipeline(&self.rect_pipe);
                pass.set_vertex_buffer(0, b.slice(..));
                pass.draw(0..4, 0..self.overlay.n_rects);
            }
            if let (Some(b), true) = (&self.overlay.glyphs, self.overlay.n_glyphs > 0) {
                pass.set_pipeline(&self.glyph_pipe);
                pass.set_vertex_buffer(0, b.slice(..));
                pass.draw(0..4, 0..self.overlay.n_glyphs);
            }
            self.draw_strip(&mut pass);
        }
        queue.submit([enc.finish()]);
    }

    /// Render one frame off screen and read it back as RGBA bytes.
    pub fn capture(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, now: f64) -> (u32, u32, Vec<u8>) {
        let (w, h) = (self.screen[0] as u32, self.screen[1] as u32);
        let extent = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("capture"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        self.render(device, queue, &view, now);

        let bpr = (w * 4).div_ceil(256) * 256;
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture"),
            size: (bpr * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("capture") });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(h) },
            },
            extent,
        );
        queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h {
            let s = (row * bpr) as usize;
            out.extend_from_slice(&data[s..s + (w * 4) as usize]);
        }
        drop(data);
        buf.unmap();
        if matches!(self.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb) {
            out.chunks_exact_mut(4).for_each(|p| p.swap(0, 2));
        }
        (w, h, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_is_detected_and_static_screens_are_not() {
        let old = vec![1u64, 2, 3, 4, 5, BLANK_ROW];
        assert_eq!(detect_scroll(&old, &old.clone()), 0);
        assert_eq!(detect_scroll(&old, &[2, 3, 4, 5, 6, BLANK_ROW]), 1);
        assert_eq!(detect_scroll(&old, &[4, 5, 9, 9, 9, 9]), 3);
        // One changed line is an edit in place, not a scroll.
        assert_eq!(detect_scroll(&old, &[1, 2, 9, 4, 5, BLANK_ROW]), 0);
    }

    /// An agent's screen: the conversation scrolls up one line while the
    /// input box and status line at the bottom stay put. Nothing that was
    /// already on screen may count as new.
    #[test]
    fn scrolling_body_with_fixed_footer_keeps_every_rows_history() {
        let old = vec![10u64, 11, 12, 13, 90, 91];
        let new = vec![11u64, 12, 13, 14, 90, 91];
        let k = detect_scroll(&old, &new);
        assert_eq!(k, 1);
        let src = row_sources(&old, &new, k, |_, _| 0);
        assert_eq!(src, vec![Some(1), Some(2), Some(3), None, Some(4), Some(5)]);
        // A row that moved and was edited on the way follows whichever old
        // row it shares more cells with.
        let new2 = vec![11u64, 77, 13, 14, 90, 91];
        let src2 = row_sources(&old, &new2, 1, |row, s| if row == 1 && s == 2 { 5 } else { 1 });
        assert_eq!(src2[1], Some(2));
        // A full-screen scroll: the last row is new.
        assert_eq!(row_sources(&[1, 2, 3], &[2, 3, 4], 1, |_, _| 0), vec![Some(1), Some(2), None]);
    }

    #[test]
    fn keys_distinguish_content_and_are_never_zero() {
        let a = key_hash('a', [255, 255, 255], 0);
        assert_ne!(a, 0);
        assert_ne!(a, key_hash('b', [255, 255, 255], 0));
        assert_ne!(a, key_hash('a', [255, 0, 255], 0));
        assert_ne!(a, key_hash('a', [255, 255, 255], 1));
        assert_eq!(hash_row(&[0, 0, 0]), BLANK_ROW);
        assert_ne!(hash_row(&[a, 0, 0]), hash_row(&[0, a, 0]));
    }
}
