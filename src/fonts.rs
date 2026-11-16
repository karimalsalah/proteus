//! Fonts: the embedded primary family, lazily loaded system fallbacks, and the
//! cell metrics everything else is laid out on.
use std::path::PathBuf;

use ab_glyph::{point, Font, FontArc, FontVec, PxScale, ScaleFont};

static REGULAR: &[u8] = include_bytes!("../assets/fonts/MapleMono-NF-Regular.ttf");
static BOLD: &[u8] = include_bytes!("../assets/fonts/MapleMono-NF-Bold.ttf");
static ITALIC: &[u8] = include_bytes!("../assets/fonts/MapleMono-NF-Italic.ttf");
static BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/MapleMono-NF-BoldItalic.ttf");

/// System fonts tried, in order, for characters the primary family lacks.
const FALLBACKS: &[&str] = &["seguisym.ttf", "seguiemj.ttf", "msyh.ttc", "malgun.ttf", "YuGothR.ttc", "segoeui.ttf"];

#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub cell_w: f32,
    pub cell_h: f32,
    /// Distance from the top of a cell to the text baseline.
    pub baseline: f32,
    pub ul_y: f32,
    pub ul_h: f32,
    pub strike_y: f32,
}

/// A rasterised glyph: coverage bytes plus where its top-left corner sits
/// relative to the top-left of its cell.
pub struct Raster {
    pub w: u32,
    pub h: u32,
    pub left: i32,
    pub top: i32,
    pub data: Vec<u8>,
}

enum Fallback {
    Unloaded(PathBuf),
    Loaded(FontArc),
    Missing,
}

pub struct FontSet {
    primary: Vec<FontArc>,
    fallbacks: Vec<Fallback>,
    pub px: f32,
    pub line_height: f32,
    pub metrics: CellMetrics,
}

/// Text sizes relative to the grid's, for views drawn outside the grid (the
/// World view; 3 is the link strip below the grid). Bits 2-3 of a glyph's
/// `style` choose one; 0 is the grid's own.
pub const SIZES: [f32; 4] = [1.0, 0.8, 1.7, 0.62];

fn scale_for(face: &FontArc, px: f32) -> PxScale {
    // ab_glyph scales by ascent-to-descent height, not by em; convert.
    let upem = face.units_per_em().unwrap_or(1000.0);
    PxScale::from(px * face.height_unscaled() / upem)
}

fn compute_metrics(face: &FontArc, px: f32, line_height: f32) -> CellMetrics {
    let sf = face.as_scaled(scale_for(face, px));
    let adv = sf.h_advance(face.glyph_id('M'));
    let (asc, desc, gap) = (sf.ascent(), -sf.descent(), sf.line_gap());
    let cell_w = adv.round().max(1.0);
    let cell_h = ((asc + desc + gap) * line_height).round().max(1.0);
    let baseline = ((cell_h - (asc + desc)) * 0.5 + asc).round();
    let ul_h = (px / 14.0).round().max(1.0);
    let ul_y = (baseline + (desc * 0.45).max(ul_h + 1.0)).round().min(cell_h - ul_h);
    let strike_y = (baseline - asc * 0.30).round();
    CellMetrics { cell_w, cell_h, baseline, ul_y, ul_h, strike_y }
}

impl FontSet {
    pub fn new(px: f32) -> Self {
        let primary: Vec<FontArc> = [REGULAR, BOLD, ITALIC, BOLD_ITALIC]
            .into_iter()
            .map(|d| FontArc::try_from_slice(d).expect("embedded font is valid"))
            .collect();
        let dir = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
        let fallbacks = FALLBACKS.iter().map(|n| Fallback::Unloaded(dir.join(n))).collect();
        let line_height = 1.22;
        let metrics = compute_metrics(&primary[0], px, line_height);
        Self { primary, fallbacks, px, line_height, metrics }
    }

    pub fn set_px(&mut self, px: f32) {
        self.px = px.clamp(6.0, 200.0);
        self.metrics = compute_metrics(&self.primary[0], self.px, self.line_height);
    }

    /// Cell metrics of size class `size` (an index into `SIZES`).
    pub fn metrics_at(&self, size: u8) -> CellMetrics {
        match size & 3 {
            0 => self.metrics,
            s => compute_metrics(&self.primary[0], self.px * SIZES[s as usize], self.line_height),
        }
    }

    /// Rasterise `ch`. `style` bit 0 is bold, bit 1 italic, bits 2-3 a size
    /// class (`SIZES`), `MARK` says the character is a combining mark drawn
    /// over the cell's own character. `span` is how many cells the character
    /// occupies. `None` means no font has it.
    pub fn raster(&mut self, ch: char, style: u8, span: u8) -> Option<Raster> {
        let size = style >> 2 & 3;
        let (px, m) = (self.px * SIZES[size as usize], self.metrics_at(size));
        let mark = style & MARK != 0;
        let styled = (style as usize & 3).min(self.primary.len() - 1);
        for which in [styled, 0] {
            let face = &self.primary[which];
            if face.glyph_id(ch).0 != 0 {
                return Some(if mark { raster_mark(face, ch, px, m, span) } else { raster_in(face, ch, px, m, span, false) });
            }
        }
        for i in 0..self.fallbacks.len() {
            let unloaded = match &self.fallbacks[i] {
                Fallback::Unloaded(p) => Some(p.clone()),
                _ => None,
            };
            if let Some(path) = unloaded {
                self.fallbacks[i] = match std::fs::read(&path).ok().and_then(|d| FontVec::try_from_vec_and_index(d, 0).ok()) {
                    Some(f) => Fallback::Loaded(FontArc::new(f)),
                    None => Fallback::Missing,
                };
            }
            if let Fallback::Loaded(face) = &self.fallbacks[i] {
                if face.glyph_id(ch).0 != 0 {
                    return Some(if mark { raster_mark(face, ch, px, m, span) } else { raster_in(face, ch, px, m, span, true) });
                }
            }
        }
        None
    }
}

