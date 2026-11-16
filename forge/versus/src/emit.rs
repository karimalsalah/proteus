//! The workload: writes to its own stdout and times the writes.
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleMode, SetConsoleOutputCP, CONSOLE_SCREEN_BUFFER_INFO,
    ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::{js, ms, opt, opt_num, percentile, qpc, qpf, sleep_until};

const CHUNK: usize = 64 * 1024;

fn write_all(h: HANDLE, mut b: &[u8]) -> Result<(), String> {
    while !b.is_empty() {
        let mut n = 0u32;
        let ok = unsafe { WriteFile(h, b.as_ptr(), b.len().min(1 << 30) as u32, &mut n, std::ptr::null_mut()) };
        if ok == 0 || n == 0 {
            return Err(format!("WriteFile failed: {}", std::io::Error::last_os_error()));
        }
        b = &b[n as usize..];
    }
    Ok(())
}

/// One synthetic line; `i` varies it so neighbours differ.
pub fn line(kind: &str, i: usize) -> String {
    match kind {
        // Colour changes inside the line: 16, 256 and 24-bit, bold, dim,
        // underline, inverse badges, as in a coloured build or test log.
        "sgr" => format!(
            "\x1b[2m{i:07}\x1b[0m \x1b[3{}mquick brown fox\x1b[0m \x1b[38;5;{}mjumps over\x1b[0m \x1b[1;38;2;{};{};{}mthe lazy dog\x1b[0m \x1b[4m0123456789\x1b[24m {}\x1b[0m\r\n",
            1 + i % 6,
            16 + i % 216,
            (i * 7) % 256,
            (i * 13) % 256,
            (i * 29) % 256,
            if i % 3 == 0 { "\x1b[41;97m FAIL \x1b[0m" } else { "\x1b[42;30m pass \x1b[0m" }
        ),
        // Two-, three- and four-byte UTF-8, wide CJK, emoji, box and blocks.
        "unicode" => format!("{i:07} ünïcødé → λ∑∫ ┌─┬─┐ 日本語のテキスト 中文字符 한국어 🙂🚀 ░▒▓█ ═║╔╗\r\n"),
        _ => format!("{i:07} the quick brown fox jumps over the lazy dog 0123456789 abcdefghijklmnopqrstuvwxyz\r\n"),
    }
}

fn lines_payload(kind: &str, bytes: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes as usize + 256);
    let mut i = 0;
    while (out.len() as u64) < bytes {
        out.extend_from_slice(line(kind, i).as_bytes());
        i += 1;
    }
    out
}

/// A full-screen frame for a cols x rows screen: every row addressed with
/// CUP and every cell rewritten, colours shifting with the frame number,
/// like a TUI repainting (htop, a full-screen agent UI).
fn tui_frame(f: usize, cols: usize, rows: usize) -> Vec<u8> {
    let mut o = String::with_capacity(rows * (cols + cols / 8 * 14) + 64);
    let glyphs = b"abcdefghijklmnopqrstuvwxyz0123456789#=-+*%@";
    for r in 0..rows {
        o.push_str(&format!("\x1b[{};1H", r + 1));
        let mut c = 0;
        while c < cols {
            let seg = (c / 8 + r + f) % 7;
            o.push_str(&format!("\x1b[38;5;{};48;5;{}m", 17 + (seg * 31 + f) % 214, 232 + (r + f) % 24));
            for k in 0..8.min(cols - c) {
                o.push(glyphs[(c + k + r * 3 + f) % glyphs.len()] as char);
            }
            c += 8;
        }
        o.push_str("\x1b[0m");
    }
    o.into_bytes()
}

/// Whether this process is inside the runner's job (named in VERSUS_JOB),
/// so the runner knows whether its CPU total includes the workload's own.
fn in_runner_job() -> Option<bool> {
    use windows_sys::Win32::System::JobObjects::{IsProcessInJob, OpenJobObjectW};
    const JOB_OBJECT_QUERY: u32 = 4; // winnt.h
    let name = std::env::var("VERSUS_JOB").ok()?;
    let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let j = OpenJobObjectW(JOB_OBJECT_QUERY, 0, w.as_ptr());
        if j.is_null() {
            return None;
        }
        let mut b = 0;
        let ok = IsProcessInJob(GetCurrentProcess(), j, &mut b);
        windows_sys::Win32::Foundation::CloseHandle(j);
        (ok != 0).then_some(b != 0)
    }
}

/// Bytes written when the chunk that crossed `half` finished.
fn half_at(p: &[u8], half: usize) -> usize {
    p.len().min(half.div_ceil(CHUNK) * CHUNK)
}

fn self_cpu_s() -> f64 {
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut a, mut b, mut k, mut u) = (z, z, z, z);
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u) };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    t(k) + t(u)
}

