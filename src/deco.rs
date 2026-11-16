//! Underlines that are shapes rather than straight lines: the curl, dots and
//! dashes. Each is drawn once per font size as a tile one cell wide whose
//! pattern repeats exactly at the cell edge, so a run of cells shows one
//! unbroken wave or one even row of dots, whatever the cell width.
use alacritty_terminal::term::cell::Flags;

/// Marks an atlas key as one of these tiles rather than a character.
pub const STYLE_BIT: u8 = 0x80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Deco {
    Curl = 1,
    Dotted = 2,
    Dashed = 3,
}

impl Deco {
    /// The shaped underline a cell asks for, if any.
    pub fn of(flags: Flags) -> Option<Deco> {
        if flags.contains(Flags::UNDERCURL) {
            Some(Deco::Curl)
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            Some(Deco::Dotted)
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            Some(Deco::Dashed)
        } else {
            None
        }
    }

    /// The atlas key's style byte for this tile.
    pub fn style(self) -> u8 {
        STYLE_BIT | self as u8
    }

    pub fn from_style(style: u8) -> Option<Deco> {
        match style ^ STYLE_BIT {
            1 => Some(Deco::Curl),
            2 => Some(Deco::Dotted),
            3 => Some(Deco::Dashed),
            _ => None,
        }
    }
}

/// Coverage for the rows `top..top + h` of a cell `w` pixels wide.
pub struct Tile {
    pub w: u32,
    pub h: u32,
    pub top: i32,
    pub data: Vec<u8>,
}

/// Draw one tile. `ul_y` and `ul_h` are where a plain underline sits in the
/// cell and how thick it is.
pub fn tile(kind: Deco, cell_w: u32, cell_h: u32, ul_y: f32, ul_h: f32) -> Tile {
    let (w, ch) = (cell_w.max(1), cell_h.max(1));
    let (fw, fh) = (w as f32, ch as f32);
    let th = ul_h.max(1.0);
    let mid = ul_y + th * 0.5;
    // Vertical reach of the shape above and below its centre line.
    let reach = match kind {
        Deco::Curl => curl_amplitude(th) + th * 0.5,
        Deco::Dotted => dot_radius(th),
        Deco::Dashed => th * 0.5,
    };
    // Keep the whole shape inside the cell: a clipped wave is a flat line.
    let mid = mid.min(fh - reach - 0.5).max(reach);
    let top = ((mid - reach - 1.0).floor() as i32).clamp(0, ch as i32 - 1);
    let bottom = ((mid + reach + 1.0).ceil() as i32).clamp(top + 1, ch as i32);
    let h = (bottom - top) as u32;
    let mut data = vec![0u8; (w * h) as usize];

    // Coverage by 4x4 supersampling of "is this point inside the shape".
    let mut paint = |inside: &dyn Fn(f32, f32) -> bool| {
        for y in 0..h {
            for x in 0..w {
                let mut n = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let px = x as f32 + (sx as f32 + 0.5) / 4.0;
                        let py = top as f32 + y as f32 + (sy as f32 + 0.5) / 4.0;
                        n += inside(px, py) as u32;
                    }
                }
                data[(y * w + x) as usize] = (n * 255 / 16) as u8;
            }
        }
    };

    match kind {
        Deco::Curl => {
            // One full wave per cell, so both height and slope match at the
            // edge and the curve continues smoothly into the next cell.
            let amp = curl_amplitude(th);
            let k = std::f32::consts::TAU / fw;
            paint(&|px, py| {
                let dy = py - (mid + amp * (px * k).sin());
                let slope = amp * k * (px * k).cos();
                // Distance to the curve, to first order.
                dy.abs() / (1.0 + slope * slope).sqrt() <= th * 0.5
            });
        }
        Deco::Dotted => {
            // Round dots, a whole number of them per cell, evenly spaced.
            let r = dot_radius(th);
            let n = (fw / (r * 4.0)).round().max(1.0);
            let pitch = fw / n;
            paint(&|px, py| {
                let cx = ((px / pitch).floor() + 0.5) * pitch;
                (px - cx).powi(2) + (py - mid).powi(2) <= r * r
            });
        }
        Deco::Dashed => {
            // One dash per cell, centred: dashes and gaps alternate evenly
            // along the line and no dash is ever cut by a cell edge.
            let dash = (fw * 0.5).round().max(1.0);
            let start = ((fw - dash) * 0.5).floor();
            let (y0, y1) = ((mid - th * 0.5).round(), (mid + th * 0.5).round());
            paint(&|px, py| px >= start && px < start + dash && py >= y0 && py < y1);
        }
    }
    Tile { w, h, top, data }
}

fn curl_amplitude(th: f32) -> f32 {
    (th * 1.25).max(1.5)
}