/// Style bit: a combining mark (an accent, a dot below), drawn over the
/// character that owns the cell instead of in a cell of its own.
pub const MARK: u8 = 0x40;

/// Zero-width characters that have no shape of their own: joiners, direction
/// marks and variation selectors. They change how their neighbours are meant
/// to be shown; drawing whatever placeholder a font holds for them is wrong.
pub fn is_invisible(ch: char) -> bool {
    matches!(ch as u32, 0x034F | 0x061C | 0x180B..=0x180F | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFE00..=0xFE0F | 0xFEFF | 0xE0000..=0xE0FFF)
}

/// A combining mark. Fonts draw them in one of two ways: with no advance and
/// the ink to the left of the origin (to be placed after the base character),
/// or with a full advance and the ink inside it. Either way the mark's
/// advance box is made to end where the cell ends, which puts it over the base.
fn raster_mark(face: &FontArc, ch: char, px: f32, m: CellMetrics, span: u8) -> Raster {
    let gid = face.glyph_id(ch);
    let scale = scale_for(face, px);
    let avail = m.cell_w * span.max(1) as f32;
    let adv = face.as_scaled(scale).h_advance(gid);
    outline(face, gid.with_scale_and_position(scale, point((avail - adv).round(), m.baseline)))
}

/// `fit`: shrink and centre glyphs from fallback fonts so they stay inside
/// the cells they own (symbol and CJK fonts are wider than the grid).
fn raster_in(face: &FontArc, ch: char, px: f32, m: CellMetrics, span: u8, fit: bool) -> Raster {
    let gid = face.glyph_id(ch);
    let mut scale = scale_for(face, px);
    let avail = m.cell_w * span.max(1) as f32;
    let mut xoff = 0.0;
    if fit {
        let adv = face.as_scaled(scale).h_advance(gid);
        if adv > avail * 1.02 {
            let k = avail / adv;
            scale = PxScale { x: scale.x * k, y: scale.y * k };
        }
        let adv = face.as_scaled(scale).h_advance(gid);
        xoff = ((avail - adv) * 0.5).max(0.0).round();
    }
    outline(face, gid.with_scale_and_position(scale, point(xoff, m.baseline)))
}

fn outline(face: &FontArc, glyph: ab_glyph::Glyph) -> Raster {
    match face.outline_glyph(glyph) {
        Some(o) => {
            let b = o.px_bounds();
            let w = b.width().ceil().max(0.0) as u32;
            let h = b.height().ceil().max(0.0) as u32;
            let mut data = vec![0u8; (w * h) as usize];
            o.draw(|x, y, c| {
                if x < w && y < h {
                    data[(y * w + x) as usize] = (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
            });
            Raster { w, h, left: b.min.x.round() as i32, top: b.min.y.round() as i32, data }
        }
        None => Raster { w: 0, h: 0, left: 0, top: 0, data: Vec::new() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_whole_pixels_and_glyphs_fit() {
        let mut f = FontSet::new(30.0);
        let m = f.metrics;
        assert_eq!(m.cell_w, m.cell_w.round());
        assert_eq!(m.cell_h, m.cell_h.round());
        assert!(m.cell_w >= 12.0 && m.cell_h > m.cell_w);
        assert!(m.baseline > 0.0 && m.baseline < m.cell_h);
        let r = f.raster('A', 0, 1).expect("primary font has A");
        assert!(r.w > 0 && r.h > 0 && (r.w as f32) <= m.cell_w + 2.0);
        assert!(r.data.iter().any(|&v| v > 200));
        // A space has an entry but no ink.
        let s = f.raster(' ', 0, 1).expect("space exists");
        assert_eq!(s.w * s.h, 0);
    }

    /// An accent is drawn over the letter that owns the cell: inside the
    /// cell's width, and above where a lower-case letter's body ends.
    #[test]
    fn combining_marks_land_on_their_letter() {
        let mut f = FontSet::new(30.0);
        let m = f.metrics;
        let e = f.raster('e', 0, 1).expect("e");
        for mark in ['\u{301}', '\u{308}', '\u{303}'] {
            let r = f.raster(mark, MARK, 1).unwrap_or_else(|| panic!("no font has U+{:04X}", mark as u32));
            assert!(r.w > 0 && r.h > 0, "U+{:04X} has ink", mark as u32);
            let centre = r.left as f32 + r.w as f32 * 0.5;
            assert!(centre > m.cell_w * 0.2 && centre < m.cell_w * 0.8, "U+{:04X} centre {centre} in a cell {} wide", mark as u32, m.cell_w);
            assert!(r.top + r.h as i32 <= e.top + 2, "U+{:04X} sits above the letter (mark bottom {}, e top {})", mark as u32, r.top + r.h as i32, e.top);
            assert!(r.top >= 0, "and inside the cell");
        }
        let below = f.raster('\u{323}', MARK, 1).expect("dot below");
        assert!(below.top as f32 >= m.baseline, "a dot below sits under the baseline");
        assert!(is_invisible('\u{200d}') && is_invisible('\u{fe0f}') && !is_invisible('\u{301}'));
    }
}
