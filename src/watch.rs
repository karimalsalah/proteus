//! File changes under a directory, as the operating system reports them.
//!
//! One thread sleeps in the kernel on `ReadDirectoryChangesW` (recursive,
//! overlapped) and turns each record into an [`Event`] on a channel. Nothing
//! polls: while nothing changes the thread is blocked and costs no CPU.
//!
//! The OS keeps events in a buffer of the size given here. If changes arrive
//! faster than they are read the buffer overflows and the OS drops them all;
//! that is reported as [`Change::Rescan`], which means "anything under the
//! root may have changed: look again". A consumer that honours `Rescan` never
//! holds a stale picture.
//!
//! The channel is bounded too. A consumer that stops reading does not make
//! this process grow: the thread waits, the OS buffer fills behind it, and
//! the consumer finds a `Rescan` when it returns.
//!
//!   let w = watch::Watcher::start(root)?;
//!   for ev in w.events.iter() { ... }      // ends when the watch ends
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, SendTimeoutError, Sender};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// Room for a few thousand events between two reads. The kernel keeps a
/// buffer of the same size while we are busy with the previous batch.
pub const DEFAULT_BUFFER: usize = 256 * 1024;
/// The most a network share accepts.
const NETWORK_BUFFER: usize = 64 * 1024;
/// Events held for a consumer that is busy, before the thread waits for it.
pub const DEFAULT_QUEUE: usize = 65_536;

const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Create,
    Modify,
    Delete,
    /// The entry at `from` is now at the event's path.
    Rename { from: PathBuf },
    /// The OS dropped events. Everything under the root must be looked at again.
    Rescan,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub change: Change,
    /// Relative to the watched root. Empty for `Rescan`.
    pub path: PathBuf,
    /// When this process read the event from the OS.
    pub at: Instant,
}

/// What the watch thread has done so far.
#[derive(Default)]
pub struct Counters {
    pub events: AtomicU64,
    pub overflows: AtomicU64,
    /// The Windows error that ended the watch, or 0 while it runs.
    pub ended_with: AtomicU32,
}

struct Raw(HANDLE);
// A kernel handle is a number; using it from another thread is what the API expects.
unsafe impl Send for Raw {}
unsafe impl Sync for Raw {}

pub struct Watcher {
    /// Every change, in the order the OS reported it. Disconnects when the
    /// watch ends (the watcher was dropped, or the root went away).
    pub events: Receiver<Event>,
    pub counters: Arc<Counters>,
    stop: Raw,
    stopping: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    pub fn start(root: &Path) -> io::Result<Watcher> {
        Watcher::with_limits(root, DEFAULT_BUFFER, DEFAULT_QUEUE)
    }

