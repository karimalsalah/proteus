//! Characters drawn by the engine instead of a font: box drawing, block
//! elements, braille and powerline shapes. They are generated at the exact
//! cell size so lines join seamlessly at any font size or line height.

/// Returns a cell-sized coverage bitmap, or `None` to fall back to a font.
pub fn render(ch: char, w: u32, h: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 {
        return None;
    }
    let cp = ch as u32;
    let mut c = Canvas::new(w as i32, h as i32);
    match cp {
        0x2500..=0x257F => {
            if !box_drawing(&mut c, cp - 0x2500) {
                return None;
            }
        }
        0x2580..=0x259F => block(&mut c, cp),
        0x2800..=0x28FF => braille(&mut c, cp - 0x2800),
        0xE0B0 | 0xE0B2 | 0xE0B4 | 0xE0B6 => powerline(&mut c, cp),
        _ => return None,
    }
    Some(c.d)
}

struct Canvas {
    w: i32,
    h: i32,
    d: Vec<u8>,
    light: i32,
}

impl Canvas {
    fn new(w: i32, h: i32) -> Self {
        let light = ((w as f32 / 9.0).round() as i32).max(1);
        Self { w, h, d: vec![0; (w * h) as usize], light }
    }

    fn fill(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, a: u8) {
        let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(self.w), y1.min(self.h));
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y * self.w + x) as usize;
                self.d[i] = self.d[i].max(a);
            }
        }
    }

    fn clear(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(self.w), y1.min(self.h));
        for y in y0..y1 {
            for x in x0..x1 {
                self.d[(y * self.w + x) as usize] = 0;
            }
        }
    }

    fn put(&mut self, x: i32, y: i32, a: f32) {
        if x >= 0 && y >= 0 && x < self.w && y < self.h {
            let i = (y * self.w + x) as usize;
            self.d[i] = self.d[i].max((a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        }
    }

    /// Fill by 4x4 supersampling a shape predicate over the unit square.
    fn shape(&mut self, inside: impl Fn(f32, f32) -> bool) {
        for y in 0..self.h {
            for x in 0..self.w {
                let mut n = 0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let u = (x as f32 + (sx as f32 + 0.5) / 4.0) / self.w as f32;
                        let v = (y as f32 + (sy as f32 + 0.5) / 4.0) / self.h as f32;
                        if inside(u, v) {
                            n += 1;
                        }
                    }
                }
                self.put(x, y, n as f32 / 16.0);
            }
        }
    }
}

/// Arm weights (up, right, down, left): 0 none, 1 light, 2 heavy.
/// Index is the code point minus U+2500; 9 marks "handled elsewhere".
const X: [u8; 4] = [9, 9, 9, 9];
#[rustfmt::skip]
const ARMS: [[u8; 4]; 0x4C] = [
    [0,1,0,1],[0,2,0,2],[1,0,1,0],[2,0,2,0], X,X,X,X, X,X,X,X,
    [0,1,1,0],[0,2,1,0],[0,1,2,0],[0,2,2,0],
    [0,0,1,1],[0,0,1,2],[0,0,2,1],[0,0,2,2],
    [1,1,0,0],[1,2,0,0],[2,1,0,0],[2,2,0,0],
    [1,0,0,1],[1,0,0,2],[2,0,0,1],[2,0,0,2],
    [1,1,1,0],[1,2,1,0],[2,1,1,0],[1,1,2,0],[2,1,2,0],[2,2,1,0],[1,2,2,0],[2,2,2,0],
    [1,0,1,1],[1,0,1,2],[2,0,1,1],[1,0,2,1],[2,0,2,1],[2,0,1,2],[1,0,2,2],[2,0,2,2],
    [0,1,1,1],[0,1,1,2],[0,2,1,1],[0,2,1,2],[0,1,2,1],[0,1,2,2],[0,2,2,1],[0,2,2,2],
    [1,1,0,1],[1,1,0,2],[1,2,0,1],[1,2,0,2],[2,1,0,1],[2,1,0,2],[2,2,0,1],[2,2,0,2],
    [1,1,1,1],[1,1,1,2],[1,2,1,1],[1,2,1,2],[2,1,1,1],[1,1,2,1],[2,1,2,1],[2,1,1,2],
    [2,2,1,1],[1,1,2,2],[1,2,2,1],[2,2,1,2],[1,2,2,2],[2,1,2,2],[2,2,2,1],[2,2,2,2],
];

