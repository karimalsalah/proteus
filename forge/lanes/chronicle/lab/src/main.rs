//! Lab measurements behind chronicle's design choices. Not part of the product.
//!   cargo run --release -- CORPUS_DIR SCRATCH_DIR [MAX_MB] [MAX_FILES]
//! Hashes, compresses and stores the first MAX_FILES files of CORPUS_DIR (read
//! only) and reports: SHA-256 against BLAKE3, deflate against zstd, and one
//! pack file against one file per object. SCRATCH_DIR is created and removed.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn walk(dir: &Path, out: &mut Vec<PathBuf>, max: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if out.len() >= max {
            return;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), out, max);
        } else if ft.is_file() {
            out.push(e.path());
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let corpus = PathBuf::from(&a[1]);
    let out = PathBuf::from(&a[2]);
    let max_mb: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);
    let max_files: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let mut paths = Vec::new();
    walk(&corpus, &mut paths, usize::MAX);
    paths.sort();
    let mut files: Vec<Vec<u8>> = Vec::new();
    let mut total = 0usize;
    for p in &paths {
        if files.len() >= max_files || total >= max_mb << 20 {
            break;
        }
        if let Ok(b) = std::fs::read(p) {
            total += b.len();
            files.push(b);
        }
    }
    let mb = total as f64 / (1 << 20) as f64;
    println!("corpus: {} files, {:.1} MB", files.len(), mb);

    // Hashing, per file, one thread.
    for round in 0..2 {
        let t = Instant::now();
        let mut x = 0u8;
        for f in &files {
            x ^= blake3::hash(f).as_bytes()[0];
        }
        let s = t.elapsed().as_secs_f64();
        println!("blake3 per-file round {round}: {:.1} ms, {:.0} MB/s ({x})", s * 1e3, mb / s);
        let t = Instant::now();
        let mut x = 0u8;
        for f in &files {
            use sha2::Digest;
            x ^= sha2::Sha256::digest(f)[0];
        }
        let s = t.elapsed().as_secs_f64();
        println!("sha256 per-file round {round}: {:.1} ms, {:.0} MB/s ({x})", s * 1e3, mb / s);
    }
    // Hashing one contiguous buffer.
    let big: Vec<u8> = files.iter().flat_map(|f| f.iter().copied()).collect();
    let t = Instant::now();
    let h = blake3::hash(&big);
    let s = t.elapsed().as_secs_f64();
    println!("blake3 one buffer: {:.1} ms, {:.0} MB/s ({})", s * 1e3, mb / s, &h.to_hex()[..8]);
    let t = Instant::now();
    let h = {
        use sha2::Digest;
        sha2::Sha256::digest(&big)
    };
    let s = t.elapsed().as_secs_f64();
    println!("sha256 one buffer: {:.1} ms, {:.0} MB/s ({:02x})", s * 1e3, mb / s, h[0]);

    // Compression, per file.
    let t = Instant::now();
    let mut z = 0usize;
    for f in &files {
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(1));
        e.write_all(f).unwrap();
        z += e.finish().unwrap().len();
    }
    let s = t.elapsed().as_secs_f64();
    println!("deflate-1: {:.1} MB ({:.1}% of raw), {:.0} ms, {:.0} MB/s", z as f64 / 1048576.0, 100.0 * z as f64 / total as f64, s * 1e3, mb / s);
    for lvl in [1, 3] {
        let t = Instant::now();
        let mut z = 0usize;
        let mut comp: Vec<Vec<u8>> = Vec::new();
        for f in &files {
            let c = zstd::bulk::compress(f, lvl).unwrap();
            z += c.len();
            comp.push(c);
        }
        let s = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let mut n = 0usize;
        for (c, f) in comp.iter().zip(&files) {
            n += zstd::bulk::decompress(c, f.len()).unwrap().len();
        }
        let d = t.elapsed().as_secs_f64();
        println!(
            "zstd-{lvl}: {:.1} MB ({:.1}% of raw), compress {:.0} ms ({:.0} MB/s), decompress {:.0} ms ({:.0} MB/s) [{n}]",
            z as f64 / 1048576.0,
            100.0 * z as f64 / total as f64,
            s * 1e3,
            mb / s,
            d * 1e3,
            mb / d
        );
    }

    // Loose objects (one file per blob, two-level fan-out) against one pack file.
    let loose = out.join("loose");
    let _ = std::fs::remove_dir_all(&out);
    for i in 0..256 {
        std::fs::create_dir_all(loose.join(format!("{i:02x}"))).unwrap();
    }
    let hashes: Vec<String> = files.iter().map(|f| blake3::hash(f).to_hex().to_string()).collect();
    let t = Instant::now();
    let mut written = 0;
    for (f, h) in files.iter().zip(&hashes) {
        let p = loose.join(&h[..2]).join(&h[2..]);
        if !p.exists() {
            std::fs::write(&p, f).unwrap();
            written += 1;
        }
    }
    let s = t.elapsed().as_secs_f64();
    println!("loose write: {written} objects in {:.0} ms ({:.0} files/s)", s * 1e3, written as f64 / s);
    let t = Instant::now();
    let mut n = 0usize;
    for h in &hashes {
        n += std::fs::read(loose.join(&h[..2]).join(&h[2..])).unwrap().len();
    }
    let s = t.elapsed().as_secs_f64();
    println!("loose read: {:.0} ms ({n} bytes)", s * 1e3);

    let t = Instant::now();
    let mut seen = std::collections::HashSet::new();
    let mut pack = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(out.join("one.pack")).unwrap());
    let mut written = 0;
    for (f, h) in files.iter().zip(&hashes) {
        if seen.insert(h.clone()) {
            pack.write_all(f).unwrap();
            written += 1;
        }
    }
    let file = pack.into_inner().unwrap();
    let s_nosync = t.elapsed().as_secs_f64();
    file.sync_all().unwrap();
    let s = t.elapsed().as_secs_f64();
    println!("pack write: {written} objects in {:.0} ms ({:.0} ms before sync)", s * 1e3, s_nosync * 1e3);
    let t = Instant::now();
    let n = std::fs::read(out.join("one.pack")).unwrap().len();
    println!("pack read: {:.0} ms ({n} bytes)", t.elapsed().as_secs_f64() * 1e3);
    let _ = std::fs::remove_dir_all(&out);
}
