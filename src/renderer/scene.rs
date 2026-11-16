//! Drawing a `world::Scene` with the grid's own pipelines: the same rounded
//! rectangles, the same glyph atlas, the same light field. A scene replaces
//! the grid's instances until the grid is built again; nothing here adds a
//! pass, a buffer or a shader.
use super::*;
use crate::world::{Scene, Size, Type};

/// A full block: drawn stretched and fully transparent, it is invisible but
/// still emits into the light field while "hot", which is how an event with
/// no text of its own (a process starting, or failing) casts light.
const EMITTER: char = '\u{2588}';

impl Renderer {
    /// Cell sizes of the three text sizes, for laying a scene out.
    pub fn scene_type(&self) -> Type {
        let cell = |s: Size| {
            let m = self.fonts.metrics_at(s as u8);
            [m.cell_w, m.cell_h]
        };
        Type { cell: [cell(Size::Body), cell(Size::Small), cell(Size::Large)], unit: self.fonts.px / 15.0 }
    }

    /// The grid gives way to a scene: its cursor goes, and so does any light
    /// its text was still casting.
    pub fn enter_scene(&mut self) {
        self.cursor = None;
        self.cursor_energy = 0.0;
        self.field_live = false;
        self.field_until = 0.0;
        self.anim_until = 0.0;
    }

    /// Back to the grid: it is shown as it stands, without animating what
    /// arrived while it was out of sight.
    pub fn leave_scene(&mut self) {
        self.suppress_reveal = true;
        self.field_live = false;
        self.field_until = 0.0;
        self.anim_until = 0.0;
    }

