//! The console host's ceiling, measured. A program inside a terminal on
//! Windows writes to a console; the console host (OpenConsole, through
//! conpty.dll) turns that into VT and writes it into a pipe the terminal
//! reads. This module runs a self-timing emitter through a chosen console
//! host and a chosen kind of pipe, into a reader that throws the bytes away
//! or into PROTEUS's own parser, so each stage has its own number.
//!
//!   proteus conhost flood [--host bundled|DIR] [--pipe anon|named|overlapped]
//!                         [--quota BYTES] [--read BYTES] [--sink null|parse]
//!                         [--mb N] [--chunk BYTES] [--api file|wide] [--vt 0|1]
//!                         [--kind ascii|sgr] [--runs N]
//!   proteus conhost echo  [--host ..] [--pipe ..] [--keys N]   key -> echo latency
//!   proteus conhost idle  [--host ..] [--pipe ..] [--secs N]   console host CPU while idle
//!   proteus conhost emit  ...                                  the child (internal)
//!
//! `--host DIR` loads DIR\conpty.dll by full path, which starts
//! DIR\OpenConsole.exe; `bundled` is the copy embedded in this exe.
//! `--pipe anon` is what portable-pty makes (CreatePipe, default size,
//! synchronous); `overlapped` is what Windows Terminal makes (the console's
//! end opened for overlapped I/O, 128 KiB quota), which lets OpenConsole
//! keep parsing while its previous write drains (VtIo::_flushNow).
//! Every result is printed as one JSON line.
#![cfg(windows)]

use std::ffi::c_void;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::Res;

type Handle = *mut c_void;
const INVALID: Handle = -1isize as Handle;

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_chars: u32,
    y_chars: u32,
    fill: u32,
    flags: u32,
    show: u16,
    reserved2_len: u16,
    reserved2: *mut u8,
    std_in: Handle,
    std_out: Handle,
    std_err: Handle,
}

#[repr(C)]
struct StartupInfoExW {
    si: StartupInfoW,
    attrs: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct ProcessInformation {
    process: usize,
    thread: usize,
    pid: u32,
    tid: u32,
}

#[repr(C)]
struct ProcessEntry32W {
    size: u32,
    usage: u32,
    pid: u32,
    heap: usize,
    module: u32,
    threads: u32,
    parent: u32,
    pri: i32,
    flags: u32,
    exe: [u16; 260],
}

#[repr(C)]
struct Coord {
    x: i16,
    y: i16,
}

extern "system" {
    fn CreatePipe(read: *mut Handle, write: *mut Handle, sa: *const c_void, size: u32) -> i32;
    fn CreateNamedPipeW(name: *const u16, open: u32, mode: u32, max: u32, out: u32, inq: u32, timeout: u32, sa: *const c_void) -> Handle;
    fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *const c_void, disp: u32, flags: u32, tmpl: Handle) -> Handle;
    fn ReadFile(h: Handle, buf: *mut u8, n: u32, read: *mut u32, ov: *mut c_void) -> i32;
    fn WriteFile(h: Handle, buf: *const u8, n: u32, written: *mut u32, ov: *mut c_void) -> i32;
    fn CloseHandle(h: Handle) -> i32;
    fn LoadLibraryW(name: *const u16) -> Handle;
    fn GetProcAddress(m: Handle, name: *const u8) -> *const c_void;
    fn InitializeProcThreadAttributeList(list: *mut c_void, count: u32, flags: u32, size: *mut usize) -> i32;
    fn UpdateProcThreadAttribute(list: *mut c_void, flags: u32, attr: usize, val: *const c_void, size: usize, prev: *mut c_void, ret: *mut usize) -> i32;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn CreateProcessW(app: *const u16, cmd: *mut u16, pa: *const c_void, ta: *const c_void, inherit: i32, flags: u32, env: *const c_void, cwd: *const u16, si: *const StartupInfoExW, pi: *mut ProcessInformation) -> i32;
    fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
    fn TerminateProcess(h: Handle, code: u32) -> i32;
    fn GetProcessTimes(h: Handle, c: *mut u64, e: *mut u64, k: *mut u64, u: *mut u64) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
    fn Process32FirstW(snap: Handle, e: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snap: Handle, e: *mut ProcessEntry32W) -> i32;
    fn GetConsoleMode(h: Handle, m: *mut u32) -> i32;
    fn SetConsoleMode(h: Handle, m: u32) -> i32;
    fn WriteConsoleW(h: Handle, buf: *const u16, n: u32, written: *mut u32, res: *const c_void) -> i32;
    fn ReadConsoleW(h: Handle, buf: *mut u16, n: u32, read: *mut u32, ctl: *const c_void) -> i32;
    fn SetConsoleOutputCP(cp: u32) -> i32;
    fn GetLastError() -> u32;
    fn AttachConsole(pid: u32) -> i32;
}

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_ACCESS_INBOUND: u32 = 1;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 8;
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
const STARTF_USESTDHANDLES: u32 = 0x100;
const ENABLE_PROCESSED_OUTPUT: u32 = 1;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 4;
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x200;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

