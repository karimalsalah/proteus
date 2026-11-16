//! Flash safety (WCAG 2.3.1, "Three Flashes or Below Threshold").
//!
//!   proteus shot --flash --vt FILE [--every FRAMES] [--lines N] [--seconds S]
//!                [--prefill N] [--fps F] [--light X] [--still] [--size WxH]
//!                [--scale S] [--film OUT.png]
//!
//! Plays a VT file into the real renderer the way output reaches a window:
//! every `--every` frames (fractions allowed) the next `--lines` lines
//! arrive, the grid is built, and a frame is drawn at `--fps` with the light
//! field stepped exactly as the window steps it. Every frame is read back
//! and analysed; one JSON object is printed. `--film` writes a strip of
//! frames to look at.
//!
//! The analysis follows the WCAG 2.2 definitions:
//! * a general transition is a change of relative luminance of 0.1 or more
//!   where the darker state is below 0.8;
//! * a red transition is one to or from a saturated red (R/(R+G+B) >= 0.8,
//!   linear values) between states more than 0.2 apart in CIE 1976 u'v';
//! * a flash is a pair of opposing transitions; it counts only if the area
//!   changing together covers at least 25% of a 10-degree field of view,
//!   taken (as the WCAG note does for 1024x768) as a third of the picture's
//!   width by a third of its height, anywhere on it (25 overlapping fields).
//!
//! In each field two things are watched. Changes from one frame to the next
//! that cover 25% of the field in the same direction are transitions (a
//! hard cut, a pattern inverting). Slower changes are found from the field's
//! mean: when it has moved by a quarter of the threshold from its last peak
//! or valley, the pixels are compared with that peak or valley, and if 25%
//! of the field has changed by a full transition it counts. Transitions in
//! one field alternate in direction. The rule fails when more than six
//! transitions (more than three flashes) fall within any one second.
use std::path::PathBuf;
use std::sync::Arc;

use crate::gpu;
use crate::renderer::Renderer;
use crate::term::Terminal;
use crate::theme::Theme;
use crate::Res;

/// Relative luminance change of a general transition.
const GENERAL: f32 = 0.1;
/// The darker state must be below this.
const DARKER: f32 = 0.8;
/// Saturated red: R / (R + G + B) at least this.
const SATURATED: f32 = 0.8;
/// Chromaticity distance (u'v') of a red transition.
const RED_UV: f32 = 0.2;
/// Share of a 10-degree field that must change together.
const AREA: f32 = 0.25;
/// More than this many transitions in one second fails (three flashes).
const MAX_TRANSITIONS: usize = 6;
/// Pixels are averaged in blocks of this many on a side before analysis.
const BLOCK: usize = 4;
/// D65 white in u'v', used for black (which has no colour of its own).
const WHITE_UV: (f32, f32) = (0.1978, 0.4683);

#[derive(Default)]
struct Track {
    started: bool,
    dir: i8,
    hi: f32,
    lo: f32,
    events: Vec<usize>,
}

/// One 10-degree field.
struct Field {
    x: usize,
    y: usize,
    general: Track,
    red: Track,
    // The field's pixels at its last peak and valley (luminance; red
    // flag and chromaticity).
    l_hi: Vec<f32>,
    l_lo: Vec<f32>,
    c_hi: Vec<(bool, f32, f32)>,
    c_lo: Vec<(bool, f32, f32)>,
}

pub struct Analyser {
    w: usize,
    h: usize,
    fw: usize,
    fh: usize,
    lut: [f32; 256],
    l: Vec<f32>,
    uv: Vec<(f32, f32)>,
    red: Vec<bool>,
    prev_l: Vec<f32>,
    prev_uv: Vec<(f32, f32)>,
    prev_red: Vec<bool>,
    /// Integral images: luminance, red pixels, and frame-to-frame
    /// transitions (luminance up, down; into red, out of red).
    sums: [Vec<f64>; 6],
    fields: Vec<Field>,
    pub frames: usize,
}

/// The result for one kind of transition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Worst1 {
    /// Most transitions within any one second, in any field.
    pub max: usize,
    pub field: usize,
    /// Frame at which that second starts.
    pub frame: usize,
    pub total: usize,
}

