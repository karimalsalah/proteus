//! Where PROTEUS keeps its data on disk.
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn data_dir() -> PathBuf {
    // `PROTEUS_DATA` redirects everything (used by tests).
    let d = match std::env::var_os("PROTEUS_DATA") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("proteus"),
    };
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn journal_dir() -> PathBuf {
    let d = data_dir().join("journal");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Append one line to the log file. Never panics.
pub fn log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(data_dir().join("proteus.log")) {
        let _ = writeln!(f, "[{}] {}", unix_secs(), msg);
    }
}