fn box_drawing(c: &mut Canvas, i: u32) -> bool {
    match i {
        // Dashed lines: (segments, horizontal, heavy).
        0x04 => dashes(c, 3, true, false),
        0x05 => dashes(c, 3, true, true),
        0x06 => dashes(c, 3, false, false),
        0x07 => dashes(c, 3, false, true),
        0x08 => dashes(c, 4, true, false),
        0x09 => dashes(c, 4, true, true),
        0x0A => dashes(c, 4, false, false),
        0x0B => dashes(c, 4, false, true),
        0x4C => dashes(c, 2, true, false),
        0x4D => dashes(c, 2, true, true),
        0x4E => dashes(c, 2, false, false),
        0x4F => dashes(c, 2, false, true),
        0x50..=0x6C => double(c, DOUBLE[(i - 0x50) as usize]),
        0x6D..=0x70 => arc(c, i),
        0x71 => diagonal(c, true, false),
        0x72 => diagonal(c, false, true),
        0x73 => diagonal(c, true, true),
        0x74 => lines(c, [0, 0, 0, 1]),
        0x75 => lines(c, [1, 0, 0, 0]),
        0x76 => lines(c, [0, 1, 0, 0]),
        0x77 => lines(c, [0, 0, 1, 0]),
        0x78 => lines(c, [0, 0, 0, 2]),
        0x79 => lines(c, [2, 0, 0, 0]),
        0x7A => lines(c, [0, 2, 0, 0]),
        0x7B => lines(c, [0, 0, 2, 0]),
        0x7C => lines(c, [0, 2, 0, 1]),
        0x7D => lines(c, [1, 0, 2, 0]),
        0x7E => lines(c, [0, 1, 0, 2]),
        0x7F => lines(c, [2, 0, 1, 0]),
        _ => match ARMS.get(i as usize) {
            Some(a) if a[0] != 9 => lines(c, *a),
            _ => return false,
        },
    }
    true
}

fn lines(c: &mut Canvas, arms: [u8; 4]) {
    let (w, h) = (c.w, c.h);
    let t = |weight: u8| match weight {
        1 => c.light,
        2 => c.light * 2,
        _ => 0,
    };
    let (tu, tr, td, tl) = (t(arms[0]), t(arms[1]), t(arms[2]), t(arms[3]));
    let vmax = tu.max(td);
    let hmax = tl.max(tr);
    let cx0 = |tw: i32| (w - tw) / 2;
    let cy0 = |th: i32| (h - th) / 2;
    // Each arm runs from its edge to the far side of the joint, so corners
    // and tees are filled solid whatever mix of weights meets there.
    if tu > 0 {
        let yend = if hmax > 0 { cy0(hmax) + hmax } else { (h + 1) / 2 };
        c.fill(cx0(tu), 0, cx0(tu) + tu, yend, 255);
    }
    if td > 0 {
        let ystart = if hmax > 0 { cy0(hmax) } else { h / 2 };
        c.fill(cx0(td), ystart, cx0(td) + td, h, 255);
    }
    if tl > 0 {
        let xend = if vmax > 0 { cx0(vmax) + vmax } else { (w + 1) / 2 };
        c.fill(0, cy0(tl), xend, cy0(tl) + tl, 255);
    }
    if tr > 0 {
        let xstart = if vmax > 0 { cx0(vmax) } else { w / 2 };
        c.fill(xstart, cy0(tr), w, cy0(tr) + tr, 255);
    }
}