fn worst(fields: &[Field], fps: usize, pick: impl Fn(&Field) -> &Track) -> Worst1 {
    let mut w = Worst1 { max: 0, field: 0, frame: 0, total: 0 };
    for (i, f) in fields.iter().enumerate() {
        let ev = &pick(f).events;
        w.total = w.total.max(ev.len());
        let mut j = 0;
        for (k, &start) in ev.iter().enumerate() {
            j = j.max(k);
            while j < ev.len() && ev[j] < start + fps {
                j += 1;
            }
            if j - k > w.max {
                w = Worst1 { max: j - k, field: i, frame: start, total: w.total };
            }
        }
    }
    w
}

impl Analyser {
    /// For frames of `fw` x `fh` RGBA pixels (sRGB).
    pub fn new(fw: usize, fh: usize) -> Self {
        let (w, h) = ((fw / BLOCK).max(6), (fh / BLOCK).max(6));
        let (tw, th) = (w / 3, h / 3);
        let mut fields = Vec::new();
        for j in 0..5 {
            for i in 0..5 {
                let (x, y) = (((w - tw) * i) / 4, ((h - th) * j) / 4);
                fields.push(Field { x, y, general: Track::default(), red: Track::default(), l_hi: Vec::new(), l_lo: Vec::new(), c_hi: Vec::new(), c_lo: Vec::new() });
            }
        }
        let mut lut = [0f32; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = crate::theme::srgb_to_linear(i as u8);
        }
        let n = w * h;
        let s = (w + 1) * (h + 1);
        Self {
            w,
            h,
            fw,
            fh,
            lut,
            l: vec![0.0; n],
            uv: vec![WHITE_UV; n],
            red: vec![false; n],
            prev_l: vec![0.0; n],
            prev_uv: vec![WHITE_UV; n],
            prev_red: vec![false; n],
            sums: std::array::from_fn(|_| vec![0.0; s]),
            fields,
            frames: 0,
        }
    }

    fn field_size(&self) -> (usize, usize) {
        (self.w / 3, self.h / 3)
    }