    /// `buffer` is the size in bytes of the OS event buffer; `queue` is how
    /// many events may wait for the consumer. Returns once the OS is
    /// recording changes: nothing made after this call returns is missed.
    pub fn with_limits(root: &Path, buffer: usize, queue: usize) -> io::Result<Watcher> {
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let dir = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if dir == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if stop.is_null() {
            let e = io::Error::last_os_error();
            unsafe { CloseHandle(dir) };
            return Err(e);
        }
        let (tx, rx) = bounded(queue.max(1));
        let (ready_tx, ready_rx) = bounded::<io::Result<()>>(1);
        let counters = Arc::new(Counters::default());
        let stopping = Arc::new(AtomicBool::new(false));
        let (dir, stop_for_thread, c) = (Raw(dir), Raw(stop), counters.clone());
        let out = Out { tx, stopping: stopping.clone() };
        let thread = std::thread::Builder::new()
            .name("proteus-watch".into())
            .spawn(move || pump(dir, stop_for_thread, buffer.max(1024), out, ready_tx, c))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Watcher { events: rx, counters, stop: Raw(stop), stopping, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                unsafe { CloseHandle(stop) };
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                unsafe { CloseHandle(stop) };
                Err(io::Error::other("the watch thread ended before it started watching"))
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        unsafe { SetEvent(self.stop.0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe { CloseHandle(self.stop.0) };
    }
}

/// The consumer's end, as the watch thread sees it.
struct Out {
    tx: Sender<Event>,
    stopping: Arc<AtomicBool>,
}

impl Out {
    /// Hand over one event, waiting while the queue is full. False when
    /// nobody will ever read it (the watcher is being dropped).
    fn send(&self, mut ev: Event) -> bool {
        loop {
            match self.tx.send_timeout(ev, Duration::from_millis(50)) {
                Ok(()) => return true,
                Err(SendTimeoutError::Disconnected(_)) => return false,
                Err(SendTimeoutError::Timeout(back)) if !self.stopping.load(Ordering::Acquire) => ev = back,
                Err(SendTimeoutError::Timeout(_)) => return false,
            }
        }
    }
}

fn pump(dir: Raw, stop: Raw, buffer: usize, tx: Out, ready: Sender<io::Result<()>>, counters: Arc<Counters>) {
    // The records hold 32-bit fields: the buffer must be aligned for them.
    let mut buf: Vec<u32> = vec![0; buffer / 4];
    let io_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    let mut ready = Some(ready);
    let mut renamed_from: Option<PathBuf> = None;
    let rescan = |counters: &Counters| {
        counters.overflows.fetch_add(1, Ordering::Relaxed);
        tx.send(Event { change: Change::Rescan, path: PathBuf::new(), at: Instant::now() })
    };
    let end = loop {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = io_event;
        let issued = unsafe {
            ReadDirectoryChangesW(dir.0, buf.as_mut_ptr().cast(), (buf.len() * 4) as u32, 1, FILTER, std::ptr::null_mut(), &mut ov, None)
        };
        if issued == 0 {
            let e = unsafe { GetLastError() };
            if e == ERROR_NOTIFY_ENUM_DIR {
                if !rescan(&counters) {
                    break 0;
                }
                continue;
            }
            if e == ERROR_INVALID_PARAMETER && buf.len() * 4 > NETWORK_BUFFER {
                buf.truncate(NETWORK_BUFFER / 4);
                continue;
            }
            if e != ERROR_IO_PENDING {
                break e;
            }
        }
        // From here the OS is recording. Tell whoever started us.
        if let Some(r) = ready.take() {
            let _ = r.send(Ok(()));
        }
        let handles = [io_event, stop.0];
        let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        let mut bytes = 0u32;
        if woke != WAIT_OBJECT_0 {
            // Asked to stop (or the wait failed). The OS still owns the
            // buffer until the cancelled read has completed.
            unsafe {
                CancelIoEx(dir.0, &ov);
                GetOverlappedResult(dir.0, &ov, &mut bytes, 1);
            }
            break 0;
        }
        if unsafe { GetOverlappedResult(dir.0, &ov, &mut bytes, 0) } == 0 {
            let e = unsafe { GetLastError() };
            if e == ERROR_NOTIFY_ENUM_DIR {
                if !rescan(&counters) {
                    break 0;
                }
                continue;
            }
            break e;
        }
        if bytes == 0 {
            // Success with nothing in the buffer is how an overflow arrives.
            if !rescan(&counters) {
                break 0;
            }
            continue;
        }
        let at = Instant::now();
        let mut sent = 0u64;
        let alive = parse(&buf, bytes as usize, |action, path| {
            let change = match action {
                FILE_ACTION_ADDED => Change::Create,
                FILE_ACTION_REMOVED => Change::Delete,
                FILE_ACTION_MODIFIED => Change::Modify,
                FILE_ACTION_RENAMED_OLD_NAME => {
                    // Its new name follows; hold the old one until then.
                    if let Some(orphan) = renamed_from.replace(path) {
                        sent += 1;
                        return tx.send(Event { change: Change::Delete, path: orphan, at });
                    }
                    return true;
                }
                FILE_ACTION_RENAMED_NEW_NAME => match renamed_from.take() {
                    Some(from) => Change::Rename { from },
                    None => Change::Create,
                },
                _ => Change::Modify,
            };
            sent += 1;
            tx.send(Event { change, path, at })
        });
        counters.events.fetch_add(sent, Ordering::Relaxed);
        if !alive {
            break 0;
        }
    };
    if let Some(r) = ready.take() {
        let _ = r.send(Err(io::Error::from_raw_os_error(end as i32)));
    }
    counters.ended_with.store(end, Ordering::Relaxed);
    unsafe {
        CloseHandle(io_event);
        CloseHandle(dir.0);
    }
}

/// Walk the FILE_NOTIFY_INFORMATION records in the first `bytes` of `buf`.
/// Record layout: u32 next offset | u32 action | u32 name bytes | UTF-16 name.
fn parse(buf: &[u32], bytes: usize, mut each: impl FnMut(u32, PathBuf) -> bool) -> bool {
    let bytes = bytes.min(buf.len() * 4);
    let mut at = 0usize;
    while at + 12 <= bytes && at % 4 == 0 {
        let (next, action, name_bytes) = (buf[at / 4] as usize, buf[at / 4 + 1], buf[at / 4 + 2] as usize);
        if at + 12 + name_bytes > bytes {
            break;
        }
        let name: &[u16] = unsafe { std::slice::from_raw_parts((buf.as_ptr() as *const u8).add(at + 12).cast::<u16>(), name_bytes / 2) };
        if !each(action, PathBuf::from(OsString::from_wide(name))) {
            return false;
        }
        if next == 0 {
            break;
        }
        at += next;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("proteus-watch-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Collect events until `done` says so or the time is up.
    fn collect(w: &Watcher, wait: Duration, mut done: impl FnMut(&[Event]) -> bool) -> Vec<Event> {
        let deadline = Instant::now() + wait;
        let mut got = Vec::new();
        while !done(&got) {
            match w.events.recv_deadline(deadline) {
                Ok(e) => got.push(e),
                Err(_) => break,
            }
        }
        got
    }

    #[test]
    fn records_parse_from_a_hand_built_buffer() {
        // Two records: MODIFIED "ab", then ADDED "c". Offsets are 4-aligned.
        let mut bytes: Vec<u8> = Vec::new();
        for (next, action, name) in [(16u32, 3u32, "ab"), (0, 1, "c")] {
            let wide: Vec<u16> = name.encode_utf16().collect();
            bytes.extend(next.to_le_bytes());
            bytes.extend(action.to_le_bytes());
            bytes.extend(((wide.len() * 2) as u32).to_le_bytes());
            for u in wide {
                bytes.extend(u.to_le_bytes());
            }
            while bytes.len() % 4 != 0 {
                bytes.push(0);
            }
        }
        let buf: Vec<u32> = bytes.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        let mut seen = Vec::new();
        assert!(parse(&buf, bytes.len(), |a, p| {
            seen.push((a, p));
            true
        }));
        assert_eq!(seen, vec![(3, PathBuf::from("ab")), (1, PathBuf::from("c"))]);
        // A record whose name runs past the end is not read.
        let mut none = 0;
        parse(&buf, 14, |_, _| {
            none += 1;
            true
        });
        assert_eq!(none, 0);
    }

    #[test]
    fn sees_create_modify_rename_and_delete() {
        let root = temp("crud");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let w = Watcher::start(&root).unwrap();
        let has = |got: &[Event], change: &Change, path: &str| got.iter().any(|e| &e.change == change && e.path == Path::new(path));

        std::fs::write(root.join("sub").join("a.txt"), b"one").unwrap();
        let got = collect(&w, Duration::from_secs(5), |g| has(g, &Change::Create, "sub\\a.txt"));
        assert!(has(&got, &Change::Create, "sub\\a.txt"), "no create event: {got:?}");

        std::fs::write(root.join("sub").join("a.txt"), b"one two three").unwrap();
        let got = collect(&w, Duration::from_secs(5), |g| has(g, &Change::Modify, "sub\\a.txt"));
        assert!(has(&got, &Change::Modify, "sub\\a.txt"), "no modify event: {got:?}");

        std::fs::rename(root.join("sub").join("a.txt"), root.join("sub").join("b.txt")).unwrap();
        let renamed = Change::Rename { from: PathBuf::from("sub\\a.txt") };
        let got = collect(&w, Duration::from_secs(5), |g| has(g, &renamed, "sub\\b.txt"));
        assert!(has(&got, &renamed, "sub\\b.txt"), "no rename event: {got:?}");

        std::fs::remove_file(root.join("sub").join("b.txt")).unwrap();
        let got = collect(&w, Duration::from_secs(5), |g| has(g, &Change::Delete, "sub\\b.txt"));
        assert!(has(&got, &Change::Delete, "sub\\b.txt"), "no delete event: {got:?}");

        assert_eq!(w.counters.overflows.load(Ordering::Relaxed), 0);
        drop(w);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn overflow_is_reported_and_the_watch_goes_on() {
        // Nobody reads while 10,000 files are written: 8 events wait in the
        // queue, the OS buffer (4 KB) fills behind them and must overflow.
        // The OS has to say so, and keep reporting afterwards.
        let root = temp("flood");
        let w = Watcher::with_limits(&root, 4096, 8).unwrap();
        for i in 0..10_000 {
            std::fs::write(root.join(format!("f{i:05}.txt")), b"x").unwrap();
        }
        let got = collect(&w, Duration::from_secs(20), |g| g.iter().any(|e| e.change == Change::Rescan));
        assert!(got.iter().any(|e| e.change == Change::Rescan), "10,000 unread changes and no overflow reported ({} events)", got.len());
        assert!(got.len() < 10_000, "the queue was meant to hold 8 events, not {}", got.len());
        assert!(w.counters.overflows.load(Ordering::Relaxed) >= 1);

        // Let the storm drain, then prove a later change still arrives.
        while w.events.recv_timeout(Duration::from_millis(300)).is_ok() {}
        std::fs::write(root.join("after.txt"), b"still here").unwrap();
        let got = collect(&w, Duration::from_secs(5), |g| g.iter().any(|e| e.path == Path::new("after.txt")));
        assert!(got.iter().any(|e| e.path == Path::new("after.txt")), "nothing reported after the overflow: {got:?}");
        assert_eq!(w.counters.ended_with.load(Ordering::Relaxed), 0);
        drop(w);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ten_thousand_changes_with_the_default_buffer_lose_nothing_silently() {
        // Either every file is reported, or a Rescan says events were dropped.
        let root = temp("many");
        let w = Watcher::start(&root).unwrap();
        for i in 0..10_000 {
            std::fs::write(root.join(format!("g{i:05}.txt")), b"y").unwrap();
        }
        let mut names = std::collections::HashSet::new();
        let mut rescans = 0;
        while let Ok(e) = w.events.recv_timeout(Duration::from_millis(500)) {
            match e.change {
                Change::Rescan => rescans += 1,
                _ => {
                    names.insert(e.path);
                }
            }
        }
        assert!(rescans > 0 || names.len() == 10_000, "{} of 10,000 files reported and no rescan", names.len());
        drop(w);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stops_promptly_and_reports_a_missing_root() {
        let root = temp("stop");
        let t = Instant::now();
        drop(Watcher::start(&root).unwrap());
        assert!(t.elapsed() < Duration::from_secs(2), "stopping took {:?}", t.elapsed());
        // Also with the thread stuck on a full queue that nobody reads.
        let w = Watcher::with_limits(&root, 4096, 1).unwrap();
        for i in 0..50 {
            std::fs::write(root.join(format!("s{i}")), b"x").unwrap();
        }
        std::thread::sleep(Duration::from_millis(200));
        let t = Instant::now();
        drop(w);
        assert!(t.elapsed() < Duration::from_secs(2), "stopping with a full queue took {:?}", t.elapsed());
        std::fs::remove_dir_all(&root).unwrap();
        assert!(Watcher::start(&root).is_err());
    }
}