/// U+2550 to U+256C. Arms (up, right, down, left): 0 none, 1 single, 2 double.
#[rustfmt::skip]
const DOUBLE: [[u8; 4]; 29] = [
    [0,2,0,2],[2,0,2,0],
    [0,2,1,0],[0,1,2,0],[0,2,2,0], [0,0,1,2],[0,0,2,1],[0,0,2,2],
    [1,2,0,0],[2,1,0,0],[2,2,0,0], [1,0,0,2],[2,0,0,1],[2,0,0,2],
    [1,2,1,0],[2,1,2,0],[2,2,2,0], [1,0,1,2],[2,0,2,1],[2,0,2,2],
    [0,2,1,2],[0,1,2,1],[0,2,2,2], [1,2,0,2],[2,1,0,1],[2,2,0,2],
    [1,2,1,2],[2,1,2,1],[2,2,2,2],
];

/// Double lines: two strokes of the light weight with a channel of the same
/// width between them, the channel sitting exactly where a single line
/// would run. Where double meets double the channel turns the corner or
/// runs through the junction; where double meets single the single stroke
/// stops at the nearest stroke of the pair (or crosses both, in a cross).
fn double(c: &mut Canvas, arms: [u8; 4]) {
    let (w, h, t) = (c.w, c.h, c.light);
    let (x0, y0) = ((w - t) / 2, (h - t) / 2);
    let [u, r, d, l] = arms;
    let vertical_double = u == 2 || d == 2;
    let horizontal_double = l == 2 || r == 2;
    if !(vertical_double && horizontal_double) && (vertical_double || horizontal_double) && arms.contains(&1) {
        if vertical_double {
            let (top, bottom) = (if u == 2 { 0 } else { y0 }, if d == 2 { h } else { y0 + t });
            c.fill(x0 - t, top, x0, bottom, 255);
            c.fill(x0 + t, top, x0 + 2 * t, bottom, 255);
            let through = u == 2 && d == 2;
            let from = if l == 1 { 0 } else if through { x0 + t } else { x0 - t };
            let to = if r == 1 { w } else if through { x0 } else { x0 + 2 * t };
            c.fill(from, y0, to, y0 + t, 255);
        } else {
            let (left, right) = (if l == 2 { 0 } else { x0 }, if r == 2 { w } else { x0 + t });
            c.fill(left, y0 - t, right, y0, 255);
            c.fill(left, y0 + t, right, y0 + 2 * t, 255);
            let through = l == 2 && r == 2;
            let from = if u == 1 { 0 } else if through { y0 + t } else { y0 - t };
            let to = if d == 1 { h } else if through { y0 } else { y0 + 2 * t };
            c.fill(x0, from, x0 + t, to, 255);
        }
        return;
    }
    // Double on every arm present: draw the arms three strokes wide, each to
    // the far side of the junction, then cut the channel out of each.
    let (any_v, any_h) = (u + d > 0, l + r > 0);
    if u > 0 {
        c.fill(x0 - t, 0, x0 + 2 * t, if any_h { y0 + 2 * t } else { (h + 1) / 2 }, 255);
    }
    if d > 0 {
        c.fill(x0 - t, if any_h { y0 - t } else { h / 2 }, x0 + 2 * t, h, 255);
    }
    if l > 0 {
        c.fill(0, y0 - t, if any_v { x0 + 2 * t } else { (w + 1) / 2 }, y0 + 2 * t, 255);
    }
    if r > 0 {
        c.fill(if any_v { x0 - t } else { w / 2 }, y0 - t, w, y0 + 2 * t, 255);
    }
    if u > 0 {
        c.clear(x0, 0, x0 + t, y0 + t);
    }
    if d > 0 {
        c.clear(x0, y0, x0 + t, h);
    }
    if l > 0 {
        c.clear(0, y0, x0 + t, y0 + t);
    }
    if r > 0 {
        c.clear(x0, y0, w, y0 + t);
    }
}

/// ╱ ╲ ╳: corner to corner, so a run of them is one unbroken line.
fn diagonal(c: &mut Canvas, rising: bool, falling: bool) {
    let (w, h) = (c.w as f32, c.h as f32);
    let half = c.light as f32 * 0.5;
    let len = (w * w + h * h).sqrt();
    for y in 0..c.h {
        for x in 0..c.w {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // Distance from the pixel centre to each diagonal of the cell.
            let fall = ((px * h - py * w) / len).abs();
            let rise = ((px * h + py * w - w * h) / len).abs();
            let d = match (rising, falling) {
                (true, true) => rise.min(fall),
                (true, false) => rise,
                _ => fall,
            };
            c.put(x, y, half + 0.5 - d);
        }
    }
}

