//! The glyph atlas: every shape the terminal can draw, rasterised once into a
//! single GPU texture and then referenced by rectangle.
use std::collections::HashMap;

use crate::builtin;
use crate::fonts::FontSet;

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
pub struct GlyphKey {
    pub ch: char,
    pub style: u8,
    pub span: u8,
}

#[derive(Clone, Copy)]
pub struct Slot {
    /// Texture coordinates: left, top, right, bottom.
    pub uv: [f32; 4],
    pub size: [f32; 2],
    /// Offset of the bitmap from the top-left of its cell.
    pub off: [f32; 2],
    pub empty: bool,
}

const EMPTY: Slot = Slot { uv: [0.0; 4], size: [0.0; 2], off: [0.0; 2], empty: true };

pub struct Atlas {
    pub tex: wgpu::Texture,
    pub view: wgpu::TextureView,
    size: u32,
    x: u32,
    y: u32,
    shelf_h: u32,
    map: HashMap<GlyphKey, Slot>,
    /// Bumped whenever the atlas is wiped; cached slots are invalid after that.
    pub generation: u64,
}

impl Atlas {
    pub fn new(device: &wgpu::Device, size: u32) -> Self {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        Self { tex, view, size, x: 1, y: 1, shelf_h: 0, map: HashMap::new(), generation: 0 }
    }

    /// Forget everything (font size changed, or the texture filled up).
    pub fn reset(&mut self, queue: &wgpu::Queue) {
        self.map.clear();
        self.x = 1;
        self.y = 1;
        self.shelf_h = 0;
        self.generation += 1;
        // Stale texels would bleed into new glyphs' padding; clear them.
        let zeros = vec![0u8; (self.size * self.size) as usize];
        self.upload(queue, 0, 0, self.size, self.size, &zeros);
    }

    fn upload(&self, queue: &wgpu::Queue, x: u32, y: u32, w: u32, h: u32, data: &[u8]) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    /// Shelf packing with a one-texel gutter. `None` means the atlas is full.
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w + 2 > self.size || h + 2 > self.size {
            return None;
        }
        if self.x + w + 1 > self.size {
            self.x = 1;
            self.y += self.shelf_h + 1;
            self.shelf_h = 0;
        }
        if self.y + h + 1 > self.size {
            return None;
        }
        let at = (self.x, self.y);
        self.x += w + 1;
        self.shelf_h = self.shelf_h.max(h);
        Some(at)
    }

    pub fn get(&mut self, queue: &wgpu::Queue, fonts: &mut FontSet, key: GlyphKey) -> Slot {
        if let Some(s) = self.map.get(&key) {
            return *s;
        }
        let m = fonts.metrics;
        let (w, h, left, top, data) = if let Some(kind) = crate::deco::Deco::from_style(key.style) {
            // Not a character: a shaped underline (curl, dots, dashes).
            let t = crate::deco::tile(kind, m.cell_w as u32, m.cell_h as u32, m.ul_y, m.ul_h);
            (t.w, t.h, 0, t.top, t.data)
        } else {
            match builtin::render(key.ch, m.cell_w as u32, m.cell_h as u32) {
                Some(d) => (m.cell_w as u32, m.cell_h as u32, 0, 0, d),
                None => match fonts.raster(key.ch, key.style, key.span) {
                    Some(r) => (r.w, r.h, r.left, r.top, r.data),
                    None => (0, 0, 0, 0, Vec::new()),
                },
            }
        };
        let slot = if w == 0 || h == 0 {
            EMPTY
        } else {
            let at = match self.alloc(w, h) {
                Some(at) => at,
                None => {
                    self.reset(queue);
                    match self.alloc(w, h) {
                        Some(at) => at,
                        None => return EMPTY,
                    }
                }
            };
            self.upload(queue, at.0, at.1, w, h, &data);
            let s = self.size as f32;
            Slot {
                uv: [at.0 as f32 / s, at.1 as f32 / s, (at.0 + w) as f32 / s, (at.1 + h) as f32 / s],
                size: [w as f32, h as f32],
                off: [left as f32, top as f32],
                empty: false,
            }
        };
        self.map.insert(key, slot);
        slot
    }
}
