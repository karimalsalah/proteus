//! Lab: how fast can this machine create files, and does it scale with threads?
//!   mkfiles OUT_DIR FILES BYTES_EACH
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let out = PathBuf::from(&a[1]);
    let files: usize = a[2].parse().unwrap();
    let bytes: usize = a[3].parse().unwrap();
    let data = vec![b'x'; bytes];
    for threads in [1usize, 2, 4, 8, 16] {
        let dir = out.join(format!("t{threads}"));
        let _ = std::fs::remove_dir_all(&dir);
        for d in 0..100 {
            std::fs::create_dir_all(dir.join(format!("d{d}"))).unwrap();
        }
        let next = AtomicUsize::new(0);
        let t = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= files {
                        break;
                    }
                    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(dir.join(format!("d{}", i % 100)).join(format!("f{i}.txt"))).unwrap();
                    f.write_all(&data).unwrap();
                });
            }
        });
        let s = t.elapsed().as_secs_f64();
        println!("{threads:>2} threads: {files} files of {bytes} bytes in {:.0} ms ({:.0} files/s)", s * 1e3, files as f64 / s);
        let t = Instant::now();
        std::fs::remove_dir_all(&dir).unwrap();
        println!("            removed in {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
    }
    let _ = std::fs::remove_dir_all(&out);
}
