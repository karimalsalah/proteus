//! The sense behind `procs.rs`: Windows itself reports every process that
//! starts or ends under a session's shell. No hooks in the shell, no polling.
//!
//! Each session's shell is placed in a job object. A job is inherited by
//! everything the shell starts, and the kernel posts a message to an I/O
//! completion port whenever a process joins or leaves it. One thread for the
//! whole program sleeps on that port; with nothing starting or ending it
//! never wakes, so an idle session costs nothing.
//!
//! When a process joins, the sensor opens it and keeps the handle until it
//! has left. While the handle is held Windows cannot give the pid to anything
//! else, and the exit code and CPU time stay readable after the process is
//! gone. A process that is over before it can be opened is still recorded,
//! as seen with details unknown. What is opened is checked to be in the
//! session's job before anything is read from it, so a pid that has passed
//! to a process elsewhere is never mistaken for the one announced. (One
//! that has passed to another process of the same session would be; that
//! needs the sensor to be later than a whole process's life.)
//!
//! The job allows a child to leave it on request (`CREATE_BREAKAWAY_FROM_JOB`,
//! which the daemon launch uses) and is never set to kill its processes, so
//! nothing lives or dies differently because it is being watched.
//!
//! The shell cannot be created inside its job (the pseudo-terminal library
//! creates it), so it joins a moment after it exists: see `Watch::adopt` for
//! what keeps that moment short, and what is done about a shell that used it.
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::io::RawHandle;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation, ProcessCommandLineInformation};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, FILETIME, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
};
use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows_sys::Win32::Storage::FileSystem::{GetLogicalDrives, QueryDosDeviceW};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetCurrentThreadId, GetExitCodeProcess, GetProcessId, GetProcessTimes, GetThreadPriority, OpenProcess,
    QueryFullProcessImageNameW, SetThreadPriority, WaitForSingleObject, INFINITE, PROCESS_NAME_NATIVE, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus, OVERLAPPED};

use crate::journal::{self, JournalWriter};
use crate::paths;
use crate::procs::{Exit, ProcEvent, Start};
use crate::session::SessionEvent;

// Messages a job posts to its completion port (winnt.h).
const MSG_ACTIVE_PROCESS_ZERO: u32 = 4;
const MSG_NEW_PROCESS: u32 = 6;
const MSG_EXIT_PROCESS: u32 = 7;
const MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;

/// Completion key of the sensor's own requests; job keys count up from 1.
const CONTROL: usize = usize::MAX;
const OP_ATTACH: u32 = 1;
const OP_FINISH: u32 = 2;
const OP_DROP: u32 = 3;

/// Events held for a window that has stopped reading before new ones are dropped.
const OUTBOX_MAX: usize = 1 << 16;
/// How long an exit may wait for the kernel to finish with the process
/// before it is recorded with whatever is known.
// How long a session's end waits for its shell's own exit. 2 s was too short on
// a fully loaded machine (the full suite failed once with ten agents building).
const EXIT_PATIENCE: Duration = Duration::from_secs(5);

/// What the sensor has done since this program started. Counters, read by
/// tests and written to the log when a session ends.
#[derive(Default)]
pub struct Stats {
    /// Processes reported as joining / leaving.
    pub started: AtomicU64,
    pub exited: AtomicU64,
    /// Gone (or its pid already reused) before it could be opened.
    pub unopened: AtomicU64,
    /// Opened, but the command line / image path could no longer be read.
    pub no_cmd: AtomicU64,
    pub no_image: AtomicU64,
    /// Exits where the process was not yet fully gone when the message came,
    /// and the longest the sensor then waited for it.
    pub exit_waited: AtomicU64,
    pub exit_wait_max_us: AtomicU64,
    /// Exits whose code could not be read at all.
    pub exit_unknown: AtomicU64,
    /// Exits found by looking at the held handle, with no message for them.
    pub exit_recovered: AtomicU64,
    /// Events dropped because a window stopped reading.
    pub dropped: AtomicU64,
    /// Processes a shell had started before it was in its job, found and put
    /// in the job after it (`gather_strays`).
    pub gathered: AtomicU64,
    /// From the newest shell's creation time (stamped before Windows has
    /// finished creating it) to it being inside its job...
    pub join_lag_last_us: AtomicU64,
    pub join_lag_max_us: AtomicU64,
    /// ...and how much of that was the call that puts it there.
    pub join_call_last_us: AtomicU64,
    pub join_call_max_us: AtomicU64,
}

pub static STATS: Stats = Stats {
    started: AtomicU64::new(0),
    exited: AtomicU64::new(0),
    unopened: AtomicU64::new(0),
    no_cmd: AtomicU64::new(0),
    no_image: AtomicU64::new(0),
    exit_waited: AtomicU64::new(0),
    exit_wait_max_us: AtomicU64::new(0),
    exit_unknown: AtomicU64::new(0),
    exit_recovered: AtomicU64::new(0),
    dropped: AtomicU64::new(0),
    gathered: AtomicU64::new(0),
    join_lag_last_us: AtomicU64::new(0),
    join_lag_max_us: AtomicU64::new(0),
    join_call_last_us: AtomicU64::new(0),
    join_call_max_us: AtomicU64::new(0),
};

pub fn stats() -> serde_json::Value {
    let s = &STATS;
    serde_json::json!({
        "started": s.started.load(Relaxed), "exited": s.exited.load(Relaxed), "unopened": s.unopened.load(Relaxed),
        "no_cmd": s.no_cmd.load(Relaxed), "no_image": s.no_image.load(Relaxed), "exit_waited": s.exit_waited.load(Relaxed),
        "exit_wait_max_us": s.exit_wait_max_us.load(Relaxed), "exit_unknown": s.exit_unknown.load(Relaxed),
        "exit_recovered": s.exit_recovered.load(Relaxed), "dropped": s.dropped.load(Relaxed), "gathered": s.gathered.load(Relaxed),
        "join_lag_last_us": s.join_lag_last_us.load(Relaxed), "join_lag_max_us": s.join_lag_max_us.load(Relaxed),
        "join_call_last_us": s.join_call_last_us.load(Relaxed), "join_call_max_us": s.join_call_max_us.load(Relaxed),
    })
}

/// A kernel handle this module owns.
struct Owned(HANDLE);
// A handle is a number naming a kernel object; the calls made with it here are thread-safe.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn filetime_us(t: &FILETIME) -> Option<u64> {
    // 100 ns ticks since 1601; the Unix epoch is 11,644,473,600 s later.
    let ticks = (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64;
    (ticks > 116_444_736_000_000_000).then(|| (ticks - 116_444_736_000_000_000) / 10)
}

/// Now, in microseconds since the Unix epoch, on the clock process creation
/// and exit times are stamped with.
pub fn now_us() -> u64 {
    let mut t = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    unsafe { GetSystemTimePreciseAsFileTime(&mut t) };
    filetime_us(&t).unwrap_or(0)
}

struct Times {
    created_us: Option<u64>,
    exited_us: Option<u64>,
    kernel_us: u64,
    user_us: u64,
}

fn times(h: HANDLE) -> Option<Times> {
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    if unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } == 0 {
        return None;
    }
    let span = |t: &FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) / 10;
    Some(Times { created_us: filetime_us(&c), exited_us: filetime_us(&e), kernel_us: span(&k), user_us: span(&u) })
}

/// The head of `PROCESS_BASIC_INFORMATION`.
#[repr(C)]
struct BasicInfo {
    exit_status: i32,
    peb: usize,
    affinity: usize,
    base_priority: i32,
    pid: usize,
    parent_pid: usize,
}

fn parent_of(h: HANDLE) -> Option<u32> {
    let mut b: BasicInfo = unsafe { zeroed() };
    let st = unsafe { NtQueryInformationProcess(h, ProcessBasicInformation, &mut b as *mut _ as *mut c_void, size_of::<BasicInfo>() as u32, null_mut()) };
    (st >= 0).then_some(b.parent_pid as u32)
}

/// The pid of the process that started this one.
pub fn parent_pid() -> Option<u32> {
    parent_of(unsafe { GetCurrentProcess() })
}

/// The command line as the process received it. It lives in the process's
/// own memory, so it can only be read while the process is alive.
fn command_line(h: HANDLE) -> Option<String> {
    // u64 storage keeps the UNICODE_STRING header aligned.
    let mut buf = vec![0u64; 256];
    for _ in 0..2 {
        let mut need = 0u32;
        let bytes = (buf.len() * 8) as u32;
        let st = unsafe { NtQueryInformationProcess(h, ProcessCommandLineInformation, buf.as_mut_ptr() as *mut c_void, bytes, &mut need) };
        if st >= 0 {
            let us = unsafe { &*(buf.as_ptr() as *const UNICODE_STRING) };
            let (base, at) = (buf.as_ptr() as usize, us.Buffer as usize);
            let len = us.Length as usize;
            // The text must lie inside what was just filled in.
            if at < base || at + len > base + bytes as usize {
                return None;
            }
            let text = unsafe { std::slice::from_raw_parts(us.Buffer, len / 2) };
            return Some(String::from_utf16_lossy(text));
        }
        if need as usize <= buf.len() * 8 {
            return None;
        }
        buf = vec![0u64; need as usize / 8 + 2];
    }
    None
}