type CreatePc = unsafe extern "system" fn(Coord, Handle, Handle, u32, *mut isize) -> i32;
type ClosePc = unsafe extern "system" fn(isize);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[derive(Clone, Debug)]
pub struct Opts {
    host: String,
    pipe: String,
    quota: u32,
    read: usize,
    sink: String,
    mb: u64,
    chunk: usize,
    api: String,
    vt: bool,
    kind: String,
    runs: usize,
    keys: usize,
    secs: f64,
    out: Option<PathBuf>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            host: "bundled".into(),
            pipe: "anon".into(),
            quota: 128 * 1024,
            read: 128 * 1024,
            sink: "null".into(),
            mb: 64,
            chunk: 64 * 1024,
            api: "file".into(),
            vt: true,
            kind: "ascii".into(),
            runs: 1,
            keys: 300,
            secs: 10.0,
            out: None,
        }
    }
}

fn parse(args: &[String]) -> Res<Opts> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let mut v = || it.next().cloned().ok_or_else(|| format!("{k} needs a value"));
        match k.as_str() {
            "--host" => o.host = v()?,
            "--pipe" => o.pipe = v()?,
            "--quota" => o.quota = v()?.parse()?,
            "--read" => o.read = v()?.parse()?,
            "--sink" => o.sink = v()?,
            "--mb" => o.mb = v()?.parse()?,
            "--chunk" => o.chunk = v()?.parse()?,
            "--api" => o.api = v()?,
            "--vt" => o.vt = v()? != "0",
            "--kind" => o.kind = v()?,
            "--runs" => o.runs = v()?.parse()?,
            "--keys" => o.keys = v()?.parse()?,
            "--secs" => o.secs = v()?.parse()?,
            "--out" => o.out = Some(PathBuf::from(v()?)),
            _ => return Err(format!("unknown option {k}").into()),
        }
    }
    if !["anon", "named", "overlapped"].contains(&o.pipe.as_str()) {
        return Err("--pipe is anon, named or overlapped".into());
    }
    Ok(o)
}

pub fn run(args: &[String]) -> Res<()> {
    let (sub, rest) = args.split_first().ok_or("usage: proteus conhost flood|echo|idle|emit [options]")?;
    let o = parse(rest)?;
    match sub.as_str() {
        "emit" => emit(&o),
        "flood" => {
            for _ in 0..o.runs.max(1) {
                let r = flood(&o)?;
                say(&r.to_string());
            }
            Ok(())
        }
        "echo" => {
            let r = echo(&o)?;
            say(&r.to_string());
            Ok(())
        }
        "idle" => {
            let r = idle(&o)?;
            say(&r.to_string());
            Ok(())
        }
        _ => Err(format!("unknown conhost mode {sub}").into()),
    }
}

/// Print a result line to whoever started us, and to the log file if asked.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

// ---------------------------------------------------------------- the child