pub fn main(args: &[String]) -> Result<(), String> {
    let t_start = qpc();
    let kind = opt(args, "--kind").unwrap_or("ascii").to_string();
    let report = opt(args, "--report").ok_or("--report FILE is required")?.to_string();
    let bytes: u64 = opt_num(args, "--bytes", 16 << 20);
    let frames: usize = opt_num(args, "--frames", 600);
    let lines: usize = opt_num(args, "--lines", 500);
    let interval = Duration::from_micros((opt_num(args, "--interval-ms", 10.0f64) * 1000.0) as u64);
    let t0: i64 = opt_num(args, "--t0", 0);

    let h = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let mut mode = 0u32;
    let console = unsafe { GetConsoleMode(h, &mut mode) } != 0;
    let (mut cols, mut rows) = (0usize, 0usize);
    if console {
        unsafe {
            SetConsoleOutputCP(65001);
            SetConsoleMode(h, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(h, &mut info) != 0 {
                cols = (info.srWindow.Right - info.srWindow.Left + 1) as usize;
                rows = (info.srWindow.Bottom - info.srWindow.Top + 1) as usize;
            }
        }
    }
    let (tc, tr) = (if cols == 0 { 120 } else { cols }, if rows == 0 { 40 } else { rows });

    // Everything is prepared before the clock starts.
    let mut payload: Vec<u8> = Vec::new();
    let mut tui: Vec<Vec<u8>> = Vec::new();
    let mut drip: Vec<Vec<u8>> = Vec::new();
    match kind.as_str() {
        "ascii" | "sgr" | "unicode" => payload = lines_payload(&kind, bytes),
        "cargo" => {
            let log = std::fs::read(opt(args, "--log").ok_or("--log FILE is required for cargo")?).map_err(|e| e.to_string())?;
            if log.is_empty() {
                return Err("empty log".into());
            }
            while (payload.len() as u64) < bytes {
                payload.extend_from_slice(&log);
            }
        }
        "tui" => tui = (0..frames).map(|f| tui_frame(f, tc, tr)).collect(),
        "drip" => drip = (0..lines).map(|i| line("ascii", i).into_bytes()).collect(),
        k => return Err(format!("unknown kind {k}")),
    }

    write_all(h, format!("versus {kind} {tc}x{tr}\r\n").as_bytes())?;
    let t_first = qpc();
    if kind == "tui" {
        write_all(h, b"\x1b[?1049h\x1b[?25l")?;
    }

    let mut extra = String::new();
    // Marks the start of the payload for the runner, which snapshots the
    // job's CPU here and again when the report appears: CPU of the payload
    // alone, without start-up or shut-down.
    let _ = std::fs::write(format!("{report}.start"), "1");
    let cpu_at_marker = self_cpu_s();
    let p0 = qpc();
    let w0 = Instant::now();
    let total: u64;
    match kind.as_str() {
        "tui" => {
            let mut n = 0u64;
            for f in &tui {
                write_all(h, f)?;
                n += f.len() as u64;
            }
            total = n;
        }
        "drip" => {
            // One line every `interval` on an absolute schedule. A write that
            // blocks shows up as write time and as lateness.
            let mut wt = Vec::with_capacity(drip.len());
            let mut late = Vec::with_capacity(drip.len());
            let mut n = 0u64;
            for (i, l) in drip.iter().enumerate() {
                let due = w0 + interval * i as u32;
                sleep_until(due);
                let a = Instant::now();
                late.push(ms(a.saturating_duration_since(due)));
                write_all(h, l)?;
                wt.push(ms(a.elapsed()));
                n += l.len() as u64;
            }
            wt.sort_by(|a, b| a.partial_cmp(b).unwrap());
            late.sort_by(|a, b| a.partial_cmp(b).unwrap());
            extra = format!(
                ",\"write_ms\":{{\"p50\":{:.4},\"p99\":{:.4},\"max\":{:.4}}},\"late_ms\":{{\"p50\":{:.4},\"p99\":{:.4},\"max\":{:.4}}}",
                percentile(&wt, 0.5),
                percentile(&wt, 0.99),
                percentile(&wt, 1.0),
                percentile(&late, 0.5),
                percentile(&late, 0.99),
                percentile(&late, 1.0)
            );
            total = n;
        }
        _ => {
            // The time at which half the payload had been taken: the rate
            // over the second half is the sustained one, after whatever a
            // terminal can swallow into its own buffers at the start.
            let half = payload.len() / 2;
            let mut done = 0usize;
            let mut t_half = 0i64;
            for c in payload.chunks(CHUNK) {
                write_all(h, c)?;
                done += c.len();
                if t_half == 0 && done >= half {
                    t_half = qpc();
                }
            }
            total = payload.len() as u64;
            let s2 = (qpc() - t_half) as f64 / qpf() as f64;
            extra = format!(",\"second_half_mb_per_s\":{:.4}", (total as usize - half_at(&payload, half)) as f64 / s2 / 1e6);
        }
    }
    let p1 = qpc();
    if kind == "tui" {
        write_all(h, b"\x1b[0m\x1b[?25h\x1b[?1049l")?;
    }
    let f = qpf() as f64;
    let secs = (p1 - p0) as f64 / f;
    let n_frames = if kind == "tui" { tui.len() } else { 0 };
    let json = format!(
        "{{\"kind\":{},\"console\":{},\"cols\":{},\"rows\":{},\"bytes\":{},\"seconds\":{:.6},\"mb_per_s\":{:.4},\"frames\":{},\"fps\":{:.3},\"qpf\":{},\"t0\":{},\"t_start\":{},\"t_first\":{},\"t_payload_start\":{},\"t_payload_end\":{},\"start_ms\":{:.3},\"first_write_ms\":{:.3},\"self_cpu_s\":{:.4},\"payload_self_cpu_s\":{:.4},\"in_runner_job\":{},\"pid\":{}{}}}\n",
        js(&kind),
        console,
        cols,
        rows,
        total,
        secs,
        total as f64 / secs / 1e6,
        n_frames,
        n_frames as f64 / secs,
        f as i64,
        t0,
        t_start,
        t_first,
        p0,
        p1,
        if t0 > 0 { (t_start - t0) as f64 / f * 1000.0 } else { -1.0 },
        if t0 > 0 { (t_first - t0) as f64 / f * 1000.0 } else { -1.0 },
        self_cpu_s(),
        self_cpu_s() - cpu_at_marker,
        in_runner_job().map(|b| b.to_string()).unwrap_or("null".into()),
        std::process::id(),
        extra
    );
    std::fs::write(&report, json).map_err(|e| format!("{report}: {e}"))?;
    Ok(())
}