fn image_name(h: HANDLE, format: u32) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    loop {
        let mut len = buf.len() as u32;
        if unsafe { QueryFullProcessImageNameW(h, format, buf.as_mut_ptr(), &mut len) } != 0 {
            return Some(String::from_utf16_lossy(&buf[..len as usize]));
        }
        if buf.len() >= 1 << 15 {
            return None;
        }
        buf = vec![0u16; 1 << 15];
    }
}

/// `\Device\HarddiskVolume3\dir\x.exe` as `C:\dir\x.exe`. Returned as it is
/// when no drive letter stands for the device (a network share, say).
fn drive_path(native: &str) -> String {
    let drives = unsafe { GetLogicalDrives() };
    for i in (0..26u8).filter(|i| drives & (1 << i) != 0) {
        let letter = [(b'A' + i) as u16, b':' as u16, 0];
        let mut device = [0u16; 512];
        if unsafe { QueryDosDeviceW(letter.as_ptr(), device.as_mut_ptr(), device.len() as u32) } == 0 {
            continue;
        }
        let device = String::from_utf16_lossy(&device[..device.iter().position(|&c| c == 0).unwrap_or(0)]);
        if let (Some(head), Some(rest)) = (native.get(..device.len()), native.get(device.len()..)) {
            if !device.is_empty() && head.eq_ignore_ascii_case(&device) && rest.starts_with('\\') {
                return format!("{}:{rest}", (b'A' + i) as char);
            }
        }
    }
    native.to_string()
}

/// Full path of the executable. The ordinary form can only be asked of a
/// process that is still alive; the kernel's own name for the file outlives
/// the process, and is translated back to a drive letter.
fn image_path(h: HANDLE) -> Option<String> {
    image_name(h, 0).or_else(|| image_name(h, PROCESS_NAME_NATIVE).map(|n| drive_path(&n)))
}

fn signaled(h: HANDLE, wait_ms: u32) -> bool {
    unsafe { WaitForSingleObject(h, wait_ms) == WAIT_OBJECT_0 }
}

/// Can the exit of this process be read in full yet: a real exit code and an exit time?
fn settled(h: HANDLE) -> bool {
    let mut code = 0u32;
    let coded = unsafe { GetExitCodeProcess(h, &mut code) } != 0 && code != 259;
    (coded && times(h).is_some_and(|t| t.exited_us.is_some())) || signaled(h, 0)
}

/// A process the sensor is holding open until its exit is recorded.
struct Held {
    handle: Option<Owned>,
    start_us: Option<u64>,
}

/// An exit reported before the kernel had finished with the process.
struct Closing {
    key: usize,
    pid: u32,
    handle: Owned,
    abnormal: bool,
    since: Instant,
}

/// What `finish` hands back: events the window had not yet taken.
struct Rest {
    tx: Option<Sender<SessionEvent>>,
    events: Vec<ProcEvent>,
}

struct Sess {
    job: Owned,
    done: Sender<Rest>,
    tx: Option<Sender<SessionEvent>>,
    journal: Option<Arc<Mutex<JournalWriter>>>,
    /// Events not yet handed to the window, oldest first.
    outbox: VecDeque<ProcEvent>,
    /// The oldest of them is already in the journal.
    head_journaled: bool,
    /// The window's end of the channel is gone.
    deaf: bool,
    held: HashMap<u32, Held>,
    join_lag_us: Option<u64>,
    started: u64,
    exited: u64,
}

#[derive(Default)]
struct State {
    next_key: usize,
    sessions: HashMap<usize, Sess>,
}

struct Sensor {
    port: Owned,
    state: Mutex<State>,
}

fn sensor() -> Option<&'static Sensor> {
    static SENSOR: OnceLock<Option<&'static Sensor>> = OnceLock::new();
    *SENSOR.get_or_init(|| {
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
        if port.is_null() {
            paths::log(&format!("procs: no completion port (error {}); processes will not be sensed", unsafe { GetLastError() }));
            return None;
        }
        let s: &'static Sensor = Box::leak(Box::new(Sensor { port: Owned(port), state: Mutex::new(State::default()) }));
        match std::thread::Builder::new().name("procs".into()).spawn(move || s.run()) {
            Ok(_) => Some(s),
            Err(e) => {
                paths::log(&format!("procs: no sensor thread ({e}); processes will not be sensed"));
                None
            }
        }
    })
}

impl Sess {
    /// Queue an event for the window and hand over as much as it will take.
    /// Never blocks: the window's channel is bounded so that a flood of
    /// output slows the shell, and the sensor must not be slowed with it.
    fn deliver(&mut self, ev: ProcEvent) {
        match &ev {
            ProcEvent::Start(_) => self.started += 1,
            ProcEvent::Exit(_) => self.exited += 1,
        }
        if self.deaf {
            return;
        }
        if self.outbox.len() >= OUTBOX_MAX {
            STATS.dropped.fetch_add(1, Relaxed);
            return;
        }
        self.outbox.push_back(ev);
        self.flush();
    }

    /// Returns true while events are still waiting on a full channel.
    fn flush(&mut self) -> bool {
        let Some(tx) = &self.tx else { return false };
        while let Some(ev) = self.outbox.pop_front() {
            if !self.head_journaled {
                self.head_journaled = true;
                if let Some(j) = &self.journal {
                    if let Ok(mut j) = j.lock() {
                        let _ = j.record(journal::K_PROC, &ev.to_bytes());
                    }
                }
            }
            match tx.try_send(SessionEvent::Proc(Box::new(ev))) {
                Ok(()) => self.head_journaled = false,
                Err(TrySendError::Full(SessionEvent::Proc(ev))) => {
                    self.outbox.push_front(*ev);
                    return true;
                }
                Err(_) => {
                    self.deaf = true;
                    self.outbox.clear();
                    return false;
                }
            }
        }
        false
    }
}

impl Sensor {
    fn post(&self, op: u32, key: usize) -> bool {
        unsafe { PostQueuedCompletionStatus(self.port.0, op, CONTROL, key as *const OVERLAPPED) != 0 }
    }

    fn with<R>(&self, key: usize, f: impl FnOnce(&mut Sess) -> R) -> Option<R> {
        self.state.lock().ok()?.sessions.get_mut(&key).map(f)
    }

    fn run(&self) {
        // The whole job of this thread is to reach a new process while it can
        // still be asked who it is. It does a few system calls per process
        // and sleeps otherwise, so running it ahead of ordinary work costs
        // nothing measurable and is what keeps short-lived processes visible
        // when every core is busy.
        unsafe { SetThreadPriority(GetCurrentThread(), sensor_priority()) };
        let mut closing: Vec<Closing> = Vec::new();
        // Sessions whose window is not keeping up.
        let mut behind: Vec<usize> = Vec::new();
        loop {
            // With nothing pending this waits forever: no timer, no wake-ups.
            let wait = if closing.is_empty() && behind.is_empty() { INFINITE } else { 1 };
            let (mut msg, mut key, mut ov) = (0u32, 0usize, null_mut::<OVERLAPPED>());
            let got = unsafe { GetQueuedCompletionStatus(self.port.0, &mut msg, &mut key, &mut ov, wait) } != 0;
            if got || !ov.is_null() {
                let arg = ov as usize;
                if key == CONTROL {
                    self.control(msg, arg, &mut closing, &mut behind);
                } else {
                    match msg {
                        MSG_NEW_PROCESS => self.on_new(key, arg as u32, &mut behind),
                        MSG_EXIT_PROCESS | MSG_ABNORMAL_EXIT_PROCESS => {
                            self.on_exit(key, arg as u32, msg == MSG_ABNORMAL_EXIT_PROCESS, &mut closing, &mut behind)
                        }
                        MSG_ACTIVE_PROCESS_ZERO => self.settle(key, &mut behind),
                        _ => {}
                    }
                }
            } else if unsafe { GetLastError() } != WAIT_TIMEOUT {
                paths::log(&format!("procs: the completion port failed (error {}); sensing stops", unsafe { GetLastError() }));
                return;
            }
            if !closing.is_empty() {
                let (ready, waiting): (Vec<Closing>, Vec<Closing>) =
                    closing.drain(..).partition(|c| settled(c.handle.0) || c.since.elapsed() > EXIT_PATIENCE);
                closing = waiting;
                for c in ready {
                    self.record_exit(c.key, c.pid, Some(c.handle), c.abnormal, Some(c.since), &mut behind);
                }
            }
            if !behind.is_empty() {
                let mut st = match self.state.lock() {
                    Ok(st) => st,
                    Err(_) => return,
                };
                behind.retain(|k| st.sessions.get_mut(k).is_some_and(|s| s.flush()));
            }
        }
    }

    fn give(&self, key: usize, ev: ProcEvent, behind: &mut Vec<usize>) {
        let waiting = self.with(key, |s| {
            s.deliver(ev);
            s.tx.is_some() && !s.outbox.is_empty()
        });
        if waiting == Some(true) && !behind.contains(&key) {
            behind.push(key);
        }
    }