    /// Look at the next frame.
    pub fn frame(&mut self, rgba: &[u8]) {
        let (w, h, fw) = (self.w, self.h, self.fw);
        std::mem::swap(&mut self.l, &mut self.prev_l);
        std::mem::swap(&mut self.uv, &mut self.prev_uv);
        std::mem::swap(&mut self.red, &mut self.prev_red);
        let inv = 1.0 / (BLOCK * BLOCK) as f32;
        for y in 0..h {
            for x in 0..w {
                let (mut r, mut g, mut b) = (0f32, 0f32, 0f32);
                for dy in 0..BLOCK {
                    let row = (y * BLOCK + dy).min(self.fh - 1) * fw;
                    for dx in 0..BLOCK {
                        let p = (row + (x * BLOCK + dx).min(fw - 1)) * 4;
                        r += self.lut[rgba[p] as usize];
                        g += self.lut[rgba[p + 1] as usize];
                        b += self.lut[rgba[p + 2] as usize];
                    }
                }
                let (r, g, b) = (r * inv, g * inv, b * inv);
                let i = y * w + x;
                let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                self.l[i] = lum;
                let xx = 0.4124 * r + 0.3576 * g + 0.1805 * b;
                let zz = 0.0193 * r + 0.1192 * g + 0.9505 * b;
                let d = xx + 15.0 * lum + 3.0 * zz;
                self.uv[i] = if d > 1e-5 { (4.0 * xx / d, 9.0 * lum / d) } else { WHITE_UV };
                let sum = r + g + b;
                self.red[i] = sum > 1e-4 && r / sum >= SATURATED;
            }
        }
        let first = self.frames == 0;
        // Integral images of what the fields need.
        let s = w + 1;
        for k in 0..6 {
            let (l, red, pl, puv, pred, uv) = (&self.l, &self.red, &self.prev_l, &self.prev_uv, &self.prev_red, &self.uv);
            let value = |i: usize| -> f64 {
                if first && k >= 2 {
                    return 0.0;
                }
                let far = || dist(uv[i], puv[i]) > RED_UV;
                match k {
                    0 => l[i] as f64,
                    1 => red[i] as u8 as f64,
                    2 => (l[i] - pl[i] >= GENERAL && pl[i] < DARKER) as u8 as f64,
                    3 => (pl[i] - l[i] >= GENERAL && l[i] < DARKER) as u8 as f64,
                    4 => (red[i] && !pred[i] && far()) as u8 as f64,
                    _ => (pred[i] && !red[i] && far()) as u8 as f64,
                }
            };
            let sum = &mut self.sums[k];
            for y in 0..h {
                let mut row = 0.0;
                for x in 0..w {
                    row += value(y * w + x);
                    sum[(y + 1) * s + x + 1] = sum[y * s + x + 1] + row;
                }
            }
        }
        let (tw, th) = self.field_size();
        let area = (tw * th) as f64;
        let mut fields = std::mem::take(&mut self.fields);
        let mut cur_l: Vec<f32> = Vec::with_capacity(tw * th);
        let mut cur_c: Vec<(bool, f32, f32)> = Vec::with_capacity(tw * th);
        for f in fields.iter_mut() {
            let q = |k: usize| {
                let sm = &self.sums[k];
                (sm[(f.y + th) * s + f.x + tw] - sm[f.y * s + f.x + tw] - sm[(f.y + th) * s + f.x] + sm[f.y * s + f.x]) / area
            };
            let (mean, reds, up, down, into, out) = (q(0) as f32, q(1) as f32, q(2) as f32, q(3) as f32, q(4) as f32, q(5) as f32);
            cur_l.clear();
            cur_c.clear();
            for y in f.y..f.y + th {
                for x in f.x..f.x + tw {
                    let i = y * w + x;
                    cur_l.push(self.l[i]);
                    cur_c.push((self.red[i], self.uv[i].0, self.uv[i].1));
                }
            }
            let n = self.frames;
            // General.
            let g = &mut f.general;
            if !g.started {
                *g = Track { started: true, dir: 0, hi: mean, lo: mean, events: Vec::new() };
                f.l_hi.clone_from(&cur_l);
                f.l_lo.clone_from(&cur_l);
            } else {
                if mean >= g.hi {
                    g.hi = mean;
                    f.l_hi.clone_from(&cur_l);
                }
                if mean <= g.lo {
                    g.lo = mean;
                    f.l_lo.clone_from(&cur_l);
                }
                let need = (AREA * (tw * th) as f32).ceil() as usize;
                let mut dir = 0;
                if g.dir != 1 && up >= AREA && up >= down {
                    dir = 1;
                } else if g.dir != -1 && down >= AREA {
                    dir = -1;
                } else if g.dir != 1 && mean - g.lo >= GENERAL * AREA {
                    let k = cur_l.iter().zip(&f.l_lo).filter(|(c, s)| **c - **s >= GENERAL && **s < DARKER).count();
                    if k >= need {
                        dir = 1;
                    }
                } else if g.dir != -1 && g.hi - mean >= GENERAL * AREA {
                    let k = cur_l.iter().zip(&f.l_hi).filter(|(c, s)| **s - **c >= GENERAL && **c < DARKER).count();
                    if k >= need {
                        dir = -1;
                    }
                }
                if dir != 0 {
                    g.events.push(n);
                    g.dir = dir;
                    g.hi = mean;
                    g.lo = mean;
                    f.l_hi.clone_from(&cur_l);
                    f.l_lo.clone_from(&cur_l);
                }
            }
            // Red.
            let r = &mut f.red;
            if !r.started {
                *r = Track { started: true, dir: 0, hi: reds, lo: reds, events: Vec::new() };
                f.c_hi.clone_from(&cur_c);
                f.c_lo.clone_from(&cur_c);
            } else {
                if reds >= r.hi {
                    r.hi = reds;
                    f.c_hi.clone_from(&cur_c);
                }
                if reds <= r.lo {
                    r.lo = reds;
                    f.c_lo.clone_from(&cur_c);
                }
                let need = (AREA * (tw * th) as f32).ceil() as usize;
                let apart = |a: &(bool, f32, f32), b: &(bool, f32, f32)| dist((a.1, a.2), (b.1, b.2)) > RED_UV;
                let mut dir = 0;
                if r.dir != 1 && into >= AREA && into >= out {
                    dir = 1;
                } else if r.dir != -1 && out >= AREA {
                    dir = -1;
                } else if r.dir != 1 && reds - r.lo >= AREA * 0.8 {
                    let k = cur_c.iter().zip(&f.c_lo).filter(|(c, s)| c.0 && !s.0 && apart(c, s)).count();
                    if k >= need {
                        dir = 1;
                    }
                } else if r.dir != -1 && r.hi - reds >= AREA * 0.8 {
                    let k = cur_c.iter().zip(&f.c_hi).filter(|(c, s)| s.0 && !c.0 && apart(c, s)).count();
                    if k >= need {
                        dir = -1;
                    }
                }
                if dir != 0 {
                    r.events.push(n);
                    r.dir = dir;
                    r.hi = reds;
                    r.lo = reds;
                    f.c_hi.clone_from(&cur_c);
                    f.c_lo.clone_from(&cur_c);
                }
            }
        }
        self.fields = fields;
        self.frames += 1;
    }

