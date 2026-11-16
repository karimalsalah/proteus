//! Measurements of the process sensor, on real sessions and real processes.
//! Run by hand, one at a time; each prints a line of JSON after "MEASURE":
//!
//!   cargo test --release -- --ignored --nocapture --test-threads=1 measure_short_lived_local
//!   cargo test --release -- --ignored --nocapture --test-threads=1 measure_short_lived_real_daemon
//!   cargo test --release -- --ignored --nocapture --test-threads=1 measure_join_race
//!   cargo test --release -- --ignored --nocapture --test-threads=1 check_window_against_the_running_daemon
//!
//! `PROTEUS_MEASURE_N` sets how many processes a burst starts (default 300),
//! or how many sessions the join race runs (default 20).
//! `PROTEUS_MEASURE_LOAD=N` runs N busy threads meanwhile. With every core
//! taken a burst of 300 takes minutes and the whole machine crawls: keep N
//! and the burst small on a machine someone is using.
//! `PROTEUS_TEST_PROCS_PRIORITY=0` runs the sensor's thread at normal
//! priority, for comparison with the priority it ships with.
//! The two that say "daemon" talk to the daemon at `PROTEUS_SOCKET`, which
//! must already be running (a built exe started with `daemon`). They refuse
//! to run unless `PROTEUS_SOCKET` and `PROTEUS_DATA` both name a place of
//! their own: the default socket is the daemon of whoever is at the keyboard.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::procs::ProcEvent;
use crate::procs_win::{stats, test_fixture, Probe};
use crate::session::{self, SessionSpec};

/// The two measurements that talk to a daemon may only ever talk to one that
/// was started for them. With `PROTEUS_SOCKET` unset the socket is the
/// default one in the person's own data folder, and the "daemon" there is the
/// one holding their live sessions: a measurement would start a session in
/// it. With `PROTEUS_DATA` unset this side's log lines go into that folder
/// too. Both must name a place of the measurement's own.
fn isolated(socket: Option<std::ffi::OsString>, data: Option<std::ffi::OsString>) -> Result<(), String> {
    for (name, value) in [("PROTEUS_SOCKET", &socket), ("PROTEUS_DATA", &data)] {
        if value.as_ref().is_none_or(|v| v.is_empty()) {
            return Err(format!("{name} is not set: this measurement would run against the daemon and data of whoever is using this machine. Start a daemon of your own first (<exe> daemon, with PROTEUS_DATA and PROTEUS_SOCKET set to a fresh temp folder) and run this with the same two variables"));
        }
    }
    // Neither may name the live data folder itself, or anything inside it.
    // (Read from the arguments only: nothing here touches a folder.)
    let lower = |p: &std::ffi::OsString| p.to_string_lossy().replace('/', "\\").to_lowercase();
    if let Some(live) = std::env::var_os("LOCALAPPDATA").map(|d| format!("{}\\proteus", lower(&d).trim_end_matches('\\'))) {
        for value in [&socket, &data].into_iter().flatten() {
            let v = lower(value);
            if v == live || v.starts_with(&format!("{live}\\")) {
                return Err("PROTEUS_DATA or PROTEUS_SOCKET points into the live data folder: use a fresh temp folder".into());
            }
        }
    }
    Ok(())
}

/// Connect to the daemon a measurement was told to use, or refuse.
fn own_daemon() -> uds_windows::UnixStream {
    if let Err(why) = isolated(std::env::var_os("PROTEUS_SOCKET"), std::env::var_os("PROTEUS_DATA")) {
        panic!("{why}");
    }
    crate::remote::connect(false).expect("start a daemon first: <exe> daemon, with PROTEUS_SOCKET and PROTEUS_DATA set")
}

/// A measurement that cannot tell whose daemon it is about to use must not
/// run: the default socket belongs to the person at the keyboard.
#[test]
fn daemon_measurements_refuse_to_run_against_the_default_daemon() {
    let some = |s: &str| Some(std::ffi::OsString::from(s));
    assert!(isolated(None, None).is_err());
    assert!(isolated(None, some("C:/tmp/x")).is_err(), "no socket named: the default one would be used");
    assert!(isolated(some("C:/tmp/x/d.sock"), None).is_err(), "no data folder named: the log would go to the live one");
    assert!(isolated(some(""), some("")).is_err(), "empty is unset");
    assert_eq!(isolated(some("C:/tmp/x/d.sock"), some("C:/tmp/x")), Ok(()));
    // The live folder is only named here, never opened.
    if let Some(live) = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("proteus")) {
        let inside = live.join("proteusd-3.sock").into_os_string();
        assert!(isolated(Some(inside), some("C:/tmp/x")).is_err(), "a socket inside the live data folder is the live daemon's");
        assert!(isolated(some("C:/tmp/x/d.sock"), Some(live.into_os_string())).is_err(), "the live data folder itself");
    }
    // The folder every test of this crate runs in is one of its own.
    let dir = test_fixture();
    assert_eq!(isolated(std::env::var_os("PROTEUS_SOCKET"), std::env::var_os("PROTEUS_DATA")), Ok(()), "{}", dir.display());
}

