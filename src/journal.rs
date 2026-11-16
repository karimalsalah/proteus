//! The journal: an append-only, timestamped record of everything a session
//! printed. It is the single source of truth for replay, time travel and
//! deterministic visual tests.
//!
//! Layout: `MAGIC | u32 meta_len | meta json | record*`
//! Record: `u8 kind | u64 micros since start | u32 len | payload` (little endian).
//!
//! Keyboard input is deliberately never recorded: it can contain passwords.
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::Instant;

pub const MAGIC: &[u8; 8] = b"PRTJ0001";
pub const K_OUTPUT: u8 = 1;
pub const K_RESIZE: u8 = 2;
#[allow(dead_code)]
pub const K_MARK: u8 = 3;
pub const K_EXIT: u8 = 4;
/// A process started or ended under the session's shell: one JSON object
/// (see `procs.rs`). Numbered apart from the kinds above (it is ASCII 'P') so
/// that record kinds added elsewhere at the same time do not collide with it.
pub const K_PROC: u8 = 0x50;

pub struct JournalWriter {
    w: BufWriter<File>,
    start: Instant,
    dirty: bool,
}

impl JournalWriter {
    pub fn create(path: &Path, meta: &serde_json::Value) -> io::Result<Self> {
        let mut w = BufWriter::with_capacity(1 << 16, File::create(path)?);
        w.write_all(MAGIC)?;
        let m = serde_json::to_vec(meta).unwrap_or_default();
        w.write_all(&(m.len() as u32).to_le_bytes())?;
        w.write_all(&m)?;
        w.flush()?;
        Ok(Self { w, start: Instant::now(), dirty: false })
    }

    pub fn record(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let t = self.start.elapsed().as_micros() as u64;
        self.w.write_all(&[kind])?;
        self.w.write_all(&t.to_le_bytes())?;
        self.w.write_all(&(payload.len() as u32).to_le_bytes())?;
        self.w.write_all(payload)?;
        self.dirty = true;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.dirty {
            self.dirty = false;
            self.w.flush()?;
        }
        Ok(())
    }
}

pub struct Record {
    pub kind: u8,
    pub t_us: u64,
    pub data: Vec<u8>,
}

/// Read a whole journal. A truncated final record (crash mid-write) is ignored.
pub fn read(path: &Path) -> io::Result<(serde_json::Value, Vec<Record>)> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 12 || &bytes[..8] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a PROTEUS journal"));
    }
    let mlen = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if bytes.len() < 12 + mlen {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "journal header truncated"));
    }
    let meta = serde_json::from_slice(&bytes[12..12 + mlen]).unwrap_or(serde_json::Value::Null);
    let mut out = Vec::new();
    let mut i = 12 + mlen;
    while i + 13 <= bytes.len() {
        let kind = bytes[i];
        let t_us = u64::from_le_bytes(bytes[i + 1..i + 9].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[i + 9..i + 13].try_into().unwrap()) as usize;
        i += 13;
        if i + len > bytes.len() {
            break;
        }
        out.push(Record { kind, t_us, data: bytes[i..i + len].to_vec() });
        i += len;
    }
    Ok((meta, out))
}

/// Decide which journals to delete: any older than `max_age` seconds, and the
/// oldest ones once the total passes `max_total` bytes. Files younger than
/// `grace` seconds are never touched (they may belong to a live session).
/// Each entry is (path, age in seconds, size in bytes).
pub fn prune_plan(files: &[(std::path::PathBuf, u64, u64)], max_age: u64, max_total: u64, grace: u64) -> Vec<std::path::PathBuf> {
    let mut newest_first: Vec<&(std::path::PathBuf, u64, u64)> = files.iter().collect();
    newest_first.sort_by_key(|f| f.1);
    let mut total = 0u64;
    let mut out = Vec::new();
    for (path, age, size) in newest_first {
        total += size;
        if *age >= grace && (*age > max_age || total > max_total) {
            out.push(path.clone());
        }
    }
    out
}

/// Apply the retention policy to a journal directory. Returns how many files went.
pub fn prune(dir: &Path, max_age_days: u64, max_total_mb: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let files: Vec<(std::path::PathBuf, u64, u64)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "prtj"))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            let age = m.modified().ok()?.elapsed().map(|d| d.as_secs()).unwrap_or(0);
            Some((e.path(), age, m.len()))
        })
        .collect();
    prune_plan(&files, max_age_days * 86_400, max_total_mb << 20, 3600)
        .iter()
        .filter(|p| {
            // A journal's keyframe index (recall.rs) is only a cache of it and goes with it.
            let _ = std::fs::remove_file(p.with_extension("prtk"));
            std::fs::remove_file(p).is_ok()
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_drops_old_and_excess_but_never_fresh() {
        let f = |n: &str, age: u64, size: u64| (std::path::PathBuf::from(n), age, size);
        let day = 86_400;
        let files = vec![f("live", 60, 900), f("recent", 2 * day, 100), f("big", 3 * day, 500), f("ancient", 40 * day, 10)];
        // Age limit only.
        assert_eq!(prune_plan(&files, 14 * day, u64::MAX, 3600), vec![std::path::PathBuf::from("ancient")]);
        // Size cap: newest are kept first; the live file is safe even though it alone exceeds the cap.
        let gone = prune_plan(&files, 14 * day, 950, 3600);
        assert_eq!(gone, vec![std::path::PathBuf::from("recent"), "big".into(), "ancient".into()]);
        assert!(prune_plan(&files, 0, 0, u64::MAX).is_empty());
    }

    #[test]
    fn roundtrip_and_truncated_tail() {
        let p = std::env::temp_dir().join(format!("proteus-journal-test-{}.prtj", std::process::id()));
        {
            let mut j = JournalWriter::create(&p, &serde_json::json!({"v": 1})).unwrap();
            j.record(K_OUTPUT, b"hello").unwrap();
            j.record(K_RESIZE, &[80, 0, 24, 0]).unwrap();
            j.record(K_OUTPUT, b"world").unwrap();
            j.flush().unwrap();
        }
        let (meta, recs) = read(&p).unwrap();
        assert_eq!(meta["v"], 1);
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].data, b"hello");
        assert_eq!(recs[2].kind, K_OUTPUT);
        // Chop the file mid-record: the reader must drop only the damaged tail.
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.truncate(bytes.len() - 3);
        std::fs::write(&p, &bytes).unwrap();
        let (_, recs) = read(&p).unwrap();
        assert_eq!(recs.len(), 2);
        let _ = std::fs::remove_file(&p);
    }
}