fn dot_radius(th: f32) -> f32 {
    (th * 0.7).max(0.8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiles(kind: Deco) -> Vec<(Tile, u32, u32, f32)> {
        // Cell sizes and underline metrics as the fonts module computes them
        // for 11, 15 and 22 point text on a 2x display, and 12 point on a 1x.
        [(13u32, 27u32, 22.0f32, 2.0f32), (18, 37, 30.0, 2.0), (26, 54, 44.0, 3.0), (7, 15, 12.0, 1.0)]
            .into_iter()
            .map(|(w, h, y, t)| (tile(kind, w, h, y, t), w, h, t))
            .collect()
    }

    fn column_centre(t: &Tile, x: u32) -> Option<f32> {
        let (mut sum, mut weight) = (0.0f32, 0.0f32);
        for y in 0..t.h {
            let a = t.data[(y * t.w + x) as usize] as f32;
            sum += a * (y as f32 + 0.5);
            weight += a;
        }
        (weight > 0.0).then(|| t.top as f32 + sum / weight)
    }

    #[test]
    fn the_curl_is_a_wave_that_continues_across_cells() {
        for (t, w, h, th) in tiles(Deco::Curl) {
            assert!(t.top >= 0 && t.top as u32 + t.h <= h, "stays inside the cell");
            let centres: Vec<f32> = (0..w).map(|x| column_centre(&t, x).expect("ink in every column")).collect();
            let (lo, hi) = centres.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &c| (lo.min(c), hi.max(c)));
            // It goes up and down by more than its own thickness: not a line, not two lines.
            assert!(hi - lo >= th.max(1.5), "{w}x{h}: swing {}", hi - lo);
            // One period: the first half sits below the middle, the second above (or the reverse).
            let mid = (lo + hi) * 0.5;
            let first: f32 = centres[..w as usize / 2].iter().map(|c| c - mid).sum();
            let second: f32 = centres[w as usize / 2..].iter().map(|c| c - mid).sum();
            assert!(first * second < 0.0, "{w}x{h}");
            // Where one cell ends the next begins without a jump: the step
            // across the edge is no bigger than the steps inside the cell.
            let inside = centres.windows(2).map(|p| (p[1] - p[0]).abs()).fold(0.0f32, f32::max);
            let across = (centres[0] - centres[w as usize - 1]).abs();
            assert!(across <= inside + 0.3, "{w}x{h}: step {across} at the edge, {inside} inside");
            // Each column carries about one stroke's worth of ink: a single curve.
            for x in 0..w {
                let ink: f32 = (0..t.h).map(|y| t.data[(y * t.w + x) as usize] as f32 / 255.0).sum();
                assert!(ink > th * 0.6 && ink < th * 2.2, "{w}x{h} column {x}: {ink}");
            }
        }
    }

    #[test]
    fn dots_and_dashes_repeat_evenly() {
        for (t, w, _, _) in tiles(Deco::Dotted) {
            // Ink summed per column: dots are separated by empty columns or
            // near-empty ones, and the pattern is the same from either end.
            let cols: Vec<u32> = (0..w).map(|x| (0..t.h).map(|y| t.data[(y * t.w + x) as usize] as u32).sum()).collect();
            let peaks = (0..w as usize).filter(|&i| cols[i] > cols[(i + w as usize - 1) % w as usize] && cols[i] >= cols[(i + 1) % w as usize]).count();
            assert!(peaks >= 2, "{w}: {peaks} dots in {cols:?}");
            let low = *cols.iter().min().unwrap();
            let high = *cols.iter().max().unwrap();
            assert!(low * 4 < high.max(1), "{w}: dots are separate ({cols:?})");
            let mirrored: Vec<u32> = cols.iter().rev().copied().collect();
            assert!(cols.iter().zip(&mirrored).all(|(a, b)| a.abs_diff(*b) <= 255), "{w}: symmetric in the cell ({cols:?})");
        }
        for (t, w, _, th) in tiles(Deco::Dashed) {
            let row = t.h / 2;
            let inked: Vec<u32> = (0..w).filter(|&x| t.data[(row * t.w + x) as usize] == 255).collect();
            // One solid dash about half the cell wide, with gaps on both sides.
            assert!(inked.len() as u32 >= w / 2 - 1 && inked.len() as u32 <= w / 2 + 1, "{w}: {inked:?}");
            assert!(inked[0] > 0 && inked[inked.len() - 1] < w - 1);
            assert_eq!(inked[inked.len() - 1] - inked[0] + 1, inked.len() as u32, "one dash, unbroken");
            let rows = (0..t.h).filter(|&y| t.data[(y * t.w + inked[0]) as usize] == 255).count();
            assert_eq!(rows as f32, th, "as thick as a plain underline");
        }
    }

    #[test]
    fn styles_round_trip() {
        for d in [Deco::Curl, Deco::Dotted, Deco::Dashed] {
            assert_eq!(Deco::from_style(d.style()), Some(d));
        }
        assert_eq!(Deco::from_style(0), None);
        assert_eq!(Deco::from_style(3), None);
        assert_eq!(Deco::of(Flags::UNDERLINE | Flags::BOLD), None);
        assert_eq!(Deco::of(Flags::UNDERCURL), Some(Deco::Curl));
    }
}