    fn on_new(&self, key: usize, pid: u32, behind: &mut Vec<usize>) {
        // Sessions are only ever removed on this thread, so the job handle
        // read here stays valid for the rest of this call.
        let Some(job) = self.with(key, |s| s.job.0 as usize) else { return };
        let mut start = Start { pid, ppid: None, image: None, cmd: None, start_us: None, seen_us: 0 };
        let mut handle = None;
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
        if !h.is_null() {
            let h = Owned(h);
            // The pid in the message may already belong to someone else. Only
            // a process inside this session's job is the one that was announced.
            let mut inside = 0;
            if unsafe { IsProcessInJob(h.0, job as HANDLE, &mut inside) } != 0 && inside != 0 {
                // First, while it may still be alive: the only detail that dies with it.
                start.cmd = command_line(h.0);
                start.image = image_path(h.0);
                start.ppid = parent_of(h.0);
                start.start_us = times(h.0).and_then(|t| t.created_us);
                if start.cmd.is_none() {
                    STATS.no_cmd.fetch_add(1, Relaxed);
                }
                if start.image.is_none() {
                    STATS.no_image.fetch_add(1, Relaxed);
                }
                handle = Some(h);
            }
        }
        if handle.is_none() {
            STATS.unopened.fetch_add(1, Relaxed);
        }
        start.seen_us = now_us();
        let fresh = self.with(key, |s| {
            if let Some(old) = s.held.get(&pid) {
                if old.start_us.is_some() && old.start_us == start.start_us {
                    return false; // the same process, announced twice
                }
            }
            s.held.insert(pid, Held { handle, start_us: start.start_us });
            true
        });
        if fresh == Some(true) {
            STATS.started.fetch_add(1, Relaxed);
            self.give(key, ProcEvent::Start(start), behind);
        }
    }

    fn on_exit(&self, key: usize, pid: u32, abnormal: bool, closing: &mut Vec<Closing>, behind: &mut Vec<usize>) {
        let Some(held) = self.with(key, |s| s.held.remove(&pid)) else { return };
        match held.and_then(|h| h.handle) {
            // The message is posted while the kernel is still taking the
            // process apart (the handle is signaled about a millisecond
            // later), but the exit code and exit time are normally already
            // set. Only when they are not is the exit put aside until they
            // are, so that one slow exit never holds up the next arrival.
            Some(h) if !settled(h.0) => closing.push(Closing { key, pid, handle: h, abnormal, since: Instant::now() }),
            h => self.record_exit(key, pid, h, abnormal, None, behind),
        }
    }

    fn record_exit(&self, key: usize, pid: u32, handle: Option<Owned>, abnormal: bool, waited: Option<Instant>, behind: &mut Vec<usize>) {
        if let Some(since) = waited {
            STATS.exit_waited.fetch_add(1, Relaxed);
            STATS.exit_wait_max_us.fetch_max(since.elapsed().as_micros() as u64, Relaxed);
        }
        let mut exit = Exit { pid, code: None, end_us: None, user_us: None, kernel_us: None, abnormal, seen_us: 0 };
        if let Some(h) = &handle {
            let mut code = 0u32;
            // 259 means "still running" unless the process is known to be gone.
            if unsafe { GetExitCodeProcess(h.0, &mut code) } != 0 && (code != 259 || signaled(h.0, 0)) {
                exit.code = Some(code);
            }
            if let Some(t) = times(h.0) {
                exit.end_us = t.exited_us;
                exit.user_us = Some(t.user_us);
                exit.kernel_us = Some(t.kernel_us);
            }
        }
        if exit.code.is_none() {
            STATS.exit_unknown.fetch_add(1, Relaxed);
        }
        STATS.exited.fetch_add(1, Relaxed);
        exit.seen_us = now_us();
        self.give(key, ProcEvent::Exit(exit), behind);
    }

    /// The job says it is empty: anything still held ended without a message.
    fn settle(&self, key: usize, behind: &mut Vec<usize>) {
        let left: Vec<(u32, Held)> = self.with(key, |s| s.held.drain().collect()).unwrap_or_default();
        for (pid, held) in left {
            STATS.exit_recovered.fetch_add(1, Relaxed);
            self.record_exit(key, pid, held.handle, false, None, behind);
        }
    }

    fn control(&self, op: u32, key: usize, closing: &mut Vec<Closing>, behind: &mut Vec<usize>) {
        if op == OP_ATTACH {
            if self.with(key, |s| s.flush()) == Some(true) && !behind.contains(&key) {
                behind.push(key);
            }
            return;
        }
        // The session is over. Record what can still be learned, then let go
        // of every handle so nothing is kept alive on its account.
        let (mine, others): (Vec<Closing>, Vec<Closing>) = closing.drain(..).partition(|c| c.key == key);
        *closing = others;
        for c in mine {
            self.record_exit(key, c.pid, Some(c.handle), c.abnormal, Some(c.since), behind);
        }
        let held: Vec<(u32, Held)> = self.with(key, |s| s.held.drain().collect()).unwrap_or_default();
        for (pid, held) in held {
            // Ended with no message: still worth its exit code. Anything
            // else is simply still running, and stays so in the ledger.
            if held.handle.as_ref().is_some_and(|h| signaled(h.0, 0)) {
                STATS.exit_recovered.fetch_add(1, Relaxed);
                self.record_exit(key, pid, held.handle, false, None, behind);
            }
        }
        behind.retain(|k| *k != key);
        let Some(mut s) = self.state.lock().ok().and_then(|mut st| st.sessions.remove(&key)) else { return };
        // Stop the job posting here. Its processes are not touched: closing
        // the handle below leaves them running, as nothing ever asked otherwise.
        let none = JOBOBJECT_ASSOCIATE_COMPLETION_PORT { CompletionKey: null_mut(), CompletionPort: null_mut() };
        unsafe { SetInformationJobObject(s.job.0, JobObjectAssociateCompletionPortInformation, &none as *const _ as *const c_void, size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32) };
        s.flush();
        paths::log(&format!(
            "procs: session over: {} started, {} exited, joined its job {} us after creation; sensor totals {}",
            s.started,
            s.exited,
            s.join_lag_us.map_or("?".into(), |l| l.to_string()),
            stats()
        ));
        if op == OP_FINISH && !s.deaf {
            // What the window has not taken goes in the journal now, and to
            // the window from the thread that is ending the session, which
            // (unlike this one) may wait for it.
            let mut events: Vec<ProcEvent> = s.outbox.drain(..).collect();
            if let Some(j) = &s.journal {
                if let Ok(mut j) = j.lock() {
                    for ev in events.iter().skip(s.head_journaled as usize) {
                        let _ = j.record(journal::K_PROC, &ev.to_bytes());
                    }
                }
            }
            if s.tx.is_none() {
                events.clear();
            }
            let _ = s.done.try_send(Rest { tx: s.tx.take(), events });
        }
    }
}

fn sensor_priority() -> i32 {
    // Tests compare priorities to justify the choice; the product has one.
    #[cfg(test)]
    if let Some(p) = std::env::var("PROTEUS_TEST_PROCS_PRIORITY").ok().and_then(|p| p.parse().ok()) {
        return p;
    }
    THREAD_PRIORITY_TIME_CRITICAL
}

#[cfg(test)]
thread_local! {
    /// Tests hold a shell out of its job for this long, on the thread that
    /// starts the session, to measure how late the join may be.
    pub(crate) static JOIN_LATE_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One session's place in the sensor. Made before the shell exists, so that
/// joining the job is the only thing left to do once it does.
pub struct Watch {
    key: usize,
    job: usize,
    done: Receiver<Rest>,
    finished: bool,
    /// The thread that is about to create the shell, and the priority it
    /// had before `prepare` raised it (given back by `adopt`, or on drop).
    hurry: std::cell::Cell<Option<(u32, i32)>>,
    /// The shell itself, to wait on when the session ends (`finish`).
    shell: std::cell::RefCell<Option<Owned>>,
}

/// Run the calling thread ahead of ordinary work, and say what to give back.
fn hurry() -> Option<(u32, i32)> {
    let me = unsafe { GetCurrentThread() };
    let old = unsafe { GetThreadPriority(me) };
    // 0x7FFFFFFF: the priority could not be read, so it could not be restored.
    if old == 0x7FFF_FFFF || unsafe { SetThreadPriority(me, THREAD_PRIORITY_TIME_CRITICAL) } == 0 {
        return None;
    }
    Some((unsafe { GetCurrentThreadId() }, old))
}

/// Every process in the system as (pid, parent pid). One snapshot, taken
/// once when a session starts; nothing here is polled.
fn parent_links() -> Vec<(u32, u32)> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let snap = Owned(snap);
    let mut e: PROCESSENTRY32W = unsafe { zeroed() };
    e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut out = Vec::with_capacity(512);
    let mut more = unsafe { Process32FirstW(snap.0, &mut e) } != 0;
    while more {
        out.push((e.th32ProcessID, e.th32ParentProcessID));
        more = unsafe { Process32NextW(snap.0, &mut e) } != 0;
    }
    out
}