    /// The worst second for general and for red transitions.
    pub fn result(&self, fps: usize) -> (Worst1, Worst1) {
        (worst(&self.fields, fps, |f| &f.general), worst(&self.fields, fps, |f| &f.red))
    }

    pub fn analysed(&self) -> (usize, usize) {
        (self.w, self.h)
    }
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Frames read back through one texture and one buffer, made once (a fresh
/// pair per frame, as a single screenshot uses, costs more than the frame).
struct Capturer {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    buf: wgpu::Buffer,
    w: u32,
    h: u32,
    bpr: u32,
}

impl Capturer {
    fn new(device: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat) -> Self {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("flash frame"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bpr = (w * 4).div_ceil(256) * 256;
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("flash readback"),
            size: (bpr * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self { tex, view, buf, w, h, bpr }
    }

    /// Draw the frame at `now` exactly as the window would, and read it back.
    fn frame(&self, r: &mut Renderer, device: &wgpu::Device, queue: &wgpu::Queue, now: f64, out: &mut Vec<u8>) {
        r.render(device, queue, &self.view, now);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("flash readback") });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(self.bpr), rows_per_image: Some(self.h) },
            },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
        queue.submit([enc.finish()]);
        let slice = self.buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        out.clear();
        {
            let data = slice.get_mapped_range();
            for row in 0..self.h {
                let s = (row * self.bpr) as usize;
                out.extend_from_slice(&data[s..s + (self.w * 4) as usize]);
            }
        }
        self.buf.unmap();
    }
}

/// Does this result break the rule?
pub fn fails(w: &Worst1) -> bool {
    w.max > MAX_TRANSITIONS
}

