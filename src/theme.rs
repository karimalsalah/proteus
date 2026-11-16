//! Colour. Everything the renderer draws resolves through a `Theme`, and
//! programs that ask the terminal for its colours get answers from the same
//! table, so well-behaved agents adopt the look on their own.

pub type C3 = [u8; 3];

#[derive(Clone, Debug)]
pub struct Theme {
    pub bg: C3,
    pub fg: C3,
    pub cursor: C3,
    pub accent: C3,
    pub selection_bg: C3,
    pub ansi: [C3; 16],
}

pub fn hex(s: &str) -> C3 {
    let s = s.trim_start_matches('#');
    let p = |i: usize| u8::from_str_radix(s.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0);
    [p(0), p(2), p(4)]
}

impl Theme {
    /// "Void + light": near-black ground, cool neutral text, one luminous accent.
    pub fn void_light() -> Self {
        Self {
            bg: hex("#05070d"),
            fg: hex("#dfe6f0"),
            cursor: hex("#5ee6ff"),
            accent: hex("#5ee6ff"),
            selection_bg: hex("#16405a"),
            ansi: [
                hex("#1b2333"), hex("#ff7a90"), hex("#4fe3a6"), hex("#ffcf5c"),
                hex("#6ea8ff"), hex("#e58bff"), hex("#5ee6ff"), hex("#c9d3e2"),
                hex("#5b6a82"), hex("#ffa3b3"), hex("#86f0c3"), hex("#ffe08f"),
                hex("#9cc4ff"), hex("#f0b3ff"), hex("#9ff1ff"), hex("#f7fafd"),
            ],
        }
    }

    /// The 256-colour palette: 16 theme colours, a 6x6x6 cube, 24 greys.
    pub fn indexed(&self, i: u8) -> C3 {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                let i = i - 16;
                let lv = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                [lv(i / 36), lv((i / 6) % 6), lv(i % 6)]
            }
            _ => {
                let v = 8 + (i - 232) * 10;
                [v, v, v]
            }
        }
    }

    /// Answer a colour query by terminal colour index (0-255 palette,
    /// 256 foreground, 257 background, 258 cursor).
    pub fn query(&self, idx: usize) -> C3 {
        match idx {
            0..=255 => self.indexed(idx as u8),
            256 => self.fg,
            257 => self.bg,
            258 => self.cursor,
            _ => self.fg,
        }
    }
}

pub fn srgb_to_linear(c: u8) -> f32 {
    let v = c as f32 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB bytes to linear premultipliable RGBA.
pub fn lin(c: C3, a: f32) -> [f32; 4] {
    [srgb_to_linear(c[0]), srgb_to_linear(c[1]), srgb_to_linear(c[2]), a]
}

/// Blend `a` toward `b` by `t` in sRGB space (good enough for dim text).
pub fn mix(a: C3, b: C3, t: f32) -> C3 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

/// Is this colour red (-1), green (1) or neither (0)? Programs print failures
/// in red and successes in green; the terminal listens for that. Orange,
/// yellow, pink and unsaturated colours are neither.
pub fn tint(c: C3) -> i8 {
    // Whole numbers only: this runs for every glyph of every build.
    let (r, g, b) = (c[0] as i32, c[1] as i32, c[2] as i32);
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    // Saturated enough to have a hue at all (dim red still counts).
    if d < 40 || d * 4 < max {
        return 0;
    }
    if max == r {
        // Hue is 60(g-b)/d degrees: red from -25 up to 12.
        let x = (g - b) * 60;
        (x < 12 * d && x > -25 * d) as i8 * -1
    } else if max == g {
        // Hue is 120 + 60(b-r)/d degrees: green from 90 up to 170.
        let x = (b - r) * 60;
        (x >= -30 * d && x < 50 * d) as i8
    } else {
        0
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn red_and_green_are_recognised_and_nothing_else_is() {
        let t = Theme::void_light();
        for i in [1, 9] {
            assert_eq!(tint(t.ansi[i]), -1, "ansi {i}");
            assert_eq!(tint(mix(t.ansi[i], t.bg, 0.42)), -1, "dim ansi {i}");
        }
        for i in [2, 10] {
            assert_eq!(tint(t.ansi[i]), 1, "ansi {i}");
        }
        for i in [0, 3, 4, 5, 6, 7, 8, 11, 12, 13, 14, 15] {
            assert_eq!(tint(t.ansi[i]), 0, "ansi {i}");
        }
        assert_eq!(tint([255, 0, 0]), -1);
        assert_eq!(tint([205, 49, 49]), -1);
        assert_eq!(tint([0, 200, 0]), 1);
        assert_eq!(tint(t.fg), 0);
        assert_eq!(tint(hex("#d97757")), 0, "orange is not an alarm");
    }
    #[test]
    fn palette_cube_and_greys() {
        let t = Theme::void_light();
        assert_eq!(t.indexed(16), [0, 0, 0]);
        assert_eq!(t.indexed(231), [255, 255, 255]);
        assert_eq!(t.indexed(196), [255, 0, 0]);
        assert_eq!(t.indexed(232), [8, 8, 8]);
        assert_eq!(t.indexed(255), [238, 238, 238]);
        assert_eq!(t.query(257), t.bg);
    }
}
