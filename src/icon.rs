//! The application icon, drawn by code so no image file has to ship.
//! A dark plate, a prompt chevron, and the cursor's light.
//!
//! This file is also compiled into the build script (to embed the icon in
//! the executable), so it must not depend on anything outside `std`.

fn sd_round_box(px: f32, py: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let (qx, qy) = (px.abs() - hx + r, py.abs() - hy + r);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (pax, pay, bax, bay) = (px - ax, py - ay, bx - ax, by - ay);
    let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay)).clamp(0.0, 1.0);
    ((pax - bax * h).powi(2) + (pay - bay * h).powi(2)).sqrt()
}

fn over(dst: &mut [f32; 3], src: [f32; 3], a: f32) {
    for i in 0..3 {
        dst[i] += (src[i] - dst[i]) * a.clamp(0.0, 1.0);
    }
}

/// `size` x `size` RGBA8 with straight alpha.
pub fn rgba(size: u32) -> Vec<u8> {
    let s = size as f32 / 256.0;
    // Small sizes need heavier strokes to stay legible.
    let bold = 1.0 + 0.9 * (1.0 - (size.min(64) as f32 / 64.0));
    let cov = |d: f32| (0.5 - d * s).clamp(0.0, 1.0);
    let accent = [0.37, 0.90, 1.0];
    let ink = [0.90, 0.93, 0.96];
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            // Work in a 256-unit design space whatever the output size.
            let (px, py) = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
            let plate = sd_round_box(px - 128.0, py - 128.0, 118.0, 118.0, 54.0);
            let t = py / 256.0;
            let mut col = [0.050 - 0.030 * t, 0.100 - 0.070 * t, 0.165 - 0.115 * t];

            let beam = sd_round_box(px - 164.0, py - 128.0, 9.0 * bold, 50.0, 9.0 * bold);
            let glow = (-(beam.max(0.0) / 30.0).powi(2)).exp();
            over(&mut col, accent, glow * 0.42);
            over(&mut col, accent, cov(beam));

            let chevron = sd_segment(px, py, 76.0, 80.0, 124.0, 128.0).min(sd_segment(px, py, 124.0, 128.0, 76.0, 176.0)) - 12.0 * bold;
            over(&mut col, ink, cov(chevron));

            // A thin lit edge so the plate reads on dark and light backgrounds.
            over(&mut col, accent, cov((plate + 2.5).abs() - 1.6) * 0.30);

            let i = ((y * size + x) * 4) as usize;
            for c in 0..3 {
                out[i + c] = (col[c].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
            out[i + 3] = (cov(plate) * 255.0 + 0.5) as u8;
        }
    }
    out
}