/// The bytes a flood writes: 80-column lines, plain or coloured.
pub fn pattern(kind: &str, size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 128);
    let mut line = 0usize;
    while out.len() < size {
        match kind {
            "sgr" => {
                // Six coloured words per line, as a compiler or a test runner prints.
                for w in 0..6 {
                    let c = 1 + (line + w) % 6;
                    out.extend_from_slice(format!("\x1b[3{c}mword{:02}\x1b[0m ", (line + w) % 100).as_bytes());
                }
                out.extend_from_slice(b"\x1b[1mbold\x1b[22m tail\n");
            }
            _ => {
                for i in 0..79 {
                    out.push(b'!' + ((line + i) % 94) as u8);
                }
                out.push(b'\n');
            }
        }
        line += 1;
    }
    out.truncate(size);
    out
}

fn emit(o: &Opts) -> Res<()> {
    // The parent is the cmd.exe that `start` ran in, whose console is the
    // pseudo-console under test (see `start`). main() skips its own attach
    // for this mode so that nothing else is borrowed by accident.
    unsafe { AttachConsole(u32::MAX) };
    let open = |name: &str| unsafe { CreateFileW(wide(name).as_ptr(), GENERIC_READ | GENERIC_WRITE, 3, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    let con = open("CONOUT$");
    if con == INVALID {
        return Err(format!("no console ({})", unsafe { GetLastError() }).into());
    }
    let mut mode = 0u32;
    unsafe {
        GetConsoleMode(con, &mut mode);
        let m = if o.vt { mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING } else { mode & !ENABLE_VIRTUAL_TERMINAL_PROCESSING };
        SetConsoleMode(con, m);
        SetConsoleOutputCP(65001);
    }
    let out = o.out.clone().ok_or("emit needs --out")?;
    if o.keys > 0 && o.api == "echo" {
        return emit_echo(con, &out);
    }
    if o.api == "idle" {
        std::fs::write(&out, "ready")?;
        std::thread::sleep(Duration::from_secs_f64(o.secs));
        return Ok(());
    }
    let total = o.mb * 1_000_000;
    let chunk = pattern(&o.kind, o.chunk.max(1));
    let chunk16: Vec<u16> = String::from_utf8_lossy(&chunk).encode_utf16().collect();
    let mut sent = 0u64;
    let mut calls = 0u64;
    let t = Instant::now();
    while sent < total {
        let ok = if o.api == "wide" {
            let mut w = 0u32;
            unsafe { WriteConsoleW(con, chunk16.as_ptr(), chunk16.len() as u32, &mut w, std::ptr::null()) }
        } else {
            let mut w = 0u32;
            unsafe { WriteFile(con, chunk.as_ptr(), chunk.len() as u32, &mut w, std::ptr::null_mut()) }
        };
        if ok == 0 {
            return Err(format!("write failed ({})", unsafe { GetLastError() }).into());
        }
        sent += chunk.len() as u64;
        calls += 1;
    }
    let secs = t.elapsed().as_secs_f64();
    let r = serde_json::json!({ "bytes": sent, "calls": calls, "secs": secs, "mb_s": sent as f64 / secs / 1e6 });
    std::fs::write(&out, r.to_string())?;
    Ok(())
}

/// Read keys one at a time and write each straight back.
fn emit_echo(con: Handle, out: &Path) -> Res<()> {
    let cin = unsafe { CreateFileW(wide("CONIN$").as_ptr(), GENERIC_READ | GENERIC_WRITE, 3, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
    if cin == INVALID {
        return Err("no console input".into());
    }
    unsafe { SetConsoleMode(cin, ENABLE_VIRTUAL_TERMINAL_INPUT) };
    std::fs::write(out, "ready")?;
    let mut buf = [0u16; 64];
    loop {
        let mut n = 0u32;
        if unsafe { ReadConsoleW(cin, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null()) } == 0 || n == 0 {
            return Ok(());
        }
        if buf[..n as usize].contains(&(b'q' as u16)) {
            return Ok(());
        }
        let mut w = 0u32;
        unsafe { WriteConsoleW(con, buf.as_ptr(), n, &mut w, std::ptr::null()) };
    }
}

// ---------------------------------------------------------------- the host side

struct Host {
    create: CreatePc,
    close: ClosePc,
    dir: PathBuf,
}

fn load_host(which: &str) -> Res<Host> {
    let dir = if which == "bundled" { crate::runtime::conpty_dir().ok_or("could not unpack the bundled console host")? } else { PathBuf::from(which) };
    let dll = dir.join("conpty.dll");
    if !dll.is_file() || !dir.join("OpenConsole.exe").is_file() {
        return Err(format!("{} must hold conpty.dll and OpenConsole.exe", dir.display()).into());
    }
    let m = unsafe { LoadLibraryW(wide_path(&dll).as_ptr()) };
    if m.is_null() {
        return Err(format!("could not load {}", dll.display()).into());
    }
    let create = unsafe { GetProcAddress(m, b"CreatePseudoConsole\0".as_ptr()) };
    let close = unsafe { GetProcAddress(m, b"ClosePseudoConsole\0".as_ptr()) };
    if create.is_null() || close.is_null() {
        return Err("conpty.dll lacks CreatePseudoConsole".into());
    }
    Ok(Host { create: unsafe { std::mem::transmute::<*const c_void, CreatePc>(create) }, close: unsafe { std::mem::transmute::<*const c_void, ClosePc>(close) }, dir })
}

/// The output pipe: (our read end, the console's write end).
fn output_pipe(kind: &str, quota: u32) -> Res<(Handle, Handle)> {
    if kind == "anon" {
        let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
        if unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 0) } == 0 {
            return Err("CreatePipe failed".into());
        }
        return Ok((r, w));
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let name = format!(r"\\.\pipe\proteus-conhost-{}-{}-{nanos}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
    let w = wide(&name);
    // One instance, created first by us, local only: nobody else can take it.
    let server = unsafe { CreateNamedPipeW(w.as_ptr(), PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_REJECT_REMOTE_CLIENTS, 1, quota, quota, 0, std::ptr::null()) };
    if server == INVALID {
        return Err(format!("CreateNamedPipeW failed ({})", unsafe { GetLastError() }).into());
    }
    let flags = if kind == "overlapped" { FILE_FLAG_OVERLAPPED } else { 0 };
    let client = unsafe { CreateFileW(w.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, flags, std::ptr::null_mut()) };
    if client == INVALID {
        unsafe { CloseHandle(server) };
        return Err(format!("opening the pipe's write end failed ({})", unsafe { GetLastError() }).into());
    }
    Ok((server, client))
}

struct Session {
    host: Host,
    hpc: isize,
    input: Handle,
    child: ProcessInformation,
    reader: Option<std::thread::JoinHandle<Read>>,
    seen: Arc<Mutex<Vec<(Instant, Vec<u8>)>>>,
    keep: Arc<AtomicBool>,
    started: Instant,
}

#[derive(Default)]
struct Read {
    bytes: u64,
    first: Option<Instant>,
    last: Option<Instant>,
}

unsafe impl Send for Session {}

fn start(o: &Opts, child_args: &[String], keep_chunks: bool) -> Res<Session> {
    let host = load_host(&o.host)?;
    let (in_read, in_write) = {
        let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
        if unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 0) } == 0 {
            return Err("CreatePipe failed".into());
        }
        (r, w)
    };
    let (out_read, out_write) = output_pipe(&o.pipe, o.quota)?;
    let mut hpc = 0isize;
    let hr = unsafe { (host.create)(Coord { x: 120, y: 30 }, in_read, out_write, 0, &mut hpc) };
    unsafe {
        CloseHandle(in_read);
        CloseHandle(out_write);
    }
    if hr != 0 {
        return Err(format!("CreatePseudoConsole failed (0x{hr:08x})").into());
    }

    // The reader: as fast as the pipe gives, into nothing or into the parser.
    // It answers the console host's start-up device-attributes question the
    // way the terminal does, or 1.24 waits up to 3 s for it.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let keep = Arc::new(AtomicBool::new(keep_chunks));
    let (seen2, keep2) = (seen.clone(), keep.clone());
    let parse = o.sink == "parse";
    let read_size = o.read.max(4096);
    let (out_addr, in_addr) = (out_read as usize, in_write as usize);
    let reader = std::thread::spawn(move || {
        let term = parse.then(|| crate::term::Terminal::new(120, 30, crate::theme::Theme::void_light(), Arc::new(|_| {}), None));
        let mut buf = vec![0u8; read_size];
        let mut r = Read::default();
        let mut answered = false;
        let mut head: Vec<u8> = Vec::new();
        loop {
            let mut n = 0u32;
            if unsafe { ReadFile(out_addr as Handle, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) } == 0 || n == 0 {
                break;
            }
            let now = Instant::now();
            let got = &buf[..n as usize];
            if !answered && head.len() < 4096 {
                head.extend_from_slice(got);
                if head.windows(3).any(|w| w == b"\x1b[c") {
                    let reply = b"\x1b[?61;4c";
                    let mut w = 0u32;
                    unsafe { WriteFile(in_addr as Handle, reply.as_ptr(), reply.len() as u32, &mut w, std::ptr::null_mut()) };
                    answered = true;
                }
            }
            r.first.get_or_insert(now);
            r.last = Some(now);
            r.bytes += n as u64;
            if let Some(t) = &term {
                t.feed(got);
            }
            if keep2.load(Ordering::Relaxed) {
                if let Ok(mut s) = seen2.lock() {
                    s.push((now, got.to_vec()));
                }
            }
        }
        unsafe { CloseHandle(out_addr as Handle) };
        r
    });

    // The child, attached to the pseudo-console and to nothing of ours.
    let exe = std::env::current_exe()?;
    // Through cmd, as the probe tests do: this exe may be a windowed
    // program, which gets no console of its own; it attaches to cmd's, which
    // is the pseudo-console. `start /wait /b` keeps cmd waiting for it.
    let mut cmd = format!("cmd.exe /d /c start \"\" /wait /b \"{}\" conhost emit", exe.display());
    for a in child_args {
        cmd.push(' ');
        cmd.push_str(&format!("\"{a}\""));
    }
    let mut cmdw = wide(&cmd);
    let mut size = 0usize;
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
    let mut attrs = vec![0u8; size];
    let list = attrs.as_mut_ptr() as *mut c_void;
    let mut child = ProcessInformation::default();
    let ok = unsafe {
        InitializeProcThreadAttributeList(list, 1, 0, &mut size) != 0
            && UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, hpc as *const c_void, std::mem::size_of::<isize>(), std::ptr::null_mut(), std::ptr::null_mut()) != 0
    };
    if !ok {
        unsafe { (host.close)(hpc) };
        return Err("could not attach the pseudo-console attribute".into());
    }
    let mut si: StartupInfoExW = unsafe { std::mem::zeroed() };
    si.si.cb = std::mem::size_of::<StartupInfoExW>() as u32;
    si.si.flags = STARTF_USESTDHANDLES;
    si.attrs = list;
    let started = Instant::now();
    let made = unsafe { CreateProcessW(std::ptr::null(), cmdw.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, EXTENDED_STARTUPINFO_PRESENT, std::ptr::null(), std::ptr::null(), &si, &mut child) };
    unsafe { DeleteProcThreadAttributeList(list) };
    if made == 0 {
        unsafe { (host.close)(hpc) };
        return Err(format!("CreateProcessW failed ({})", unsafe { GetLastError() }).into());
    }
    Ok(Session { host, hpc, input: in_write, child, reader: Some(reader), seen, keep, started })
}

impl Session {
    fn wait_child(&self, limit: Duration) -> bool {
        let ms = limit.as_millis().min(u32::MAX as u128 - 1) as u32;
        if unsafe { WaitForSingleObject(self.child.process as Handle, ms) } == 0 {
            return true;
        }
        unsafe { TerminateProcess(self.child.process as Handle, 1) };
        false
    }

    fn send(&self, bytes: &[u8]) {
        let mut w = 0u32;
        unsafe { WriteFile(self.input, bytes.as_ptr(), bytes.len() as u32, &mut w, std::ptr::null_mut()) };
    }

    /// Close the pseudo-console and collect what the reader saw.
    fn finish(mut self) -> Read {
        unsafe {
            (self.host.close)(self.hpc);
            CloseHandle(self.input);
            CloseHandle(self.child.process as Handle);
            CloseHandle(self.child.thread as Handle);
        }
        self.keep.store(false, Ordering::Relaxed);
        self.reader.take().and_then(|r| r.join().ok()).unwrap_or_default()
    }

    /// OpenConsole.exe started for this session: a child of this process.
    fn console_host_pid(&self) -> Option<u32> {
        let snap = unsafe { CreateToolhelp32Snapshot(2, 0) };
        if snap == INVALID {
            return None;
        }
        let mut e: ProcessEntry32W = unsafe { std::mem::zeroed() };
        e.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let me = std::process::id();
        let mut found = None;
        let mut ok = unsafe { Process32FirstW(snap, &mut e) } != 0;
        while ok {
            let len = e.exe.iter().position(|&c| c == 0).unwrap_or(e.exe.len());
            let name = String::from_utf16_lossy(&e.exe[..len]);
            if e.parent == me && name.eq_ignore_ascii_case("OpenConsole.exe") {
                found = Some(e.pid);
            }
            ok = unsafe { Process32NextW(snap, &mut e) } != 0;
        }
        unsafe { CloseHandle(snap) };
        found
    }
}

fn scratch(tag: &str) -> PathBuf {
    let d = crate::paths::data_dir().join("conhost-bench");
    let _ = std::fs::create_dir_all(&d);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    d.join(format!("{tag}-{}-{nanos}.json", std::process::id()))
}

fn wait_file(p: &Path, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

fn host_label(o: &Opts, h: &Path) -> String {
    let v = std::fs::read_to_string(h.join("VERSION.txt")).ok().map(|s| s.trim().to_string());
    v.unwrap_or_else(|| if o.host == "bundled" { "bundled".into() } else { h.display().to_string() })
}

pub fn flood(o: &Opts) -> Res<serde_json::Value> {
    let out = scratch("flood");
    let args: Vec<String> = vec![
        "--mb".into(),
        o.mb.to_string(),
        "--chunk".into(),
        o.chunk.to_string(),
        "--api".into(),
        o.api.clone(),
        "--vt".into(),
        if o.vt { "1" } else { "0" }.into(),
        "--kind".into(),
        o.kind.clone(),
        "--out".into(),
        out.display().to_string(),
    ];
    let s = start(o, &args, false)?;
    let host = host_label(o, &s.host.dir);
    let exited = s.wait_child(Duration::from_secs(300));
    let child_done = Instant::now();
    let started = s.started;
    let r = s.finish();
    let emitted: serde_json::Value = std::fs::read_to_string(&out).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(serde_json::Value::Null);
    let _ = std::fs::remove_file(&out);
    if !exited || emitted.is_null() {
        return Err(format!("the emitter did not finish (exited {exited}); bytes read {}", r.bytes).into());
    }
    // The console host writes at least every byte the program wrote (plus
    // line endings and its own sequences); fewer means the emitter wrote
    // somewhere else, and the number would be meaningless.
    let emitted_bytes = emitted["bytes"].as_u64().unwrap_or(u64::MAX);
    if r.bytes < emitted_bytes {
        return Err(format!("read {} bytes through the pseudo-console but the emitter wrote {emitted_bytes}", r.bytes).into());
    }
    let span = match (r.first, r.last) {
        (Some(a), Some(b)) => (b - a).as_secs_f64(),
        _ => 0.0,
    };
    Ok(serde_json::json!({
        "mode": "flood", "host": host, "pipe": o.pipe, "quota": o.quota, "read": o.read, "sink": o.sink,
        "api": o.api, "vt": o.vt, "kind": o.kind, "chunk": o.chunk, "mb": o.mb,
        "emit_mb_s": emitted["mb_s"], "emit_secs": emitted["secs"], "calls": emitted["calls"],
        "vt_bytes_read": r.bytes, "read_mb_s": if span > 0.0 { r.bytes as f64 / span / 1e6 } else { 0.0 },
        "spawn_to_exit_s": (child_done - started).as_secs_f64(),
        "first_byte_ms": r.first.map(|f| (f - started).as_secs_f64() * 1e3),
    }))
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// Key in, echo out: the console host's share of typing latency.
pub fn echo(o: &Opts) -> Res<serde_json::Value> {
    let out = scratch("echo");
    let args: Vec<String> = vec!["--api".into(), "echo".into(), "--keys".into(), "1".into(), "--out".into(), out.display().to_string()];
    let s = start(o, &args, true)?;
    let host = host_label(o, &s.host.dir);
    if !wait_file(&out, Duration::from_secs(10)) {
        let _ = s.wait_child(Duration::from_millis(1));
        s.finish();
        return Err("the echo child never became ready".into());
    }
    // Let start-up output settle, then drop it.
    std::thread::sleep(Duration::from_millis(300));
    const KEYS: &[u8] = b"#%&*+/=@";
    let mut lat = Vec::new();
    let mut missed = 0;
    for i in 0..o.keys {
        let k = KEYS[i % KEYS.len()];
        if let Ok(mut v) = s.seen.lock() {
            v.clear();
        }
        let t0 = Instant::now();
        s.send(&[k]);
        let end = t0 + Duration::from_secs(1);
        let mut hit = None;
        while Instant::now() < end && hit.is_none() {
            if let Ok(v) = s.seen.lock() {
                hit = v.iter().find(|(_, b)| b.contains(&k)).map(|(t, _)| *t);
            }
            if hit.is_none() {
                std::hint::spin_loop();
            }
        }
        match hit {
            Some(t) => lat.push((t - t0).as_secs_f64() * 1e6),
            None => missed += 1,
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    s.send(b"q");
    let _ = s.wait_child(Duration::from_secs(5));
    s.finish();
    let _ = std::fs::remove_file(&out);
    let n = lat.len();
    Ok(serde_json::json!({
        "mode": "echo", "host": host, "pipe": o.pipe, "keys": o.keys, "echoed": n, "missed": missed,
        "p50_us": percentile(&mut lat, 0.5), "p95_us": percentile(&mut lat, 0.95), "p99_us": percentile(&mut lat, 0.99), "max_us": percentile(&mut lat, 1.0),
    }))
}

fn cpu_ms(h: Handle) -> f64 {
    let (mut c, mut e, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
    unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) };
    (k + u) as f64 / 10_000.0
}

/// What the console host costs while the program inside it does nothing.
pub fn idle(o: &Opts) -> Res<serde_json::Value> {
    let out = scratch("idle");
    let secs = o.secs.max(1.0);
    let args: Vec<String> = vec!["--api".into(), "idle".into(), "--secs".into(), format!("{}", secs + 5.0), "--out".into(), out.display().to_string()];
    let s = start(o, &args, false)?;
    let host = host_label(o, &s.host.dir);
    if !wait_file(&out, Duration::from_secs(10)) {
        s.finish();
        return Err("the idle child never became ready".into());
    }
    std::thread::sleep(Duration::from_secs(1));
    let pid = s.console_host_pid().ok_or("could not find the OpenConsole.exe this session started")?;
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        s.finish();
        return Err("could not open the console host".into());
    }
    let before = cpu_ms(h);
    let t = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(secs));
    let wall = t.elapsed().as_secs_f64();
    let after = cpu_ms(h);
    unsafe { CloseHandle(h) };
    unsafe { TerminateProcess(s.child.process as Handle, 0) };
    s.finish();
    let _ = std::fs::remove_file(&out);
    Ok(serde_json::json!({
        "mode": "idle", "host": host, "pipe": o.pipe, "secs": wall,
        "console_host_cpu_ms": after - before,
        "cpu_percent_of_one_core": (after - before) / (wall * 1e3) * 100.0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_are_exact_sized_lines() {
        for kind in ["ascii", "sgr"] {
            let p = pattern(kind, 65536);
            assert_eq!(p.len(), 65536);
            assert!(p.iter().filter(|&&b| b == b'\n').count() > 300, "{kind}");
        }
        assert!(pattern("ascii", 80).ends_with(b"\n"));
    }
}