fn dashes(c: &mut Canvas, n: i32, horizontal: bool, heavy: bool) {
    let t = if heavy { c.light * 2 } else { c.light };
    let len = if horizontal { c.w } else { c.h };
    let gap = (len / (n * 4)).max(1);
    for k in 0..n {
        let a = k * len / n + gap / 2;
        let b = (k + 1) * len / n - (gap - gap / 2);
        if horizontal {
            let y = (c.h - t) / 2;
            c.fill(a, y, b, y + t, 255);
        } else {
            let x = (c.w - t) / 2;
            c.fill(x, a, x + t, b, 255);
        }
    }
}

/// Rounded corners: a quarter circle joined to straight runs that continue
/// to the cell edges, anti-aliased by distance to the stroke centre line.
fn arc(c: &mut Canvas, i: u32) {
    let (w, h) = (c.w as f32, c.h as f32);
    let t = c.light as f32;
    let cx = ((c.w - c.light) / 2) as f32 + t * 0.5;
    let cy = ((c.h - c.light) / 2) as f32 + t * 0.5;
    let r = (w * 0.5).min(h * 0.5);
    // Which way the two arms go: +1 right/down, -1 left/up.
    let (sx, sy) = match i {
        0x6D => (1.0, 1.0),   // ╭ down and right
        0x6E => (-1.0, 1.0),  // ╮ down and left
        0x6F => (-1.0, -1.0), // ╯ up and left
        _ => (1.0, -1.0),     // ╰ up and right
    };
    let (ox, oy) = (cx + sx * r, cy + sy * r);
    for y in 0..c.h {
        for x in 0..c.w {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let (dx, dy) = ((px - ox) * sx, (py - oy) * sy);
            let d = if dx <= 0.0 && dy <= 0.0 {
                ((dx * dx + dy * dy).sqrt() - r).abs()
            } else if dy > 0.0 && dx <= 0.0 {
                (px - cx).abs()
            } else if dx > 0.0 && dy <= 0.0 {
                (py - cy).abs()
            } else {
                f32::MAX
            };
            c.put(x, y, t * 0.5 + 0.5 - d);
        }
    }
    let _ = (w, h);
}

fn block(c: &mut Canvas, cp: u32) {
    let (w, h) = (c.w, c.h);
    let eighth_h = |n: i32| (h * n + 4) / 8;
    let eighth_w = |n: i32| (w * n + 4) / 8;
    let (mx, my) = ((w + 1) / 2, (h + 1) / 2);
    match cp {
        0x2580 => c.fill(0, 0, w, my, 255),
        0x2581..=0x2588 => c.fill(0, h - eighth_h((cp - 0x2580) as i32), w, h, 255),
        0x2589..=0x258F => c.fill(0, 0, eighth_w((0x2590 - cp) as i32), h, 255),
        0x2590 => c.fill(mx, 0, w, h, 255),
        0x2591 => c.fill(0, 0, w, h, 64),
        0x2592 => c.fill(0, 0, w, h, 128),
        0x2593 => c.fill(0, 0, w, h, 191),
        0x2594 => c.fill(0, 0, w, eighth_h(1), 255),
        0x2595 => c.fill(w - eighth_w(1), 0, w, h, 255),
        _ => {
            // Quadrants: bit 0 upper-left, 1 upper-right, 2 lower-left, 3 lower-right.
            let q: u8 = match cp {
                0x2596 => 0b0100,
                0x2597 => 0b1000,
                0x2598 => 0b0001,
                0x2599 => 0b1101,
                0x259A => 0b1001,
                0x259B => 0b0111,
                0x259C => 0b1011,
                0x259D => 0b0010,
                0x259E => 0b0110,
                _ => 0b1110,
            };
            if q & 1 != 0 {
                c.fill(0, 0, mx, my, 255);
            }
            if q & 2 != 0 {
                c.fill(mx, 0, w, my, 255);
            }
            if q & 4 != 0 {
                c.fill(0, my, mx, h, 255);
            }
            if q & 8 != 0 {
                c.fill(mx, my, w, h, 255);
            }
        }
    }
}