fn spec(exe: &str, args: &[&str]) -> SessionSpec {
    SessionSpec { exe: exe.into(), args: args.iter().map(|a| a.to_string()).collect(), cwd: None, cols: 100, rows: 30, journal: false, env: None }
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// p50 / p95 / max of a set of microsecond values.
fn spread(mut v: Vec<u64>) -> Value {
    v.sort_unstable();
    let at = |p: f64| v.get(((v.len() as f64 - 1.0) * p).round().max(0.0) as usize).copied();
    json!({ "n": v.len(), "p50_us": at(0.5), "p95_us": at(0.95), "max_us": v.last() })
}

/// Busy threads for as long as the returned guard lives.
struct Load(Arc<AtomicBool>);
impl Load {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        for _ in 0..env_num("PROTEUS_MEASURE_LOAD", 0) {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut x = 1u64;
                while !stop.load(Ordering::Relaxed) {
                    x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
                }
            });
        }
        Load(stop)
    }
}
impl Drop for Load {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// In an interactive `cmd` session, start `n` processes one after another as
/// fast as the shell can (`cmd /c exit 1` ... `cmd /c exit n`), and report
/// what the sensor made of them.
fn burst(p: &mut Probe, n: u64) -> Value {
    assert!(p.until(30, |p| p.said(b">")), "no prompt");
    let before = stats();
    let load = Load::start();
    let t0 = Instant::now();
    p.send(&format!("for /l %i in (1,1,{n}) do @cmd /c exit %i\r"));
    // The caret is dropped by cmd when it runs the line, so this text only
    // appears once the loop has finished and the echo has run.
    p.send("echo burst-^over\r");
    assert!(p.until(600, |p| p.said(b"burst-over")), "the burst never finished");
    let wall = t0.elapsed();
    p.until(1, |_| false); // let the last events arrive
    drop(load);

    // Everything but the shell itself was started by the loop.
    let shell = p.ledger.procs()[0].id;
    let kids: Vec<_> = p.ledger.procs().iter().filter(|k| k.id != shell).collect();
    // The number each child was told to exit with, where its command line is known.
    let wanted = |cmd: &Option<String>| cmd.as_deref().and_then(|c| c.trim().rsplit(' ').next()?.parse::<u32>().ok());
    let right = kids.iter().filter(|k| wanted(&k.cmd).is_some() && wanted(&k.cmd) == k.exit_code()).count();
    let false_259 = kids.iter().filter(|k| k.exit_code() == Some(259) && wanted(&k.cmd) != Some(259)).count();
    let exit_lat: Vec<u64> = p
        .events
        .iter()
        .filter_map(|e| match e {
            ProcEvent::Exit(x) => Some(x.seen_us.saturating_sub(x.end_us?)),
            _ => None,
        })
        .collect();
    let after = stats();
    let delta = |k: &str| after[k].as_u64().unwrap_or(0) - before[k].as_u64().unwrap_or(0);
    json!({
        "started_by_the_loop": n,
        "seen": kids.len(),
        "with_command_line": kids.iter().filter(|k| k.cmd.is_some()).count(),
        "with_image_path": kids.iter().filter(|k| k.image.is_some()).count(),
        "with_parent": kids.iter().filter(|k| k.parent == Some(shell)).count(),
        "with_exit_code": kids.iter().filter(|k| k.exit_code().is_some()).count(),
        "exit_code_is_its_own_n": right,
        "false_259": false_259,
        "with_exit_time_and_cpu": kids.iter().filter(|k| k.end.as_ref().is_some_and(|e| e.end_exact && e.user_us.is_some())).count(),
        "still_running": kids.iter().filter(|k| k.running()).count(),
        "spawn_to_recorded": spread(kids.iter().filter_map(|k| k.latency_us()).collect()),
        "exit_to_recorded": spread(exit_lat),
        "wall_ms": wall.as_millis() as u64,
        "ms_per_process": wall.as_secs_f64() * 1000.0 / n as f64,
        "busy_threads": env_num("PROTEUS_MEASURE_LOAD", 0),
        "sensor": { "unopened": delta("unopened"), "no_cmd": delta("no_cmd"), "no_image": delta("no_image"), "exit_waited": delta("exit_waited"),
                    "exit_wait_max_us": after["exit_wait_max_us"], "exit_unknown": delta("exit_unknown"), "exit_recovered": delta("exit_recovered"), "dropped": delta("dropped") },
    })
}

#[test]
#[ignore]
fn measure_short_lived_local() {
    test_fixture();
    let mut p = Probe::new(session::spawn_local(spec("cmd.exe", &[])).unwrap());
    let r = burst(&mut p, env_num("PROTEUS_MEASURE_N", 300));
    println!("MEASURE short_lived_local {r}");
    p.send("exit\r");
    p.until(10, |p| p.exit.is_some());
}

#[test]
#[ignore]
fn measure_short_lived_real_daemon() {
    // No fixture: this talks to the daemon PROTEUS_SOCKET names, and to no other.
    let stream = own_daemon();
    let (h, _) = crate::remote::open(stream, &spec("cmd.exe", &[]), false).unwrap();
    let mut p = Probe::new(h);
    let mut r = burst(&mut p, env_num("PROTEUS_MEASURE_N", 300));
    let list = crate::remote::list().unwrap();
    r["daemon_pid"] = list["pid"].clone();
    r["daemon_build"] = list["build"].clone();
    r["session"] = list["sessions"].as_array().and_then(|a| a.last()).map(|s| s["id"].clone()).unwrap_or(Value::Null);
    // The sensor's own counters live in the daemon; this process has none.
    r.as_object_mut().unwrap().remove("sensor");
    println!("MEASURE short_lived_real_daemon {r}");
    // The session is left running in the daemon, idle, for the idle-cost measurement.
}

/// This build's window code against whatever daemon is at `PROTEUS_SOCKET`,
/// for checking it against a daemon built before process sensing existed:
/// the session must work as ever, with process events simply absent.
#[test]
#[ignore]
fn check_window_against_the_running_daemon() {
    let stream = own_daemon();
    let (h, _) = crate::remote::open(stream, &spec("cmd.exe", &[]), false).unwrap();
    let mut p = Probe::new(h);
    let prompt = p.until(30, |p| p.said(b">"));
    p.send("cmd /c exit 4\r");
    p.send("echo check-^over\r");
    let ran = p.until(30, |p| p.said(b"check-over"));
    p.until(1, |_| false);
    // With PROTEUS_MEASURE_N: time the same burst of processes as
    // `measure_short_lived_*`, so a daemon that senses and one that does
    // not can be compared for what the sensing costs the shell.
    let n = env_num("PROTEUS_MEASURE_N", 0);
    let t0 = Instant::now();
    if n > 0 {
        p.send(&format!("for /l %i in (1,1,{n}) do @cmd /c exit %i\r"));
        p.send("echo loop-^over\r");
        assert!(p.until(600, |p| p.said(b"loop-over")));
    }
    let loop_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let list = crate::remote::list().unwrap();
    let journal = list["sessions"].as_array().and_then(|a| a.last()).map(|s| s["journal"].clone());
    p.send("exit\r");
    let ended = p.until(30, |p| p.exit.is_some());
    println!(
        "MEASURE window_against_daemon {}",
        json!({ "daemon_build": list["build"], "prompt": prompt, "command_ran": ran, "process_events": p.events.len(),
                "of_which_arrived_as_replayed_history": p.replayed,
                "session_ended_with": p.exit, "ended": ended, "journal": journal,
                "loop_processes": n, "loop_ms": loop_ms, "loop_ms_per_process": if n > 0 { loop_ms / n as f64 } else { 0.0 } })
    );
}

/// One session whose shell starts a child as early as a shell can:
/// `cmd /c "cmd /c exit 7"`. Returns (child seen, shell seen, microseconds
/// from the shell's creation to its child's).
fn early_child() -> (bool, bool, Option<u64>) {
    let mut p = Probe::new(session::spawn_local(spec("cmd.exe", &["/c", "cmd /c exit 7"])).unwrap());
    assert!(p.until(60, |p| p.exit.is_some()));
    let procs = p.ledger.procs();
    let gap = (procs.len() >= 2).then(|| procs[1].start_us.saturating_sub(procs[0].start_us));
    (procs.len() >= 2, !procs.is_empty(), gap)
}

/// The join race. The shell is put in its job just after Windows has created
/// it; a child the shell starts before that would never be seen. How long
/// may the join be late before that happens?
#[test]
#[ignore]
fn measure_join_race() {
    test_fixture();
    let runs = env_num("PROTEUS_MEASURE_N", 20);
    // As shipped: no artificial delay.
    let _load = Load::start();
    let (mut lags, mut calls, mut gaps, mut seen) = (Vec::new(), Vec::new(), Vec::new(), 0);
    for _ in 0..runs {
        let (child, _, gap) = early_child();
        seen += child as u64;
        gaps.extend(gap);
        let s = stats();
        lags.extend(s["join_lag_last_us"].as_u64());
        calls.extend(s["join_call_last_us"].as_u64());
    }
    let natural = json!({
        "sessions": runs, "child_seen": seen,
        "shell_created_to_joined": spread(lags), "of_which_the_join_call": spread(calls),
        "shell_created_to_first_child_created": spread(gaps.clone()), "earliest_first_child_us": gaps.iter().min(),
    });
    // Then with the join held back on purpose.
    let mut forced = Vec::new();
    for delay in [100u64, 1000, 2500, 3500, 5000] {
        crate::procs_win::JOIN_LATE_MS.set(delay);
        let n = (runs / 4).max(3);
        let (mut child_seen, mut shell_seen) = (0, 0);
        for _ in 0..n {
            let (c, s, _) = early_child();
            child_seen += c as u64;
            shell_seen += s as u64;
        }
        forced.push(json!({ "join_late_by_ms": delay, "sessions": n, "shell_seen": shell_seen, "child_seen": child_seen }));
    }
    crate::procs_win::JOIN_LATE_MS.set(0);
    println!("MEASURE join_race {}", json!({ "as_shipped": natural, "join_held_back": forced, "busy_threads": env_num("PROTEUS_MEASURE_LOAD", 0) }));
}