/// The shell joined its job at `joined_us`. Whatever it had started before
/// that is outside the job, and so is everything those have started since:
/// find them by their parents and put them in the job too. The job then
/// announces each as a new process, so the sensor records them like any
/// other. Returns how many were gathered.
///
/// A process counts as a stray only if it was created while its parent was
/// alive and not yet in the job: a child created after its parent joined
/// was born in the job, or asked to be let out of it (`CREATE_BREAKAWAY_FROM_JOB`)
/// and is left alone. What cannot be gathered: a stray that has already
/// ended, and what it started if it is no longer listed.
fn gather_strays(job: HANDLE, shell: HANDLE, joined_us: u64) -> u32 {
    let shell_pid = unsafe { GetProcessId(shell) };
    let Some(shell_created) = times(shell).and_then(|t| t.created_us) else { return 0 };
    if shell_pid == 0 {
        return 0;
    }
    // The shell and each stray found: (pid, created, in the job since).
    let mut family: Vec<(u32, u64, u64)> = vec![(shell_pid, shell_created, joined_us)];
    let mut gathered = 0;
    // A stray keeps starting processes outside the job until it is in it,
    // so look again until a look finds nothing (a few times at most).
    for _ in 0..6 {
        let links = parent_links();
        let known = family.len();
        let mut k = 0;
        while k < family.len() {
            let (parent, parent_created, parent_joined) = family[k];
            k += 1;
            for &(pid, _) in links.iter().filter(|l| l.1 == parent && l.0 != parent) {
                if family.iter().any(|f| f.0 == pid) {
                    continue;
                }
                let h = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
                if h.is_null() {
                    continue;
                }
                let h = Owned(h);
                let Some(created) = times(h.0).and_then(|t| t.created_us) else { continue };
                // Older than its "parent": the pid it names once belonged to
                // something else. Younger than its parent's join: not a stray.
                if created < parent_created || created > parent_joined {
                    continue;
                }
                let mut inside = 0;
                if unsafe { IsProcessInJob(h.0, job, &mut inside) } == 0 || inside != 0 {
                    continue;
                }
                let joined = unsafe { AssignProcessToJobObject(job, h.0) } != 0;
                // One that has ended cannot join, but what it started may be alive.
                family.push((pid, created, now_us()));
                gathered += joined as u32;
            }
        }
        if family.len() == known {
            break;
        }
    }
    gathered
}

/// Create the job for a session about to start. `None` (logged) if the OS
/// refuses; the session then simply runs unobserved.
pub fn prepare() -> Option<Watch> {
    let s = sensor()?;
    let fail = |what: &str| {
        paths::log(&format!("procs: {what} failed (error {}); this session's processes will not be sensed", unsafe { GetLastError() }));
        None
    };
    let job = Owned(unsafe { CreateJobObjectW(null(), null()) });
    if job.0.is_null() {
        return fail("CreateJobObject");
    }
    // Breakaway on request stays allowed. No kill-on-close, no limits: the
    // job observes and does nothing else.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let set = unsafe {
        SetInformationJobObject(job.0, JobObjectExtendedLimitInformation, &limits as *const _ as *const c_void, size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32)
    };
    if set == 0 {
        return fail("allowing breakaway from the job");
    }
    let mut st = s.state.lock().ok()?;
    st.next_key += 1;
    let key = st.next_key;
    let port = JOBOBJECT_ASSOCIATE_COMPLETION_PORT { CompletionKey: key as *mut c_void, CompletionPort: s.port.0 };
    let set = unsafe {
        SetInformationJobObject(job.0, JobObjectAssociateCompletionPortInformation, &port as *const _ as *const c_void, size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32)
    };
    if set == 0 {
        return fail("connecting the job to the completion port");
    }
    let (done_tx, done) = bounded(1);
    let raw = job.0 as usize;
    st.sessions.insert(
        key,
        Sess {
            job,
            done: done_tx,
            tx: None,
            journal: None,
            outbox: VecDeque::new(),
            head_journaled: false,
            deaf: false,
            held: HashMap::new(),
            join_lag_us: None,
            started: 0,
            exited: 0,
        },
    );
    // From here until the shell is in the job (`adopt`), this thread must
    // not be made to wait for a core: what the shell starts meanwhile is
    // started outside the job.
    Some(Watch { key, job: raw, done, finished: false, hurry: std::cell::Cell::new(hurry()), shell: std::cell::RefCell::new(None) })
}

impl Watch {
    /// Put the newly created shell in the job. Call it the instant the shell
    /// exists: whatever the shell starts before this returns is started
    /// outside the job.
    ///
    /// How long the shell has: `cmd` asks the console host a question before
    /// it does anything, and the console host does not answer a program
    /// until the terminal has answered its own opening questions (or three
    /// seconds have passed), which cannot happen before this returns; so
    /// `cmd` waits (`a_late_join_still_sees_the_shells_first_child`). That
    /// is `cmd`, not every console program: one that starts a child without
    /// asking the console anything is not held at all, and has been measured
    /// to have a child running within 20 ms of its own creation. Two things
    /// are done about that, neither of which is a guarantee:
    ///
    /// - from `prepare` until here the calling thread runs at the highest
    ///   ordinary priority, so it is not left waiting for a core between
    ///   creating the shell and this call (which takes about 30 microseconds).
    ///   Without that, on a machine with every core busy, the shell was seen
    ///   to win: 2 of 15 runs of this crate's tests lost a first child.
    /// - once the shell is in the job, anything it did start first, and
    ///   anything those have started, is found by its parent and put in the
    ///   job as well (`gather_strays`).
    ///
    /// What is still missed: a process the shell started that had ended
    /// before the shell joined, if it left nothing running.
    pub fn adopt(&self, shell: Option<RawHandle>) -> bool {
        let Some(shell) = shell else {
            self.unhurry();
            return false;
        };
        #[cfg(test)]
        std::thread::sleep(Duration::from_millis(JOIN_LATE_MS.get()));
        let called = Instant::now();
        let ok = unsafe { AssignProcessToJobObject(self.job as HANDLE, shell as HANDLE) } != 0;
        let err = unsafe { GetLastError() };
        let (call, now) = (called.elapsed().as_micros() as u64, now_us());
        self.unhurry();
        if !ok {
            paths::log(&format!("procs: the shell could not join its job (error {err}); this session's processes will not be sensed"));
            return false;
        }
        STATS.join_call_last_us.store(call, Relaxed);
        STATS.join_call_max_us.fetch_max(call, Relaxed);
        let lag = times(shell as HANDLE).and_then(|t| t.created_us).map(|c| now.saturating_sub(c));
        if let Some(lag) = lag {
            STATS.join_lag_last_us.store(lag, Relaxed);
            STATS.join_lag_max_us.fetch_max(lag, Relaxed);
            if let Some(s) = sensor() {
                s.with(self.key, |sess| sess.join_lag_us = Some(lag));
            }
        }
        // A handle of this module's own on the shell, for `finish` to wait on.
        let own = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, GetProcessId(shell as HANDLE)) };
        if !own.is_null() {
            *self.shell.borrow_mut() = Some(Owned(own));
        }
        let strays = gather_strays(self.job as HANDLE, shell as HANDLE, now);
        if strays > 0 {
            STATS.gathered.fetch_add(strays as u64, Relaxed);
            paths::log(&format!(
                "procs: the shell had started {strays} process(es) before it was in its job ({} us after its creation); they were put in the job after it",
                lag.map_or("?".into(), |l| l.to_string())
            ));
        }
        true
    }

    /// Give the calling thread back the priority `prepare` took it from.
    fn unhurry(&self) {
        if let Some((thread, old)) = self.hurry.take() {
            if thread == unsafe { GetCurrentThreadId() } {
                unsafe { SetThreadPriority(GetCurrentThread(), old) };
            }
        }
    }

    /// Say where this session's events go. Until then they are kept, in order.
    pub fn attach(&self, tx: Sender<SessionEvent>, journal: Option<Arc<Mutex<JournalWriter>>>) {
        if let Some(s) = sensor() {
            s.with(self.key, |sess| {
                sess.tx = Some(tx);
                sess.journal = journal;
            });
            // Delivery happens on the sensor's thread only, which keeps order.
            s.post(OP_ATTACH, self.key);
        }
    }

    /// The session has ended: everything the OS reported up to now reaches
    /// the window (waiting for it if need be) before this returns, then the
    /// job and every held handle are released.
    ///
    /// A session ends when its pseudo-terminal closes, and that can be a
    /// moment before Windows has finished with the shell: the console host
    /// goes as soon as its last program lets go of it, which a program does
    /// on its way out. So the shell is waited for first (it is normally
    /// gone already; two seconds at most). The job reports an exit before
    /// the process counts as ended, and the sensor takes its messages in
    /// order, so the shell's own exit is then recorded before the session is
    /// closed, instead of the shell being left "running" in the ledger of a
    /// session that is over.
    pub fn finish(mut self) {
        self.finished = true;
        if let Some(shell) = self.shell.borrow().as_ref() {
            signaled(shell.0, EXIT_PATIENCE.as_millis() as u32);
        }
        let Some(s) = sensor() else { return };
        if !s.post(OP_FINISH, self.key) {
            return;
        }
        if let Ok(rest) = self.done.recv_timeout(Duration::from_secs(5)) {
            if let Some(tx) = rest.tx {
                for ev in rest.events {
                    if tx.send(SessionEvent::Proc(Box::new(ev))).is_err() {
                        break;
                    }
                }
            }
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // A shell that could not be created at all never reached `adopt`.
        self.unhurry();
        if !self.finished {
            if let Some(s) = sensor() {
                s.post(OP_DROP, self.key);
            }
        }
    }
}