    /// Turn a scene into GPU instances. Call when the scene changed, and
    /// again after any `build` of the grid (which reuses the same buffers).
    pub fn build_scene(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, scene: &Scene) {
        let rect = |q: &crate::world::Quad| RectInst { pos: [q.x, q.y], size: [q.w, q.h], color: lin(q.color, q.alpha), params: [q.radius, q.ring, 0.0, 0.0] };
        self.rects_under.clear();
        self.rects_under.extend(scene.quads.iter().map(rect));
        self.rects_over.clear();
        self.rects_over.extend(scene.over.iter().map(rect));

        // Frame-clock seconds to shader time; anything from before the
        // shader's time base is long settled.
        let base = self.time_base;
        let shader_t = move |t: f64| {
            let s = (t - base) as f32;
            if s >= 0.0 { s } else { -1.0 }
        };
        let cells = [Size::Body, Size::Small, Size::Large].map(|s| self.fonts.metrics_at(s as u8).cell_w);
        let heights = [Size::Body, Size::Small, Size::Large].map(|s| self.fonts.metrics_at(s as u8).cell_h);
        for _attempt in 0..2 {
            self.glyphs.clear();
            let generation = self.atlas.generation;
            let block = self.atlas.get(queue, &mut self.fonts, GlyphKey { ch: EMITTER, style: 0, span: 1 });
            if !block.empty {
                // One texel in from each edge, so the stretched block has no soft rim.
                let texel = (block.uv[2] - block.uv[0]) / block.size[0].max(1.0);
                for e in &scene.emits {
                    self.glyphs.push(GlyphInst {
                        pos: [e.x, e.y],
                        size: [e.w, e.h],
                        uv: [block.uv[0] + texel, block.uv[1] + texel, block.uv[2] - texel, block.uv[3] - texel],
                        color: lin(e.color, 0.0),
                        params: [shader_t(e.born), e.energy, 0.0, 0.0],
                    });
                }
            }
            for t in &scene.texts {
                let (cw, ch) = (cells[t.size as usize], heights[t.size as usize]);
                let style = t.bold as u8 | (t.size as u8) << 2;
                for (i, c) in t.s.chars().enumerate() {
                    if c == ' ' {
                        continue;
                    }
                    let x = t.x + i as f32 * cw;
                    if let (Some(k), false) = (scene.clip, t.top) {
                        // By its middle: a row the card only brushes keeps its text.
                        let (mx, my) = (x + cw * 0.5, t.y + ch * 0.5);
                        if mx > k[0] && mx < k[0] + k[2] && my > k[1] && my < k[1] + k[3] {
                            continue; // under the hover card
                        }
                    }
                    let slot = self.atlas.get(queue, &mut self.fonts, GlyphKey { ch: c, style, span: 1 });
                    if slot.empty {
                        continue;
                    }
                    // Arriving text unfolds left to right, as it does on the grid.
                    let (birth, energy) = match t.born {
                        Some(b) => (shader_t(b + (i as f32 * STAGGER).min(STAGGER_MAX) as f64), 0.75),
                        None => (-1.0, 0.0),
                    };
                    self.glyphs.push(GlyphInst {
                        pos: [x + slot.off[0], t.y + slot.off[1]],
                        size: slot.size,
                        uv: slot.uv,
                        color: lin(t.color, 1.0),
                        params: [birth, if birth < 0.0 { 0.0 } else { energy }, 0.0, 0.0],
                    });
                }
            }
            // The atlas was wiped mid-build: earlier slots are stale, go again.
            if self.atlas.generation == generation {
                break;
            }
        }

        // Frames are owed only while the latest arrival is still settling.
        self.cursor = None;
        self.cursor_energy = 0.0;
        match scene.last_birth.map(shader_t).filter(|b| *b >= 0.0) {
            Some(b) => {
                self.anim_until = b + 2.0 * STAGGER_MAX + REVEAL_DUR;
                self.field_until = self.anim_until + FIELD_TAIL;
            }
            None => {
                self.anim_until = 0.0;
                self.field_until = 0.0;
            }
        }

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
            queue.write_buffer(&self.rect_buf, (self.rects_under.len() * std::mem::size_of::<RectInst>()) as u64, bytemuck::cast_slice(&self.rects_over));
        }
        if !self.glyphs.is_empty() {
            queue.write_buffer(&self.glyph_buf, 0, bytemuck::cast_slice(&self.glyphs));
        }
        self.n_under = self.rects_under.len() as u32;
        self.n_over = self.rects_over.len() as u32;
        self.n_glyphs = self.glyphs.len() as u32;
    }

    /// What was last built, in instances (for `shot` to report).
    pub fn instance_counts(&self) -> (u32, u32) {
        (self.n_under + self.n_over, self.n_glyphs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{Emit, Text};

    /// The window's frame loop draws another frame only while `animating`
    /// is true (`app.rs`, end of `redraw`). This asks that question of the
    /// real renderer on the real GPU, with no window: a settled scene owes no
    /// frames, an arrival owes them for a bounded time, and once it has
    /// settled the frame itself no longer changes.
    #[test]
    fn a_scene_owes_frames_only_while_its_latest_arrival_settles() {
        let Ok(gpu) = crate::gpu::init(crate::gpu::instance(), None) else {
            eprintln!("no GPU adapter here: the check was not run");
            return;
        };
        let mut r = Renderer::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb, 30.0, Theme::void_light());
        r.resize(&gpu.device, 640, 240, 24.0);
        let mut scene = Scene::default();
        scene.quads.push(crate::world::Quad { x: 40.0, y: 60.0, w: 300.0, h: 40.0, color: [94, 230, 255], alpha: 0.9, radius: 10.0, ring: 2.0 });
        scene.texts.push(Text { x: 60.0, y: 60.0, s: "cargo build".into(), color: [223, 230, 240], size: Size::Small, bold: true, born: None, top: false });

        // The grid had a cursor with light of its own; a scene has none.
        r.pulse();
        r.enter_scene();
        r.build_scene(&gpu.device, &gpu.queue, &scene);
        for t in [0.0, 0.001, 0.5, 60.0, 5000.0] {
            assert!(!r.animating(t), "a settled scene asked for a frame at {t} s");
        }
        let (_, _, still_a) = r.capture(&gpu.device, &gpu.queue, 1.0);
        let (_, _, still_b) = r.capture(&gpu.device, &gpu.queue, 400.0);
        assert!(still_a == still_b, "a settled scene drew differently at two times");
        assert!(still_a.chunks_exact(4).any(|p| p[2] > 150), "the scene was not drawn at all");

        // Something arrives at t = 10 s.
        scene.texts[0].born = Some(10.0);
        scene.emits.push(Emit { x: 40.0, y: 60.0, w: 300.0, h: 40.0, color: [94, 230, 255], born: 10.0, energy: 0.2 });
        scene.last_birth = Some(10.0);
        r.build_scene(&gpu.device, &gpu.queue, &scene);
        assert!(r.animating(10.0) && r.animating(10.2), "an arrival is animated");
        let settled = (0..10_000).map(|ms| 10.0 + ms as f64 / 1000.0).find(|&t| !r.animating(t)).expect("it never settles");
        assert!(settled - 10.0 < 1.5, "frames owed for {:.3} s", settled - 10.0);
        for t in [settled, settled + 0.001, settled + 1.0, 86_400.0] {
            assert!(!r.animating(t), "asked for a frame at {t} s, after settling at {settled}");
        }
        // Mid-arrival the frame differs from the settled one (light, reveal);
        // after settling, two frames a minute apart are the same bytes.
        r.simulate_field(&gpu.device, &gpu.queue, 10.0, 10.08);
        let (_, _, during) = r.capture(&gpu.device, &gpu.queue, 10.08);
        let (_, _, after_a) = r.capture(&gpu.device, &gpu.queue, settled + 0.5);
        let (_, _, after_b) = r.capture(&gpu.device, &gpu.queue, settled + 60.0);
        assert!(during != after_a, "the arrival was not drawn any differently");
        assert!(after_a == after_b, "the frame still changes after the scene has settled");
        assert!(after_a == still_a, "settled, an arrival looks exactly like what was always there");

        // Going back to the grid leaves nothing owed either.
        r.leave_scene();
        assert!(r.suppress_reveal && !r.animating(settled + 61.0));
    }

    /// What the window does on every frame while the view is up (`app.rs`,
    /// `redraw` then `world_draw`): the grid is still built when output
    /// arrives, so that arriving text is still counted and heard, and the
    /// scene is then built over it. Checked here on the real renderer: the
    /// grid's arrivals are counted, none of its animation is owed or drawn,
    /// and coming back shows the grid without replaying what was missed.
    #[test]
    fn the_grid_is_still_heard_under_a_scene_and_comes_back_settled() {
        let Ok(gpu) = crate::gpu::init(crate::gpu::instance(), None) else {
            eprintln!("no GPU adapter here: the check was not run");
            return;
        };
        let theme = Theme::void_light();
        let mut r = Renderer::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb, 30.0, theme.clone());
        r.resize(&gpu.device, 900, 300, 24.0);
        let (cols, rows) = r.grid();
        let term = crate::term::Terminal::new(cols, rows, theme, std::sync::Arc::new(|_| {}), None);
        let grid = |r: &mut Renderer, now: f64| {
            let t = term.term.lock().unwrap();
            r.build(&gpu.device, &gpu.queue, &t, now, true);
        };
        term.feed(b"$ ");
        grid(&mut r, 1.0); // the first build of a grid never animates

        let mut scene = Scene::default();
        scene.quads.push(crate::world::Quad { x: 40.0, y: 60.0, w: 300.0, h: 40.0, color: [94, 230, 255], alpha: 0.9, radius: 10.0, ring: 2.0 });
        scene.texts.push(Text { x: 60.0, y: 60.0, s: "cargo build".into(), color: [223, 230, 240], size: Size::Small, bold: true, born: None, top: false });
        r.enter_scene();
        r.build_scene(&gpu.device, &gpu.queue, &scene);
        assert!(!r.animating(1.5));
        let (_, _, alone) = r.capture(&gpu.device, &gpu.queue, 1.5);

        // The program prints while the view is up.
        term.feed(b"cargo build
   Compiling tiny v0.1.0
");
        grid(&mut r, 2.0);
        assert!(r.arrivals.cells == 29 && !r.arrivals.bulk, "{:?}: the 29 glyphs that arrived under the view must still be counted", r.arrivals);
        assert!(r.animating(2.05), "the grid's own build asks for its reveal");
        r.build_scene(&gpu.device, &gpu.queue, &scene);
        assert!(!r.animating(2.05) && !r.animating(2.0), "none is owed: the grid is not what is on screen");
        let (_, _, over) = r.capture(&gpu.device, &gpu.queue, 2.05);
        assert!(over == alone, "something of the grid showed through the scene");

        // Back to the grid: what arrived meanwhile is simply there.
        r.leave_scene();
        grid(&mut r, 3.0);
        assert_eq!(r.arrivals.cells, 0, "what arrived under the view is not announced a second time");
        let (_, _, back_a) = r.capture(&gpu.device, &gpu.queue, 3.0);
        assert!(!r.animating(3.01), "nothing replays on return");
        let (_, _, back_b) = r.capture(&gpu.device, &gpu.queue, 9.0);
        assert!(back_a == back_b && back_a != alone, "the grid is back, and still");
    }
}
