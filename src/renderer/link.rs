//! Links on the grid (`links.rs` decides what is a link): a quiet underline
//! under every link a program declared, a clear one with a faint wash under
//! the link the pointer is on, and a strip that names its target.
//!
//! The strip is drawn in the margin below the last row, outside the
//! program's cells, at its own smaller text size (`fonts::SIZES[3]`). Only
//! when that margin is too shallow to read (a window at 100% scaling) does
//! it sit over the bottom row instead, on the side away from the pointer.
//! None of it animates; it changes only when the hovered link does.
use std::hash::{Hash, Hasher};

use super::*;

/// The strip's text size class (`fonts::SIZES`).
const STRIP_SIZE: u8 = 3;

/// One cell of a declared link, noted while the grid is built.
pub(super) struct LinkCell {
    pub row: u16,
    pub col: u16,
    pub span: u8,
    pub fg: C3,
    pub key: u64,
    /// The program underlined it itself.
    pub underlined: bool,
}

impl LinkCell {
    pub fn new(r: usize, c: usize, span: usize, fg: C3, h: &alacritty_terminal::term::cell::Hyperlink, flags: Flags) -> Self {
        let mut s = std::collections::hash_map::DefaultHasher::new();
        (h.id(), h.uri()).hash(&mut s);
        Self { row: r as u16, col: c as u16, span: span as u8, fg, key: s.finish(), underlined: flags.intersects(Flags::ALL_UNDERLINES) }
    }
}

/// What the strip says: the target, then what Ctrl+click does.
#[derive(Clone, Debug, PartialEq)]
pub struct Strip {
    pub target: String,
    pub hint: String,
    /// The link is not opened (the hint says why).
    pub warn: bool,
    /// Where the pointer is, so a strip that has to sit over the grid keeps clear of it.
    pub pointer_x: f32,
}

impl Renderer {
    /// Underlines for this build's links. Called at the end of `build`.
    pub(super) fn link_lines(&mut self) {
        let m = self.fonts.metrics;
        let pad = self.pad;
        let y = |row: u16| pad[1] + row as f32 * m.cell_h;
        // Resting: one line per run of one link along a row, in the text's own colour, faint.
        let mut run: Option<(u16, u16, u16, u64, C3)> = None;
        let mut lines: Vec<RectInst> = Vec::new();
        let flush = |lines: &mut Vec<RectInst>, r: Option<(u16, u16, u16, u64, C3)>| {
            if let Some((row, c0, c1, _, fg)) = r {
                let x = pad[0] + c0 as f32 * m.cell_w;
                lines.push(RectInst { pos: [x, y(row) + m.ul_y], size: [(c1 - c0) as f32 * m.cell_w, m.ul_h], color: lin(fg, 0.42), params: [0.0; 4] });
            }
        };
        for lc in &self.link_cells {
            if lc.underlined {
                flush(&mut lines, run.take());
                continue;
            }
            let end = lc.col + lc.span as u16;
            match run.as_mut() {
                Some(r) if r.0 == lc.row && r.2 == lc.col && r.3 == lc.key => r.2 = end,
                _ => {
                    flush(&mut lines, run.take());
                    run = Some((lc.row, lc.col, end, lc.key, lc.fg));
                }
            }
        }
        flush(&mut lines, run.take());
        // Hovered: a clear line in the accent and a faint wash behind the text.
        let (cols, rows) = self.grid();
        let thick = (m.ul_h * 1.5).round().max(m.ul_h + 1.0);
        for &(row, a, b) in &self.hover {
            if row >= rows || a >= cols {
                continue;
            }
            let b = b.min(cols - 1);
            let (x, w) = (pad[0] + a as f32 * m.cell_w, (b - a + 1) as f32 * m.cell_w);
            let top = y(row as u16);
            lines.push(RectInst { pos: [x, top + m.ul_y - (thick - m.ul_h) * 0.5], size: [w, thick], color: lin(self.theme.accent, 1.0), params: [thick * 0.5, 0.0, 0.0, 0.0] });
            self.rects_under.push(RectInst { pos: [x, top], size: [w, m.cell_h], color: lin(self.theme.accent, 0.10), params: [(m.cell_h * 0.12).round(), 0.0, 0.0, 0.0] });
        }
        self.rects_over.extend(lines);
    }

    /// Show the strip (`None` removes it). Returns true if making room for its
    /// glyphs wiped the glyph atlas, in which case the grid must be built again.
    pub fn set_strip(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, strip: Option<Strip>) -> bool {
        self.strip_src = strip;
        self.upload_strip(device, queue)
    }