/// One data folder and socket for the whole test process, set before
/// anything reads them and never removed while tests run: tests share the
/// process, and the console host unpacked into the folder stays loaded.
#[cfg(test)]
pub(crate) fn test_fixture() -> &'static std::path::Path {
    static DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let tmp = std::env::temp_dir();
        // Folders of runs that are long over.
        for e in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let stale = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|a| a > Duration::from_secs(3600)));
            if stale && e.file_name().to_string_lossy().starts_with("proteus-test-") {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        let dir = tmp.join(format!("proteus-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test data folder");
        std::env::set_var("PROTEUS_DATA", &dir);
        std::env::set_var("PROTEUS_SOCKET", dir.join("d.sock"));
        std::env::set_var("PROTEUS_SOUND", "off");
        dir
    })
}

/// One argument of 30,000 UTF-16 units with no ASCII in it: accented Latin,
/// CJK, an arrow, Greek, and an emoji that takes two units. What the helper
/// mode "long-cmd" gives its child (see `tests::helper`).
#[cfg(test)]
pub(crate) fn long_arg() -> String {
    let mut s = String::new();
    let mut units = 0;
    for c in "日本語ünï→λ🙂ж".chars().cycle() {
        if units + c.len_utf16() > 30_000 {
            break;
        }
        units += c.len_utf16();
        s.push(c);
    }
    while units < 30_000 {
        s.push('é');
        units += 1;
    }
    s
}

/// A real session watched the way a window watches one: everything it
/// printed and every process event, in order. It also answers the console
/// host's two start-up questions (where is the cursor, what are you) as a
/// terminal does; until they are answered, or three seconds have passed, the
/// console host does not let the shell start.
#[cfg(test)]
pub(crate) struct Probe {
    pub h: crate::session::SessionHandle,
    pub out: Vec<u8>,
    pub events: Vec<ProcEvent>,
    pub ledger: crate::procs::Ledger,
    /// The session's own exit, once it has been delivered.
    pub exit: Option<Option<u32>>,
    /// How many of `events` arrived as replayed history (between a daemon's
    /// replay markers), which is what a window is told is not live.
    pub replayed: usize,
    replaying: bool,
    asked: [bool; 2],
}

#[cfg(test)]
impl Probe {
    pub fn new(h: crate::session::SessionHandle) -> Self {
        Self { h, out: Vec::new(), events: Vec::new(), ledger: crate::procs::Ledger::new(), exit: None, replayed: 0, replaying: false, asked: [false; 2] }
    }

    pub fn said(&self, needle: &[u8]) -> bool {
        self.out.windows(needle.len()).any(|w| w == needle)
    }

    pub fn send(&self, text: &str) {
        let _ = self.h.cmds.send(crate::session::SessionCmd::Input(text.as_bytes().to_vec()));
    }

    /// Take what the session delivers until `done` says so (true), or until
    /// the time is up or the session is gone (false).
    pub fn until(&mut self, secs: u64, done: impl Fn(&Probe) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done(self) {
            match self.h.events.recv_deadline(deadline) {
                Ok(SessionEvent::Output(b)) => {
                    self.out.extend_from_slice(&b);
                    for (i, (question, answer)) in [("\x1b[6n", "\x1b[1;1R"), ("\x1b[c", "\x1b[?6c")].into_iter().enumerate() {
                        if !self.asked[i] && self.said(question.as_bytes()) {
                            self.asked[i] = true;
                            self.send(answer);
                        }
                    }
                }
                Ok(SessionEvent::Proc(ev)) => {
                    self.replayed += self.replaying as usize;
                    self.ledger.apply(&ev);
                    self.events.push(*ev);
                }
                Ok(SessionEvent::Exit(code)) => self.exit = Some(code),
                Ok(SessionEvent::ReplayResize(..)) => self.replaying = true,
                Ok(SessionEvent::ReplayDone) => self.replaying = false,
                Err(_) => return done(self),
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{self, SessionSpec};
    use windows_sys::Win32::System::Threading::{TerminateProcess, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    /// Run a real session to its end and return everything it reported.
    fn run(exe: &str, args: &[&str], env: Option<Vec<(String, String)>>, journal: bool) -> Probe {
        test_fixture();
        let spec = SessionSpec { exe: exe.into(), args: args.iter().map(|a| a.to_string()).collect(), cwd: None, cols: 80, rows: 24, journal, env };
        let mut p = Probe::new(session::spawn_local(spec).expect("session"));
        assert!(p.until(90, |p| p.exit.is_some()), "the session did not end; so far: {:#?}", p.events);
        // The session's exit is the last word: no process event may trail it.
        let seen = p.events.len();
        p.until(1, |_| false);
        assert_eq!(p.events.len(), seen, "a process event arrived after the session's exit");
        p
    }

    /// The command line that makes this test binary run only `helper` below.
    const HELPER: [&str; 3] = ["procs_win::tests::helper", "--exact", "--nocapture"];

    /// Not a test of anything by itself: the body of the real child processes
    /// the tests below start (this same binary, told what to be by a variable).
    #[test]
    fn helper() {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        let Ok(mode) = std::env::var("PROTEUS_TEST_HELPER") else { return };
        if mode == "sleep" {
            std::thread::sleep(Duration::from_secs(40));
            std::process::exit(0);
        }
        if mode == "exit-21" {
            std::process::exit(21);
        }
        if mode == "nap" {
            std::thread::sleep(Duration::from_millis(1500));
            std::process::exit(21);
        }
        if mode == "stray-mid" || mode == "strays" {
            // "strays": a shell that starts a child at once, which at once
            // starts one of its own, which stays a while. Each waits for
            // the one it started.
            let (next, code) = if mode == "strays" { ("stray-mid", 0) } else { ("nap", 22) };
            let ran = Command::new(std::env::current_exe().unwrap())
                .args(HELPER)
                .env("PROTEUS_TEST_HELPER", next)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok();
            std::process::exit(if ran { code } else { 3 });
        }
        if mode == "child-at-once" {
            // Be a shell that starts a child the moment it runs, sooner than
            // any real shell does: the hardest case for the join.
            let code = Command::new(std::env::current_exe().unwrap())
                .args(HELPER)
                .env("PROTEUS_TEST_HELPER", "exit-21")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .ok()
                .and_then(|s| s.code());
            std::process::exit(if code == Some(21) { 0 } else { 3 });
        }
        if mode == "long-cmd" {
            // Start a child with a command line near the longest Windows
            // lets a caller pass (32,767 UTF-16 units), none of it ASCII.
            let child = Command::new(std::env::current_exe().unwrap())
                .args(HELPER)
                .arg(long_arg())
                .env("PROTEUS_TEST_HELPER", "exit-21")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let line = match child {
                Ok(s) => format!("ok {:?}", s.code()),
                Err(e) => format!("err {e}"),
            };
            std::fs::write(std::env::var("PROTEUS_TEST_OUT").unwrap(), line).unwrap();
            std::process::exit(0);
        }
        if mode == "host-in-a-job-breakaway" {
            // Be a PROTEUS inside an outer job that forbids breakaway, and
            // run a session whose shell starts a child asking to break away
            // (as the daemon launch does). Say whether that child started,
            // whether the session heard of it, and whose job it ended up in.
            let outer = unsafe { CreateJobObjectW(null(), null()) };
            let joined = unsafe { AssignProcessToJobObject(outer, GetCurrentProcess()) } != 0;
            let inner_out = format!("{}.inner", std::env::var("PROTEUS_TEST_OUT").unwrap());
            let mut env: Vec<(String, String)> = std::env::vars().collect();
            env.retain(|(k, _)| k != "PROTEUS_TEST_HELPER" && k != "PROTEUS_TEST_OUT");
            env.push(("PROTEUS_TEST_HELPER".into(), "spawn-breakaway".into()));
            env.push(("PROTEUS_TEST_OUT".into(), inner_out.clone()));
            let spec = SessionSpec { exe: std::env::current_exe().unwrap().to_string_lossy().into_owned(), args: HELPER.iter().map(|a| a.to_string()).collect(), cwd: None, cols: 80, rows: 24, journal: false, env: Some(env) };
            let mut p = Probe::new(session::spawn_local(spec).unwrap());
            p.until(60, |p| p.exit.is_some());
            let line = std::fs::read_to_string(&inner_out).unwrap_or_else(|e| format!("nothing written ({e})"));
            let child: Option<u32> = line.strip_prefix("ok ").and_then(|p| p.parse().ok());
            let sensed = child.is_some_and(|c| p.ledger.procs().iter().any(|q| q.pid == c));
            // Is the child in the outer job (it could not leave that one)?
            let (mut in_outer, mut alive_after) = (false, false);
            if let Some(c) = child {
                let h = Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, c) });
                if !h.0.is_null() {
                    let mut inside = 0;
                    in_outer = unsafe { IsProcessInJob(h.0, outer, &mut inside) } != 0 && inside != 0;
                    alive_after = !signaled(h.0, 0);
                    // The sleeping child this helper's own session started: end it by its handle.
                    unsafe { TerminateProcess(h.0, 1) };
                }
            }
            std::fs::write(
                std::env::var("PROTEUS_TEST_OUT").unwrap(),
                format!("in-a-job={joined} child-started={} sensed={sensed} in-outer-job={in_outer} alive-after-session={alive_after} session-procs={}", child.is_some(), p.ledger.procs().len()),
            )
            .unwrap();
            std::process::exit(0);
        }
        if mode == "host-in-a-job" {
            // Be a PROTEUS that someone else has already put in a job of
            // their own (one that grants nothing, not even breakaway), then
            // run a session and say what was sensed.
            let outer = unsafe { CreateJobObjectW(null(), null()) };
            let joined = unsafe { AssignProcessToJobObject(outer, GetCurrentProcess()) } != 0;
            let spec = SessionSpec { exe: "cmd.exe".into(), args: vec!["/c".into(), "cmd /c exit 3".into()], cwd: None, cols: 80, rows: 24, journal: false, env: None };
            let mut p = Probe::new(session::spawn_local(spec).unwrap());
            p.until(60, |p| p.exit.is_some());
            let codes: Vec<Option<u32>> = p.ledger.procs().iter().map(|p| p.exit_code()).collect();
            std::fs::write(std::env::var("PROTEUS_TEST_OUT").unwrap(), format!("in-a-job={joined} sensed={codes:?}")).unwrap();
            std::process::exit(0);
        }
        if mode == "job-of-its-own" {
            // Be a program that sandboxes what it starts: a job of its own,
            // set to kill on close and with a UI restriction, around a child.
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::{JobObjectBasicUIRestrictions, JOBOBJECT_BASIC_UI_RESTRICTIONS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_HANDLES};
            let inner = unsafe { CreateJobObjectW(null(), null()) };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let limited = unsafe { SetInformationJobObject(inner, JobObjectExtendedLimitInformation, &limits as *const _ as *const c_void, size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) } != 0;
            let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS { UIRestrictionsClass: JOB_OBJECT_UILIMIT_HANDLES };
            let restricted = unsafe { SetInformationJobObject(inner, JobObjectBasicUIRestrictions, &ui as *const _ as *const c_void, size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32) } != 0;
            let mut child = Command::new("cmd.exe").args(["/c", "ping -n 2 127.0.0.1 >nul & exit 6"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let assigned = unsafe { AssignProcessToJobObject(inner, child.as_raw_handle() as HANDLE) } != 0;
            let error = if assigned { 0 } else { unsafe { GetLastError() } };
            let code = child.wait().unwrap().code();
            std::fs::write(std::env::var("PROTEUS_TEST_OUT").unwrap(), format!("limited={limited} restricted={restricted} assigned={assigned} error={error} code={code:?}")).unwrap();
            std::process::exit(0);
        }
        // Start a child that outlives this process, the way the daemon is
        // started (remote.rs): detached, in its own group, and for
        // "spawn-breakaway" asking to leave whatever job this process is in.
        let leave = if mode == "spawn-breakaway" { CREATE_BREAKAWAY_FROM_JOB } else { 0 };
        let child = Command::new(std::env::current_exe().unwrap())
            .args(HELPER)
            .env("PROTEUS_TEST_HELPER", "sleep")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | leave)
            .spawn();
        let line = match child {
            Ok(c) => format!("ok {}", c.id()),
            Err(e) => format!("err {e}"),
        };
        std::fs::write(std::env::var("PROTEUS_TEST_OUT").unwrap(), line).unwrap();
        std::process::exit(0);
    }

    /// Run this binary as a session's shell in the given helper mode; returns
    /// what the session reported and the line the helper wrote.
    fn run_as_shell(mode: &str) -> (Probe, String) {
        let out = test_fixture().join(format!("helper-{mode}.txt"));
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.push(("PROTEUS_TEST_HELPER".into(), mode.into()));
        env.push(("PROTEUS_TEST_OUT".into(), out.to_string_lossy().into_owned()));
        let ran = run(&std::env::current_exe().unwrap().to_string_lossy(), &HELPER, Some(env), false);
        let line = std::fs::read_to_string(&out).unwrap_or_else(|e| panic!("the helper wrote nothing ({e}); events: {:#?}", ran.events));
        (ran, line)
    }

    /// The same, for the modes that start a lasting child: returns its pid.
    fn run_helper(mode: &str) -> (Probe, u32) {
        let (ran, line) = run_as_shell(mode);
        let pid = line.strip_prefix("ok ").unwrap_or_else(|| panic!("the helper could not start its child: {line}")).parse().unwrap();
        (ran, pid)
    }

    /// Is `pid` still the sleeping helper this test started?
    fn alive(pid: u32) -> bool {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
        if h.is_null() {
            return false;
        }
        let h = Owned(h);
        let me = std::env::current_exe().unwrap().to_string_lossy().to_lowercase();
        !signaled(h.0, 0) && image_path(h.0).is_some_and(|i| i.to_lowercase() == me)
    }

    /// End a process this test started itself.
    fn end(pid: u32) {
        let h = Owned(unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) });
        if !h.0.is_null() {
            unsafe { TerminateProcess(h.0, 1) };
            signaled(h.0, 5000);
        }
    }