pub fn run(args: &[String]) -> Res<()> {
    let mut vt: Option<PathBuf> = None;
    let mut film: Option<PathBuf> = None;
    let (mut every, mut lines, mut seconds, mut fps, mut prefill) = (1.0f64, 1usize, 4.0f64, 60usize, 0usize);
    let (mut light, mut still) = (3.0f32, false);
    let (mut w, mut h, mut scale, mut font_pt) = (2200u32, 1300u32, 2.0f32, 15.0f32);
    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--flash" => {
                i += 1;
                continue;
            }
            "--still" => {
                still = true;
                i += 1;
                continue;
            }
            "--vt" => vt = Some(PathBuf::from(&val)),
            "--film" => film = Some(PathBuf::from(&val)),
            "--every" => every = val.parse()?,
            "--lines" => lines = val.parse()?,
            "--prefill" => prefill = val.parse()?,
            "--seconds" => seconds = val.parse()?,
            "--fps" => fps = val.parse()?,
            "--light" => light = val.parse()?,
            "--scale" => scale = val.parse()?,
            "--font" => font_pt = val.parse()?,
            "--size" => {
                let (a, b) = val.split_once('x').ok_or("size must be WxH")?;
                w = a.parse()?;
                h = b.parse()?;
            }
            other => return Err(format!("unknown option {other}").into()),
        }
        i += 2;
    }
    let path = vt.ok_or("--flash needs --vt FILE")?;
    if !(every > 0.0) || lines == 0 || fps == 0 {
        return Err("--every, --lines and --fps must be positive".into());
    }
    let bytes = std::fs::read(&path)?;
    let chunks: Vec<&[u8]> = bytes.split_inclusive(|b| *b == b'\n').collect();
    if chunks.is_empty() {
        return Err("the sample is empty".into());
    }

    let gpu = gpu::init(gpu::instance(), None)?;
    let mut r = Renderer::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb, font_pt * scale, Theme::void_light());
    r.resize(&gpu.device, w, h, 12.0 * scale);
    r.light = light.clamp(0.0, 3.0);
    r.reveal_enabled = true;
    r.set_still(still);
    let (cols, rows) = r.grid();
    let term = Terminal::new(cols, rows, Theme::void_light(), Arc::new(|_| {}), None);
    let mut a = Analyser::new(w as usize, h as usize);
    let frames = (seconds * fps as f64).round() as usize;
    let (mut next, mut group, mut builds, mut owed) = (0usize, 0usize, 0usize, 0usize);
    let mut shots: Vec<(usize, Vec<u8>)> = Vec::new();
    let film_at: Vec<usize> = (0..8).map(|k| k * frames / 8 + fps / 12).collect();
    let t0 = std::time::Instant::now();
    let (mut capture_s, mut analyse_s) = (0f64, 0f64);
    let cap = Capturer::new(&gpu.device, w, h, wgpu::TextureFormat::Rgba8UnormSrgb);
    let mut rgba: Vec<u8> = Vec::with_capacity((w * h * 4) as usize);
    // The screen already full when the stream starts (`--prefill N` lines),
    // shown settled, so every arrival scrolls as in a long build.
    if prefill > 0 {
        for _ in 0..prefill {
            term.feed(chunks[next % chunks.len()]);
            next += 1;
        }
    }
    for f in 0..frames {
        let t = 1.0 + f as f64 / fps as f64;
        let mut fed = false;
        while (group as f64 * every).round() as usize <= f {
            for _ in 0..lines {
                term.feed(chunks[next % chunks.len()]);
                next += 1;
            }
            group += 1;
            fed = true;
        }
        if fed || f == 0 {
            let g = term.term.lock().map_err(|_| "terminal lock")?;
            r.build(&gpu.device, &gpu.queue, &g, t, true);
            builds += 1;
        }
        // Frames the window would have drawn: one per change, and any the
        // renderer still owes for motion.
        owed += r.animating(t) as usize;
        let c0 = std::time::Instant::now();
        cap.frame(&mut r, &gpu.device, &gpu.queue, t, &mut rgba);
        let c1 = std::time::Instant::now();
        a.frame(&rgba);
        capture_s += (c1 - c0).as_secs_f64();
        analyse_s += c1.elapsed().as_secs_f64();
        if film.is_some() && film_at.contains(&f) {
            shots.push((f, rgba.clone()));
        }
    }
    let (g, red) = a.result(fps);
    let pass = !fails(&g) && !fails(&red);
    let side = |x: &Worst1| {
        serde_json::json!({
            "max_transitions_in_1s": x.max,
            "flashes": x.max as f32 / 2.0,
            "field": x.field,
            "at_s": x.frame as f64 / fps as f64,
            "most_in_one_field": x.total,
        })
    };
    let (aw, ah) = a.analysed();
    let v = serde_json::json!({
        "sample": path.file_name().map(|n| n.to_string_lossy().into_owned()),
        "frames": frames,
        "fps": fps,
        "every_frames": every,
        "lines": lines,
        "light": r.light,
        "still": still,
        "size": [w, h],
        "analysed": [aw, ah],
        "fields": 25,
        "builds": builds,
        "frames_with_motion_owed": owed,
        "general": side(&g),
        "red": side(&red),
        "pass": pass,
        "rule": "fail if more than 6 transitions (3 flashes) covering 25% of a 10-degree field fall in any 1 s (WCAG 2.3.1)",
        "prefill": prefill,
        "seconds_taken": t0.elapsed().as_secs_f32(),
        "seconds_capturing": capture_s as f32,
        "seconds_analysing": analyse_s as f32,
    });
    if let Some(out) = film {
        write_film(&out, w as usize, h as usize, &shots, fps)?;
    }
    println!("{}", serde_json::to_string(&v).unwrap_or_default());
    Ok(())
}