fn braille(c: &mut Canvas, bits: u32) {
    // Dot numbering to (column, row) in the 2x4 braille cell.
    const POS: [(u32, u32); 8] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
    let (w, h) = (c.w as f32, c.h as f32);
    let r = (w / 2.0).min(h / 4.0) * 0.30;
    for (n, (col, row)) in POS.iter().enumerate() {
        if bits & (1 << n) == 0 {
            continue;
        }
        let cx = (*col as f32 + 0.5) * w / 2.0;
        let cy = (*row as f32 + 0.5) * h / 4.0;
        let (x0, x1) = ((cx - r - 1.0).floor() as i32, (cx + r + 1.0).ceil() as i32);
        let (y0, y1) = ((cy - r - 1.0).floor() as i32, (cy + r + 1.0).ceil() as i32);
        for y in y0..y1 {
            for x in x0..x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                c.put(x, y, r + 0.5 - d);
            }
        }
    }
}

fn powerline(c: &mut Canvas, cp: u32) {
    let aspect = c.h as f32 / c.w as f32;
    match cp {
        0xE0B0 => c.shape(|u, v| u <= 1.0 - (2.0 * v - 1.0).abs()),
        0xE0B2 => c.shape(|u, v| 1.0 - u <= 1.0 - (2.0 * v - 1.0).abs()),
        0xE0B4 => c.shape(move |u, v| {
            let y = (v - 0.5) * aspect;
            u * u + y * y * 4.0 / (aspect * aspect) <= 1.0
        }),
        _ => c.shape(move |u, v| {
            let y = (v - 0.5) * aspect;
            (1.0 - u) * (1.0 - u) + y * y * 4.0 / (aspect * aspect) <= 1.0
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(d: &[u8], w: u32, x: u32, y: u32) -> u8 {
        d[(y * w + x) as usize]
    }

    #[test]
    fn lines_reach_the_cell_edges_so_neighbours_join() {
        let (w, h) = (18, 37);
        let hz = render('─', w, h).unwrap();
        let row = (0..h).find(|&y| at(&hz, w, 0, y) == 255).expect("line present");
        assert!((0..w).all(|x| at(&hz, w, x, row) == 255));
        let vt = render('│', w, h).unwrap();
        let col = (0..w).find(|&x| at(&vt, w, x, 0) == 255).expect("line present");
        assert!((0..h).all(|y| at(&vt, w, col, y) == 255));
        // A corner touches exactly the bottom and right edges.
        let corner = render('┌', w, h).unwrap();
        assert_eq!(at(&corner, w, col, h - 1), 255);
        assert_eq!(at(&corner, w, w - 1, row), 255);
        assert_eq!(at(&corner, w, col, 0), 0);
        assert_eq!(at(&corner, w, 0, row), 0);
        // A rounded corner meets the same edge positions.
        let round = render('╭', w, h).unwrap();
        assert!(at(&round, w, col, h - 1) > 200);
        assert!(at(&round, w, w - 1, row) > 200);
    }

    #[test]
    fn blocks_and_braille() {
        let (w, h) = (10, 20);
        let full = render('█', w, h).unwrap();
        assert!(full.iter().all(|&v| v == 255));
        let lower = render('▄', w, h).unwrap();
        assert_eq!(at(&lower, w, 5, 0), 0);
        assert_eq!(at(&lower, w, 5, h - 1), 255);
        assert!(render('⠀', w, h).unwrap().iter().all(|&v| v == 0));
        assert!(render('⣿', w, h).unwrap().iter().any(|&v| v > 200));
        assert!(render('A', w, h).is_none());
    }

    /// Rows (or columns) that are fully inked along one edge of the cell.
    fn edge(d: &[u8], w: u32, h: u32, side: char) -> Vec<u32> {
        match side {
            'l' => (0..h).filter(|&y| at(d, w, 0, y) == 255).collect(),
            'r' => (0..h).filter(|&y| at(d, w, w - 1, y) == 255).collect(),
            't' => (0..w).filter(|&x| at(d, w, x, 0) == 255).collect(),
            _ => (0..w).filter(|&x| at(d, w, x, h - 1) == 255).collect(),
        }
    }

    #[test]
    fn double_lines_are_two_strokes_and_join_their_neighbours() {
        for (w, h) in [(18u32, 37u32), (12, 25), (8, 16), (7, 15)] {
            let t = ((w as f32 / 9.0).round() as u32).max(1);
            let g = |ch: char| render(ch, w, h).unwrap_or_else(|| panic!("{ch} is drawn by the engine"));
            let hz = g('═');
            let rows = edge(&hz, w, h, 'l');
            // Two strokes of the light weight with a gap of the same width.
            assert_eq!(rows.len() as u32, 2 * t, "{w}x{h}");
            assert_eq!(rows[rows.len() - 1] - rows[0] + 1, 3 * t);
            assert!(rows.iter().all(|&y| (0..w).all(|x| at(&hz, w, x, y) == 255)), "strokes run edge to edge");
            let vt = g('║');
            let cols = edge(&vt, w, h, 't');
            assert_eq!(cols.len() as u32, 2 * t);
            // The channel between the strokes is where a single line runs.
            let single: Vec<u32> = edge(&g('─'), w, h, 'l');
            assert!(single.iter().all(|y| !rows.contains(y) && *y > rows[0] && *y < rows[rows.len() - 1]));

            // Every double arm meets ═ or ║ at the cell edge, stroke for stroke;
            // every single arm meets ─ or │; an absent arm leaves the edge empty.
            let single_cols: Vec<u32> = edge(&g('│'), w, h, 't');
            for (i, arms) in DOUBLE.iter().enumerate() {
                let ch = char::from_u32(0x2550 + i as u32).unwrap();
                let d = g(ch);
                for (arm, side) in arms.iter().zip(['t', 'r', 'b', 'l']) {
                    let got = edge(&d, w, h, side);
                    let horizontal = side == 'l' || side == 'r';
                    let want: &[u32] = match (arm, horizontal) {
                        (0, _) => &[],
                        (1, true) => &single,
                        (1, false) => &single_cols,
                        (_, true) => &rows,
                        (_, false) => &cols,
                    };
                    assert_eq!(got, want, "{ch} side {side} at {w}x{h}");
                }
            }
            // The channel runs through a double cross and around a double corner.
            let cross = g('╬');
            assert_eq!(at(&cross, w, single_cols[0], single[0]), 0, "the middle of ╬ is open");
            assert_eq!(at(&cross, w, cols[0], rows[0]), 255, "each quarter of ╬ has its own corner");
            let corner = g('╔');
            assert_eq!(at(&corner, w, cols[0], rows[0]), 255, "outer corner of ╔ is solid");
            assert_eq!(at(&corner, w, single_cols[0], single[0]), 0, "the channel turns the corner");
            // A single line crosses a double one without a break.
            let mixed = g('╫');
            assert!((0..w).all(|x| at(&mixed, w, x, single[0]) == 255));
        }
    }

    #[test]
    fn diagonals_run_corner_to_corner() {
        let (w, h) = (18u32, 37u32);
        let rise = render('╱', w, h).unwrap();
        assert!(at(&rise, w, w - 1, 0) > 100 && at(&rise, w, 0, h - 1) > 100);
        assert_eq!(at(&rise, w, 0, 0), 0);
        let fall = render('╲', w, h).unwrap();
        assert!(at(&fall, w, 0, 0) > 100 && at(&fall, w, w - 1, h - 1) > 100);
        let both = render('╳', w, h).unwrap();
        assert!(both.iter().zip(rise.iter().zip(&fall)).all(|(b, (r, f))| *b == *r.max(f)));
    }
}