    /// Build the strip again if a wiped atlas left its glyphs pointing nowhere.
    pub(super) fn refresh_strip(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        if self.strip_src.is_some() && self.strip_gen != self.atlas.generation {
            self.upload_strip(device, queue);
        }
    }

    fn upload_strip(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> bool {
        let generation = self.atlas.generation;
        let Some(s) = self.strip_src.clone() else {
            self.strip.n_rects = 0;
            self.strip.n_glyphs = 0;
            return false;
        };
        let th = self.theme.clone();
        let (grid_m, small) = (self.fonts.metrics, self.fonts.metrics_at(STRIP_SIZE));
        let (_, rows) = self.grid();
        let [sw, sh] = self.screen;
        let pad = self.pad;
        let px = (grid_m.cell_h / 22.0).round().max(1.0);
        // Below the last row if that margin holds the text; shrunk a little if
        // it nearly does; over the bottom row only if it cannot.
        let below = pad[1] + rows as f32 * grid_m.cell_h;
        let margin = sh - below;
        let fit = (margin - 2.0 * px) / small.cell_h;
        let (k, y_text, over) = if fit >= 0.82 {
            let k = fit.min(1.0);
            (k, below + (margin - small.cell_h * k) * 0.5, false)
        } else {
            (1.0, sh - small.cell_h - 2.0 * px, true)
        };
        let (cw, ch) = (small.cell_w * k, small.cell_h * k);
        // The target, cut in the middle if it does not fit, then the hint.
        let room = (((sw - 2.0 * pad[0]) / cw).floor() as usize).saturating_sub(4);
        let hint_n = s.hint.chars().count();
        let target_room = room.saturating_sub(hint_n + 2).max(12);
        let target: String = if s.target.chars().count() > target_room {
            let half = target_room.saturating_sub(1) / 2;
            let head: String = s.target.chars().take(half).collect();
            let tail: Vec<char> = s.target.chars().rev().take(target_room - 1 - half).collect();
            format!("{head}\u{2026}{}", tail.into_iter().rev().collect::<String>())
        } else {
            s.target.clone()
        };
        let hint: String = s.hint.chars().take(room.saturating_sub(target.chars().count() + 2)).collect();
        let n = target.chars().count() + if hint.is_empty() { 0 } else { 2 + hint.chars().count() };
        let w = n as f32 * cw;
        let inset = cw * 0.8;
        let x_text = if over && s.pointer_x < sw * 0.5 { sw - pad[0] - w - inset } else { pad[0] + inset };
        let pill_h = ch + 2.0 * px;
        let mut rects = vec![RectInst {
            pos: [x_text - inset, y_text - px],
            size: [w + 2.0 * inset, pill_h],
            color: lin(mix(th.bg, th.accent, 0.10), if over { 0.97 } else { 0.92 }),
            params: [pill_h * 0.5, 0.0, 0.0, 0.0],
        }];
        let edge = if s.warn { th.ansi[3] } else { th.accent };
        rects.push(RectInst { pos: [x_text - inset, y_text - px], size: [w + 2.0 * inset, pill_h], color: lin(edge, 0.35), params: [pill_h * 0.5, px.max(1.0), 0.0, 0.0] });
        let mut glyphs = Vec::new();
        let words = [(target, if s.warn { th.ansi[3] } else { th.accent }, 0.95f32), (if hint.is_empty() { String::new() } else { format!("  {hint}") }, mix(th.fg, th.bg, 0.35), 0.9)];
        let mut i = 0usize;
        for (text, color, alpha) in words {
            for ch_ in text.chars() {
                if ch_ != ' ' {
                    let slot = self.atlas.get(queue, &mut self.fonts, GlyphKey { ch: ch_, style: STRIP_SIZE << 2, span: 1 });
                    if !slot.empty {
                        glyphs.push(GlyphInst {
                            pos: [x_text + i as f32 * cw + slot.off[0] * k, y_text + slot.off[1] * k],
                            size: [slot.size[0] * k, slot.size[1] * k],
                            uv: slot.uv,
                            color: lin(color, alpha),
                            params: [-1.0, 0.0, 0.0, 0.0],
                        });
                    }
                }
                i += 1;
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
        upload(&mut self.strip.rects, "strip rects", bytemuck::cast_slice(&rects));
        upload(&mut self.strip.glyphs, "strip glyphs", bytemuck::cast_slice(&glyphs));
        self.strip.n_rects = rects.len() as u32;
        self.strip.n_glyphs = glyphs.len() as u32;
        self.strip_gen = self.atlas.generation;
        self.atlas.generation != generation
    }

    /// Drawn last, over everything.
    pub(super) fn draw_strip(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let (Some(b), true) = (&self.strip.rects, self.strip.n_rects > 0) {
            pass.set_pipeline(&self.rect_pipe);
            pass.set_vertex_buffer(0, b.slice(..));
            pass.draw(0..4, 0..self.strip.n_rects);
        }
        if let (Some(b), true) = (&self.strip.glyphs, self.strip.n_glyphs > 0) {
            pass.set_pipeline(&self.glyph_pipe);
            pass.set_vertex_buffer(0, b.slice(..));
            pass.draw(0..4, 0..self.strip.n_glyphs);
        }
    }

    /// Reduced motion on or off. Turning it on stops whatever is still
    /// moving: the next build shows the grid settled, the light goes out,
    /// the cursor stands where it is going.
    pub fn set_still(&mut self, on: bool) {
        self.still = on;
        if on {
            self.anim_until = 0.0;
            self.field_until = 0.0;
            self.field_live = false;
            self.cursor_energy = 0.0;
            self.suppress_reveal = true;
            if let Some(c) = self.cursor {
                self.cursor_pos = Some([c.x, c.y]);
            }
            self.cursor_moving = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything the renderer would draw, at two moments.
    fn frames(r: &mut Renderer, gpu: &crate::gpu::Gpu, a: f64, b: f64) -> (Vec<u8>, Vec<u8>) {
        let (_, _, x) = r.capture(&gpu.device, &gpu.queue, a);
        let (_, _, y) = r.capture(&gpu.device, &gpu.queue, b);
        (x, y)
    }

    /// With reduced motion a burst of output owes no frame beyond the one
    /// that shows it: nothing unfolds, glows, glides or pulses, so the
    /// picture right after it arrives is the picture a minute later. The
    /// same burst with motion on does owe frames (the control).
    #[test]
    fn reduced_motion_draws_each_change_once_and_nothing_after() {
        let Ok(gpu) = crate::gpu::init(crate::gpu::instance(), None) else {
            eprintln!("no GPU adapter here: the check was not run");
            return;
        };
        let theme = Theme::void_light();
        for still in [false, true] {
            let mut r = Renderer::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb, 30.0, theme.clone());
            r.resize(&gpu.device, 900, 320, 24.0);
            r.light = 3.0;
            r.set_still(still);
            let (cols, rows) = r.grid();
            let term = crate::term::Terminal::new(cols, rows, theme.clone(), std::sync::Arc::new(|_| {}), None);
            let build = |r: &mut Renderer, now: f64| {
                let t = term.term.lock().unwrap();
                r.build(&gpu.device, &gpu.queue, &t, now, true);
            };
            term.feed(b"$ ");
            build(&mut r, 1.0);
            r.pulse(); // a key
            term.feed(b"cargo build\r\n\x1b[31merror\x1b[0m: could not compile\r\n");
            build(&mut r, 2.0);
            assert!(r.arrivals.cells > 20, "arrivals are still counted (and heard): {:?}", r.arrivals);
            if still {
                assert!(!r.animating(2.0) && !r.animating(2.001), "reduced motion still owes frames");
                let (a, b) = frames(&mut r, &gpu, 2.0, 62.0);
                assert!(a == b, "the picture changed after the change was shown");
            } else {
                assert!(r.animating(2.05), "the control: with motion on, the burst animates");
                let (a, b) = frames(&mut r, &gpu, 2.05, 62.0);
                assert!(a != b, "the control: mid-reveal differs from settled");
            }
        }
    }

    /// Animations switched off in Windows while text is still unfolding and
    /// the light still glowing: the next frame (the window rebuilds once, as
    /// it does on the setting's change) is the settled picture, and none is
    /// owed after it.
    #[test]
    fn reduced_motion_switched_on_mid_reveal_settles_at_once() {
        let Ok(gpu) = crate::gpu::init(crate::gpu::instance(), None) else {
            eprintln!("no GPU adapter here: the check was not run");
            return;
        };
        let theme = Theme::void_light();
        let mut r = Renderer::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb, 30.0, theme.clone());
        r.resize(&gpu.device, 900, 320, 24.0);
        r.light = 3.0;
        let (cols, rows) = r.grid();
        let term = crate::term::Terminal::new(cols, rows, theme.clone(), std::sync::Arc::new(|_| {}), None);
        let build = |r: &mut Renderer, now: f64| {
            let t = term.term.lock().unwrap();
            r.build(&gpu.device, &gpu.queue, &t, now, true);
        };
        term.feed(b"$ ");
        build(&mut r, 1.0);
        r.pulse();
        term.feed(b"cargo build\r\n\x1b[31merror\x1b[0m: could not compile\r\n");
        build(&mut r, 2.0);
        assert!(r.animating(2.05), "the control: the burst is mid-reveal");
        r.set_still(true);
        build(&mut r, 2.05);
        assert!(!r.animating(2.05) && !r.animating(2.06), "frames are still owed after animations were switched off");
        let (a, b) = frames(&mut r, &gpu, 2.05, 62.0);
        assert!(a == b, "the first frame after the switch is not the settled picture");
        // And back on: nothing replays; only new output unfolds again.
        r.set_still(false);
        build(&mut r, 3.0);
        assert!(!r.animating(3.0), "switching motion back on replayed old text");
        term.feed(b"more\r\n");
        build(&mut r, 4.0);
        assert!(r.animating(4.05), "with motion back on, new text unfolds");
    }

    /// The hovered link is underlined and the strip is drawn below the last
    /// row, and neither touches a pixel of the program's rows' text.
    #[test]
    fn hover_underlines_and_the_strip_sits_below_the_grid() {
        let Ok(gpu) = crate::gpu::init(crate::gpu::instance(), None) else {
            eprintln!("no GPU adapter here: the check was not run");
            return;
        };
        let theme = Theme::void_light();
        let mut r = Renderer::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb, 30.0, theme.clone());
        r.resize(&gpu.device, 1200, 400, 24.0);
        let (cols, rows) = r.grid();
        let term = crate::term::Terminal::new(cols, rows, theme.clone(), std::sync::Arc::new(|_| {}), None);
        term.feed(b"see \x1b]8;;https://example.com/x\x1b\\the manual\x1b]8;;\x1b\\ now");
        let build = |r: &mut Renderer| {
            let t = term.term.lock().unwrap();
            r.build(&gpu.device, &gpu.queue, &t, 1.0, true);
        };
        build(&mut r);
        let (_, _, plain) = r.capture(&gpu.device, &gpu.queue, 50.0);
        r.hover = vec![(0, 4, 13)];
        r.set_strip(&gpu.device, &gpu.queue, Some(Strip { target: "https://example.com/x".into(), hint: "ctrl+click to open".into(), warn: false, pointer_x: 300.0 }));
        build(&mut r);
        let (w, _, hovered) = r.capture(&gpu.device, &gpu.queue, 50.0);
        let m = r.fonts.metrics;
        let bottom = (r.pad[1] + rows as f32 * m.cell_h) as usize;
        let changed: Vec<(usize, usize)> = (0..hovered.len() / 4).filter(|&i| hovered[i * 4..i * 4 + 3] != plain[i * 4..i * 4 + 3]).map(|i| (i % w as usize, i / w as usize)).collect();
        assert!(changed.iter().any(|&(_, y)| y >= bottom), "no strip below the grid");
        let x0 = (r.pad[0] + 4.0 * m.cell_w) as usize;
        let x1 = (r.pad[0] + 14.0 * m.cell_w) as usize;
        let in_grid: Vec<&(usize, usize)> = changed.iter().filter(|&&(_, y)| y < bottom).collect();
        assert!(!in_grid.is_empty(), "the hovered link was not drawn any differently");
        assert!(in_grid.iter().all(|&&(x, y)| x + 1 >= x0 && x <= x1 + 1 && y < (r.pad[1] + m.cell_h) as usize + 1), "hover drew outside the link's cells");
        // Every cell of the link is marked, not only the first few.
        for c in 4..14 {
            let (a, b) = ((r.pad[0] + c as f32 * m.cell_w) as usize + 1, (r.pad[0] + (c + 1) as f32 * m.cell_w) as usize - 1);
            assert!(in_grid.iter().any(|&&(x, _)| (a..b).contains(&x)), "cell {c} of the hovered link was not drawn as hovered");
        }
        // Off again: back to the plain picture exactly.
        r.hover.clear();
        r.set_strip(&gpu.device, &gpu.queue, None);
        build(&mut r);
        let (_, _, again) = r.capture(&gpu.device, &gpu.queue, 50.0);
        assert!(again == plain, "leaving the link did not restore the picture");
    }
}