    #[test]
    fn a_session_reports_its_whole_process_tree_from_the_os() {
        let r = run("cmd.exe", &["/c", "cmd /c exit 7"], None, true);
        assert_eq!(r.exit, Some(Some(7)));
        let procs = r.ledger.procs();
        assert_eq!(procs.len(), 2, "{:#?}", r.events);
        let (shell, child) = (&procs[0], &procs[1]);
        assert_eq!(shell.ppid, Some(std::process::id()), "the shell is this process's child");
        assert_eq!(shell.parent, None);
        assert!(shell.image.as_deref().unwrap().to_lowercase().ends_with("\\cmd.exe"), "{:?}", shell.image);
        assert!(shell.cmd.as_deref().unwrap().contains("cmd /c exit 7"), "{:?}", shell.cmd);
        assert_eq!((child.ppid, child.parent), (Some(shell.pid), Some(shell.id)));
        assert!(child.cmd.as_deref().unwrap().contains("/c exit 7"), "{:?}", child.cmd);
        for p in procs {
            let e = p.end.as_ref().unwrap_or_else(|| panic!("no exit recorded for {p:?}"));
            assert_eq!(e.code, Some(7));
            assert!(p.start_exact && e.end_exact && e.end_us >= p.start_us, "{p:?}");
            assert!(e.user_us.is_some() && e.kernel_us.is_some() && !e.abnormal && !e.lost, "{p:?}");
            assert!(p.latency_us().unwrap() < 5_000_000, "seen {} us after creation", p.latency_us().unwrap());
        }
        let (se, ce) = (shell.end.as_ref().unwrap(), child.end.as_ref().unwrap());
        assert!(shell.start_us <= child.start_us && ce.end_us <= se.end_us, "the child lived inside the shell's life");

        // The journal holds the same history, and knows the session is over.
        let j = crate::procs::read_journal(r.h.journal_path.as_ref().unwrap()).unwrap();
        assert!(j.ended);
        assert_eq!(j.ledger.procs(), r.ledger.procs());
    }

    #[test]
    fn every_short_lived_process_is_seen_with_its_own_exit_code() {
        let r = run("cmd.exe", &["/c", "for /l %i in (1,1,40) do @cmd /c exit %i"], None, false);
        let kids: Vec<_> = r.ledger.procs().iter().skip(1).collect();
        assert_eq!(kids.len(), 40, "{:#?}", r.events);
        for p in kids {
            let cmd = p.cmd.as_deref().unwrap_or_else(|| panic!("no command line for {p:?}"));
            let n: u32 = cmd.trim().rsplit(' ').next().unwrap().parse().unwrap_or_else(|_| panic!("unexpected command line {cmd:?}"));
            assert_eq!(p.exit_code(), Some(n), "{cmd:?} must report its own exit code");
        }
    }

