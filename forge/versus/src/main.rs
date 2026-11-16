//! versus: the tool behind forge/versus.ps1, the cross-terminal benchmark.
//!
//!   versus emit --kind ascii|sgr|unicode|cargo|tui|drip --report FILE.json
//!               [--bytes N] [--frames N] [--lines N] [--interval-ms MS]
//!               [--log FILE] [--t0 QPC]
//!       A console program that writes a known workload to its own stdout and
//!       times its own writes. A terminal that consumes slowly makes the
//!       console host stop taking bytes, so the writes block: the time the
//!       writer measures is the throughput the terminal imposes. The payload
//!       is built before the clock starts.
//!
//!   versus run --out FILE.json [--timeout S] [--report FILE] [--grace S]
//!              [--idle S --settle S] -- COMMAND LINE
//!       Start COMMAND LINE (a terminal, with the workload as its program)
//!       inside a fresh job object, so every process it starts is counted
//!       and only those processes are ever stopped. Records CPU seconds of
//!       the whole tree (job accounting includes processes that have
//!       exited), the peak of the summed working set (sampled every 20 ms),
//!       and the names of every process seen. `{T0}` in the command line is
//!       replaced with the QueryPerformanceCounter value taken just before
//!       the process is created. With --idle it waits --settle seconds, then
//!       measures CPU and memory over --idle seconds and closes the job.
//!
//!   versus latency ...  see latency.rs
use std::time::{Duration, Instant};

mod emit;
mod latency;
mod run;
mod sink;

pub fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut v) };
    v
}

pub fn qpf() -> i64 {
    let mut v = 0i64;
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut v) };
    v
}

/// Value of `--name` in `args`.
pub fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

pub fn opt_num<T: std::str::FromStr>(args: &[String], name: &str, d: T) -> T {
    opt(args, name).and_then(|s| s.parse().ok()).unwrap_or(d)
}

/// JSON string literal.
pub fn js(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

pub fn sleep_until(t: Instant) {
    let now = Instant::now();
    if t > now {
        std::thread::sleep(t - now);
    }
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(|s| s.as_str()) {
        Some("emit") => emit::main(&args[1..]),
        Some("run") => run::main(&args[1..]),
        Some("latency") => latency::main(&args[1..]),
        Some("echo") => latency::echo(&args[1..]),
        Some("calib") => latency::calib(&args[1..]),
        Some("sink") => sink::main(&args[1..]),
        _ => Err("usage: versus emit|run|latency|echo|calib|sink ... (see src/main.rs)".to_string()),
    };
    if let Err(e) = r {
        eprintln!("versus: {e}");
        std::process::exit(2);
    }
}
