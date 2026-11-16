//! Embeds the icon into the executable and stamps the build.
//!
//! The icon is written straight into a Windows resource file (`.res`), which
//! the MSVC linker accepts as an input, so no resource compiler is needed.
use std::path::PathBuf;

#[path = "src/icon.rs"]
mod icon;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;

/// One resource record: 32-byte header (ordinal type and name), data, padding.
fn entry(buf: &mut Vec<u8>, ty: u16, id: u16, flags: u16, lang: u16, data: &[u8]) {
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(&32u32.to_le_bytes());
    buf.extend_from_slice(&[0xFF, 0xFF]);
    buf.extend_from_slice(&ty.to_le_bytes());
    buf.extend_from_slice(&[0xFF, 0xFF]);
    buf.extend_from_slice(&id.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // data version
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&lang.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // version
    buf.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    buf.extend_from_slice(data);
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

/// Classic icon image: header, BGRA pixels bottom-up, then an empty 1-bit mask.
fn dib(size: u32) -> Vec<u8> {
    let rgba = icon::rgba(size);
    let mut d = Vec::new();
    d.extend_from_slice(&40u32.to_le_bytes());
    d.extend_from_slice(&(size as i32).to_le_bytes());
    d.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&32u16.to_le_bytes());
    d.extend_from_slice(&[0u8; 24]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            d.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    let mask_row = size.div_ceil(32) * 4;
    d.resize(d.len() + (mask_row * size) as usize, 0);
    d
}

fn png(size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().expect("png header");
        w.write_image_data(&icon::rgba(size)).expect("png data");
    }
    out
}

fn resources() -> Vec<u8> {
    let mut buf = Vec::new();
    entry(&mut buf, 0, 0, 0, 0, &[]); // every .res file starts with an empty record
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 256];
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    for (n, &size) in sizes.iter().enumerate() {
        let id = n as u16 + 1;
        let image = if size >= 256 { png(size) } else { dib(size) };
        entry(&mut buf, RT_ICON, id, 0x1010, 0x0409, &image);
        let dim = if size >= 256 { 0u8 } else { size as u8 };
        group.extend_from_slice(&[dim, dim, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&(image.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut buf, RT_GROUP_ICON, 1, 0x1030, 0x0409, &group);
    buf
}

fn main() {
    // Re-run on any source change so the build stamp always moves.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor");
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    println!("cargo:rustc-env=PROTEUS_BUILD={stamp}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("proteus.res");
        std::fs::write(&out, resources()).expect("write resources");
        println!("cargo:rustc-link-arg-bins={}", out.display());
    }
}