/// Eight frames, a quarter size each, in two rows of four.
fn write_film(out: &std::path::Path, w: usize, h: usize, shots: &[(usize, Vec<u8>)], _fps: usize) -> Res<()> {
    let (sw, sh) = (w / 4, h / 4);
    let (fw, fh) = (sw * 4 + 3 * 8, sh * 2 + 8);
    let mut img = vec![0u8; fw * fh * 4];
    for (k, (_, rgba)) in shots.iter().take(8).enumerate() {
        let (ox, oy) = ((k % 4) * (sw + 8), (k / 4) * (sh + 8));
        for y in 0..sh {
            for x in 0..sw {
                let s = ((y * 4) * w + x * 4) * 4;
                let d = ((oy + y) * fw + ox + x) * 4;
                img[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
            }
        }
    }
    crate::shot::write_png(out, fw as u32, fh as u32, &img)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames of one colour each, with an optional block of another.
    fn solid(w: usize, h: usize, c: [u8; 3]) -> Vec<u8> {
        (0..w * h).flat_map(|_| [c[0], c[1], c[2], 255]).collect()
    }

    fn play(w: usize, h: usize, frames: usize, f: impl Fn(usize) -> Vec<u8>) -> (Worst1, Worst1) {
        let mut a = Analyser::new(w, h);
        for i in 0..frames {
            a.frame(&f(i));
        }
        a.result(60)
    }

    const BLACK: [u8; 3] = [5, 7, 13];
    const RED: [u8; 3] = [255, 0, 0];

    /// The roadmap's kill criterion: a full-screen red and black alternation
    /// at 4 Hz must be flagged, as both a general and a red flash.
    #[test]
    fn four_hertz_red_and_black_is_flagged() {
        let (w, h) = (480, 320);
        let (red_frame, black) = (solid(w, h, RED), solid(w, h, BLACK));
        let (g, r) = play(w, h, 120, |i| if (i as f64 / 7.5) as usize % 2 == 0 { red_frame.clone() } else { black.clone() });
        assert!(fails(&g) && fails(&r), "general {g:?} red {r:?}");
        assert_eq!(g.max, 8, "{g:?}");
    }

    /// Three a second is allowed: the same alternation at 3 Hz passes.
    #[test]
    fn three_hertz_passes() {
        let (w, h) = (480, 320);
        let (white, black) = (solid(w, h, [255, 255, 255]), solid(w, h, BLACK));
        let (g, r) = play(w, h, 180, |i| if (i / 10) % 2 == 0 { white.clone() } else { black.clone() });
        assert_eq!(g.max, 6, "{g:?}");
        assert!(!fails(&g) && r.max == 0);
    }

    /// A smooth 5 Hz pulse has no hard cut in it and must still be caught.
    #[test]
    fn a_smooth_pulse_is_caught_by_its_peaks_and_valleys() {
        let (w, h) = (480, 320);
        let (g, _) = play(w, h, 120, |i| {
            let v = ((i as f32 / 12.0 * std::f32::consts::TAU).sin() * 0.5 + 0.5) * 140.0 + 10.0;
            solid(w, h, [v as u8; 3])
        });
        assert!(fails(&g), "{g:?}");
    }

    /// Flashing on a small part of the screen is not a flash by the rule.
    #[test]
    fn a_small_flashing_area_passes() {
        let (w, h) = (480, 320);
        let (g, r) = play(w, h, 120, |i| {
            let mut f = solid(w, h, BLACK);
            if (i / 3) % 2 == 0 {
                for y in 100..130 {
                    for x in 200..240 {
                        f[(y * w + x) * 4..(y * w + x) * 4 + 3].copy_from_slice(&RED);
                    }
                }
            }
            f
        });
        assert!(!fails(&g) && !fails(&r), "{g:?} {r:?}");
    }

    /// A fine pattern inverting every frame keeps every field's mean the
    /// same, but half of every field changes at once: it flashes.
    #[test]
    fn an_inverting_pattern_is_caught_though_its_mean_never_moves() {
        let (w, h) = (480, 320);
        let (g, _) = play(w, h, 60, |i| {
            (0..w * h)
                .flat_map(|p| {
                    let on = ((p % w / 8 + p / w / 8 + i) % 2) == 0;
                    let v = if on { 230 } else { 10 };
                    [v, v, v, 255]
                })
                .collect()
        });
        assert!(fails(&g), "{g:?}");
    }

    /// Nothing changing is nothing.
    #[test]
    fn a_still_picture_has_no_transitions() {
        let (w, h) = (480, 320);
        let f = solid(w, h, [200, 40, 40]);
        let (g, r) = play(w, h, 90, |_| f.clone());
        assert_eq!((g.max, r.max), (0, 0));
    }
}