    /// What the sensor can still learn when it reaches a process only after
    /// it has ended. (A parent normally holds its child until it has
    /// collected the exit code, which keeps the ended process openable.)
    #[test]
    fn a_process_that_has_already_ended_still_tells_all_but_its_command_line() {
        use std::os::windows::io::AsRawHandle;
        use std::process::{Command, Stdio};
        let job = Owned(unsafe { CreateJobObjectW(null(), null()) });
        let mut child = Command::new("cmd.exe").args(["/c", "exit 5"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        assert!(unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) } != 0, "error {}", unsafe { GetLastError() });
        assert_eq!(child.wait().unwrap().code(), Some(5));
        // `child` is still held, as a shell holds what it started.
        let h = Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, child.id()) });
        assert!(!h.0.is_null(), "an ended process could not be opened (error {})", unsafe { GetLastError() });
        assert!(signaled(h.0, 0) && settled(h.0));
        let mut inside = 0;
        assert!(unsafe { IsProcessInJob(h.0, job.0, &mut inside) } != 0 && inside != 0, "an ended process must still count as its job's, or its details would be thrown away");
        assert_eq!(parent_of(h.0), Some(std::process::id()));
        let t = times(h.0).unwrap();
        assert!(t.created_us.is_some() && t.exited_us >= t.created_us, "creation and exit times outlive the process");
        // The ordinary question fails for an ended process; the answer comes
        // from the kernel's name for the file, turned back into a drive path.
        assert_eq!(image_name(h.0, 0), None);
        let image = image_path(h.0).unwrap();
        assert!(image.as_bytes()[1] == b':' && image.to_lowercase().ends_with("\\windows\\system32\\cmd.exe"), "{image}");
        assert_eq!(command_line(h.0), None, "the command line lived in the process's own memory");
        assert_eq!(drive_path("\\Device\\NoSuchVolume\\x.exe"), "\\Device\\NoSuchVolume\\x.exe");
    }

    /// The shell joins its job a moment after it is created. This holds the
    /// join back a full second, in which a shell could start a great deal,
    /// and requires that nothing was missed: the console host keeps the
    /// shell at its start-up until the terminal answers, and the terminal
    /// cannot answer before the join. If a newer console host stops doing
    /// that, this fails, and the join needs another way to be first.
    #[test]
    fn a_late_join_still_sees_the_shells_first_child() {
        JOIN_LATE_MS.set(1000);
        let r = run("cmd.exe", &["/c", "cmd /c exit 9"], None, false);
        JOIN_LATE_MS.set(0);
        let procs = r.ledger.procs();
        assert_eq!(procs.len(), 2, "the shell ran ahead of its job: {:#?}", r.events);
        assert_eq!((procs[1].parent, procs[1].exit_code()), (Some(procs[0].id), Some(9)));
        // Not because the shell was slow: it had existed for over a second
        // before it was even in the job, and started its child only after.
        assert!(procs[0].latency_us().unwrap() >= 1_000_000, "the join was not actually late: {:?}", procs[0]);
        assert!(procs[1].start_us - procs[0].start_us >= 1_000_000, "{procs:#?}");
    }

    #[test]
    fn a_child_that_asks_to_leave_the_job_still_starts_and_outlives_the_session() {
        let (r, pid) = run_helper("spawn-breakaway");
        // It left the job at birth, so the session never heard of it...
        assert!(r.ledger.procs().iter().all(|p| p.pid != pid), "{:#?}", r.events);
        assert_eq!(r.ledger.procs().len(), 1, "only the helper itself: {:#?}", r.events);
        // ...and with that session over, it is still running.
        assert!(alive(pid), "the child that broke away did not survive");
        end(pid);
    }

    /// PROTEUS is often not the outermost thing: a launcher, a CI runner or
    /// another terminal may already hold it in a job. The session's job then
    /// nests inside that one, and sensing must work all the same.
    #[test]
    fn sensing_works_when_the_program_is_itself_inside_someone_elses_job() {
        use std::process::{Command, Stdio};
        let out = test_fixture().join("helper-host-in-a-job.txt");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(HELPER)
            .env("PROTEUS_TEST_HELPER", "host-in-a-job")
            .env("PROTEUS_TEST_OUT", &out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "in-a-job=true sensed=[Some(3), Some(3)]");
    }

    /// Programs inside a session use jobs too (sandboxes, build tools). One
    /// that puts its child in a job of its own, with limits, must still be
    /// able to, and what runs in that inner job is still the session's.
    #[test]
    fn a_program_in_a_session_can_still_put_its_children_in_a_job_of_its_own() {
        let (r, line) = run_as_shell("job-of-its-own");
        assert_eq!(line, "limited=true restricted=true assigned=true error=0 code=Some(6)");
        let shell = r.ledger.procs()[0].id;
        let inner = r.ledger.procs().iter().find(|p| p.parent == Some(shell)).unwrap_or_else(|| panic!("the child in the inner job was not sensed: {:#?}", r.events));
        assert_eq!(inner.exit_code(), Some(6));
        // And what that child started, two jobs deep, as well.
        assert!(r.ledger.procs().iter().any(|p| p.parent == Some(inner.id) && p.image.as_deref().is_some_and(|i| i.to_lowercase().ends_with("\\ping.exe"))), "{:#?}", r.events);
    }

    /// A command line near the longest Windows allows (30,000 UTF-16 units
    /// in one argument), none of it ASCII, is recorded whole, and survives
    /// the encoding the journal and the daemon's frames carry it in.
    #[test]
    fn a_very_long_command_line_that_is_not_ascii_is_recorded_whole() {
        let (r, line) = run_as_shell("long-cmd");
        assert_eq!(line, "ok Some(21)", "the child with the long command line did not run");
        let p = r.ledger.procs().iter().find(|p| p.exit_code() == Some(21)).unwrap_or_else(|| panic!("the child was not sensed: {} events", r.events.len()));
        let cmd = p.cmd.as_deref().expect("its command line was not read");
        let want = long_arg();
        assert_eq!(want.encode_utf16().count(), 30_000);
        assert!(cmd.ends_with(&want), "the command line was cut or changed: {} UTF-16 units read, ends {:?}", cmd.encode_utf16().count(), cmd.chars().rev().take(12).collect::<String>());
        assert!(cmd.encode_utf16().count() > 30_000 && cmd.encode_utf16().count() < 32_767);
        assert_eq!(p.parent, Some(r.ledger.procs()[0].id));
        for ev in &r.events {
            assert_eq!(ProcEvent::from_bytes(&ev.to_bytes()).as_ref(), Some(ev), "an event did not survive its own encoding");
        }
        // The table a person reads shows the start of it and does not break.
        let table = r.ledger.render(true, now_us());
        assert!(table.lines().all(|l| l.chars().count() < 260), "a row of the table ran on");
    }

    /// An exit Windows classes as abnormal (here an access-violation status)
    /// is one process with that code, marked abnormal: the job posts one
    /// message for it, not an ordinary exit as well.
    #[test]
    fn an_abnormal_exit_is_one_process_with_its_own_code() {
        let r = run("cmd.exe", &["/c", "cmd /c exit -1073741819"], None, false);
        let procs = r.ledger.procs();
        assert_eq!(procs.len(), 2, "{:#?}", r.events);
        assert_eq!(r.events.len(), 4, "two starts and two exits, nothing twice: {:#?}", r.events);
        let child = procs[1].end.as_ref().expect("no exit recorded for the child");
        assert_eq!((child.code, child.abnormal, child.lost), (Some(0xC000_0005), true, false), "{:#?}", r.events);
        assert!(child.end_exact && child.user_us.is_some());
    }

    /// PROTEUS inside an outer job that forbids breakaway, and a shell that
    /// starts a child asking to break away (what the daemon launch does):
    /// the child must still start and outlive the session. Windows lets it
    /// leave the session's job (which allows it) and keeps it in the outer one.
    #[test]
    fn a_child_can_leave_the_sessions_job_even_when_an_outer_job_forbids_breakaway() {
        use std::process::{Command, Stdio};
        let out = test_fixture().join("helper-host-in-a-job-breakaway.txt");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(HELPER)
            .env("PROTEUS_TEST_HELPER", "host-in-a-job-breakaway")
            .env("PROTEUS_TEST_OUT", &out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "in-a-job=true child-started=true sensed=false in-outer-job=true alive-after-session=true session-procs=1"
        );
    }

    /// Only `cmd` is held at its start by the console host. A shell that
    /// starts a child without asking the console anything has it running
    /// within milliseconds, so if the join is late (here: by 400 ms, on
    /// purpose) that child, and the child it starts in turn, begin outside
    /// the job. They must be found and put in it: three processes, their
    /// tree, their command lines and their own exit codes, not one.
    #[test]
    fn what_a_shell_started_before_it_was_in_its_job_is_gathered_in_after_it() {
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.push(("PROTEUS_TEST_HELPER".into(), "strays".into()));
        let exe = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        // On a machine so busy that the shell has not even started its child
        // 400 ms after its own creation, a session shows nothing about
        // gathering (the child is then simply born in the job): try again.
        for _ in 0..4 {
            let before = STATS.gathered.load(Relaxed);
            JOIN_LATE_MS.set(400);
            let r = run(&exe, &HELPER, Some(env.clone()), false);
            JOIN_LATE_MS.set(0);
            assert_eq!(r.exit, Some(Some(0)), "the shell could not run its child");
            let procs = r.ledger.procs();
            assert!(procs[0].latency_us().unwrap() >= 400_000, "the join was not actually late: {:?}", procs[0]);
            assert_eq!(procs.len(), 3, "the shell ran ahead of its job and what it started was never seen: {:#?}", r.events);
            let (shell, child, grandchild) = (&procs[0], &procs[1], &procs[2]);
            assert_eq!((child.parent, grandchild.parent), (Some(shell.id), Some(child.id)), "{procs:#?}");
            assert_eq!((shell.exit_code(), child.exit_code(), grandchild.exit_code()), (Some(0), Some(22), Some(21)));
            for p in procs {
                assert!(p.cmd.as_deref().is_some_and(|c| c.contains("procs_win::tests::helper")) && p.image.is_some() && p.start_exact, "{p:?}");
            }
            // Created before the shell was recorded as joined: it began outside the job.
            if child.start_us < shell.seen_us {
                assert!(child.seen_us >= shell.seen_us, "it could only be recorded once it had been put in the job: {child:?}");
                assert!(STATS.gathered.load(Relaxed) > before, "the sensor's own count of what it gathered");
                return;
            }
        }
        panic!("in four sessions the shell never started its child within 400 ms of its own creation: nothing was learned about gathering");
    }

    /// From the moment a session's job is ready until its shell is in it,
    /// the thread creating the shell runs at the highest ordinary priority:
    /// a thread left waiting for a core at that point gives the shell time
    /// to start things outside the job. Afterwards, and on every way out,
    /// the thread has the priority it came with.
    #[test]
    fn the_thread_that_starts_a_shell_is_not_kept_waiting_until_the_shell_is_in_its_job() {
        use std::os::windows::io::AsRawHandle;
        use std::process::{Command, Stdio};
        test_fixture();
        let priority = || unsafe { GetThreadPriority(GetCurrentThread()) };
        let before = priority();
        assert_ne!(before, THREAD_PRIORITY_TIME_CRITICAL, "tests run at an ordinary priority");
        // The ordinary way: the shell is created, then joins.
        let watch = prepare().expect("a job for the session");
        assert_eq!(priority(), THREAD_PRIORITY_TIME_CRITICAL, "ahead of ordinary work while the shell is created");
        let mut child = Command::new("cmd.exe").args(["/c", "exit 0"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        assert!(watch.adopt(Some(child.as_raw_handle())), "the shell could not join its job");
        assert_eq!(priority(), before, "given back once the shell is in its job");
        child.wait().unwrap();
        drop(watch);
        // The shell could not be created: the session's place is dropped unused.
        drop(prepare().expect("a job for the session"));
        assert_eq!(priority(), before, "given back when no shell was ever created");
        // The pseudo-terminal gave no handle to join with.
        let watch = prepare().expect("a job for the session");
        assert!(!watch.adopt(None));
        assert_eq!(priority(), before, "given back when there was nothing to join");
    }

    /// A session's pseudo-terminal can close a moment before Windows has
    /// finished with the shell. Here the moment is made long: the session is
    /// ended while its shell still has a second to live. The shell's own
    /// exit, with its code, must still be in what the session reports; it
    /// must not be left "running" in a session that is over.
    #[test]
    fn a_session_that_ends_before_its_shell_is_quite_gone_still_records_the_shells_exit() {
        use std::os::windows::io::AsRawHandle;
        use std::process::{Command, Stdio};
        test_fixture();
        let (tx, rx) = bounded::<SessionEvent>(256);
        let watch = prepare().expect("a job for the session");
        let mut shell = Command::new("cmd.exe").args(["/c", "ping -n 2 127.0.0.1 >nul & exit 7"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        assert!(watch.adopt(Some(shell.as_raw_handle())), "the shell could not join its job");
        watch.attach(tx, None);
        // The reader thread of a session does exactly this when its pipe closes.
        watch.finish();
        let mut ledger = crate::procs::Ledger::new();
        for ev in rx.try_iter() {
            if let SessionEvent::Proc(ev) = ev {
                ledger.apply(&ev);
            }
        }
        assert_eq!(shell.wait().unwrap().code(), Some(7));
        let first = ledger.procs().first().unwrap_or_else(|| panic!("nothing was reported at all"));
        assert_eq!(first.pid, shell.id());
        assert_eq!(first.exit_code(), Some(7), "the session is over and its shell is recorded as {}: {first:?}", if first.running() { "still running" } else { "ended without a code" });
        // (What the shell started may outlast it by a moment, as the console
        // host a windowless caller's `cmd` is given does; that is not at issue.)
        let ping = ledger.procs().iter().find(|p| p.image.as_deref().is_some_and(|i| i.to_lowercase().ends_with("\\ping.exe"))).expect("the shell's child was not reported");
        assert_eq!((ping.parent, ping.exit_code()), (Some(first.id), Some(0)), "{ping:?}");
    }

    /// Busy threads for as long as the guard lives: every core taken, so the
    /// thread that starts a session has to compete for its next turn.
    struct Busy(Arc<std::sync::atomic::AtomicBool>);
    impl Busy {
        fn start(threads: usize) -> Self {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            for _ in 0..threads {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut x = 1u64;
                    while !stop.load(Relaxed) {
                        x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
                    }
                });
            }
            Busy(stop)
        }
    }
    impl Drop for Busy {
        fn drop(&mut self) {
            self.0.store(true, Relaxed);
        }
    }

    /// Sessions whose shell starts a child at once, with every core busy.
    /// Returns (sessions in which that child was seen, sessions run, the
    /// longest the shell had existed before it was in its job, microseconds).
    fn fast_shells_under_load(sessions: usize, busy: usize) -> (usize, usize, u64) {
        test_fixture();
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.push(("PROTEUS_TEST_HELPER".into(), "child-at-once".into()));
        let exe = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        let _busy = Busy::start(busy);
        let (mut seen, mut lag) = (0, 0);
        for _ in 0..sessions {
            let spec = SessionSpec { exe: exe.clone(), args: HELPER.iter().map(|a| a.to_string()).collect(), cwd: None, cols: 80, rows: 24, journal: false, env: Some(env.clone()) };
            let mut p = Probe::new(session::spawn_local(spec).expect("session"));
            assert!(p.until(120, |p| p.exit.is_some()), "the session did not end");
            assert_eq!(p.exit, Some(Some(0)), "the shell could not run its child at all");
            let procs = p.ledger.procs();
            seen += (procs.len() == 2 && procs[1].parent == Some(procs[0].id) && procs[1].exit_code() == Some(21)) as usize;
            lag = lag.max(procs.first().and_then(|s| s.latency_us()).unwrap_or(0));
        }
        (seen, sessions, lag)
    }

    /// The join race, measured where it is worst: a shell that starts its
    /// child at once (no real shell is this quick), on a machine with no core
    /// to spare. `PROTEUS_MEASURE_N` sessions (default 40), with
    /// `PROTEUS_MEASURE_LOAD` busy threads (default: two per core).
    ///
    ///   cargo test --release -- --ignored --nocapture --test-threads=1 measure_join_race_with_a_shell_that_starts_a_child_at_once
    #[test]
    #[ignore]
    fn measure_join_race_with_a_shell_that_starts_a_child_at_once() {
        let num = |name: &str, default: usize| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let cores = std::thread::available_parallelism().map_or(8, |n| n.get());
        let (n, busy) = (num("PROTEUS_MEASURE_N", 40), num("PROTEUS_MEASURE_LOAD", 2 * cores));
        let (seen, sessions, lag) = fast_shells_under_load(n, busy);
        println!("MEASURE join_race_fast_shell {}", serde_json::json!({ "sessions": sessions, "first_child_seen": seen, "missed": sessions - seen, "busy_threads": busy, "longest_shell_created_to_recorded_us": lag }));
        // And is such a shell held at its start-up by the console host, as
        // cmd is? With the join held back on purpose and nothing else busy.
        let mut held = Vec::new();
        for late_ms in [5u64, 20, 50, 200, 1000] {
            JOIN_LATE_MS.set(late_ms);
            let (seen, sessions, _) = fast_shells_under_load(5, 0);
            held.push(serde_json::json!({ "join_late_by_ms": late_ms, "sessions": sessions, "first_child_seen": seen }));
        }
        JOIN_LATE_MS.set(0);
        println!("MEASURE join_race_fast_shell_held_back {}", serde_json::json!(held));
        // What the look for strays costs every session start: one snapshot
        // of the system's processes (a second one only if a stray is found).
        let mut us: Vec<u64> = (0..50)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(parent_links());
                t.elapsed().as_micros() as u64
            })
            .collect();
        us.sort_unstable();
        println!("MEASURE process_snapshot {}", serde_json::json!({ "processes": parent_links().len(), "n": us.len(), "p50_us": us[us.len() / 2], "max_us": us[us.len() - 1] }));
    }

    #[test]
    fn ending_a_session_and_closing_its_job_kills_nothing() {
        let (r, pid) = run_helper("spawn-stay");
        let shell = &r.ledger.procs()[0];
        assert_eq!(shell.exit_code(), Some(0), "{:#?}", r.events);
        let kept = r.ledger.procs().iter().find(|p| p.pid == pid).unwrap_or_else(|| panic!("the child was not reported: {:#?}", r.events));
        assert_eq!(kept.parent, Some(shell.id));
        assert!(kept.running(), "it has not ended, so no exit may be recorded: {kept:?}");
        // The session has ended and its job handle is closed. Nothing in the job was told to die.
        assert!(alive(pid), "a process was killed by the end of the session that started it");
        end(pid);
    }
}
