//! The session daemon. It owns the child processes and their journals, so a
//! window can crash, be replaced by a new build, or be closed, and the work
//! inside it carries on. A window that attaches gets the journal replayed
//! and then the live stream, with nothing lost or repeated in between.
use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, RecvTimeoutError, Sender};
use uds_windows::{UnixListener, UnixStream};

use crate::journal::{self, JournalWriter};
use crate::paths;
use crate::proto::{self, Frame};
use crate::session::{self, SessionCmd, SessionEvent, SessionSpec};
use crate::Res;

struct DState {
    journal: JournalWriter,
    clients: Vec<(u64, Sender<Frame>)>,
    exited: Option<Option<u32>>,
    cols: u16,
    rows: u16,
}

struct DSession {
    id: u64,
    exe: String,
    started: u64,
    journal_path: PathBuf,
    cmds: Sender<SessionCmd>,
    state: Mutex<DState>,
    /// Started by an agent (`proteus mcp`), not by a window: a plain launch
    /// never adopts it, and it ends when its last client has gone.
    agent: bool,
    /// An agent session asked to outlive its clients.
    linger: bool,
}

struct Daemon {
    sessions: Mutex<HashMap<u64, Arc<DSession>>>,
    next_id: AtomicU64,
    next_client: AtomicU64,
    conns: AtomicUsize,
    last_activity: Mutex<Instant>,
    /// False in tests, where the daemon shares a process with the test runner.
    may_exit: bool,
}

fn exit_payload(code: Option<u32>) -> Vec<u8> {
    code.unwrap_or(u32::MAX).to_le_bytes().to_vec()
}

fn spawn(d: &Arc<Daemon>, v: &serde_json::Value) -> io::Result<Arc<DSession>> {
    let exe = v["exe"].as_str().unwrap_or("").to_string();
    let args: Vec<String> =
        v["args"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let (exe, args) = if exe.is_empty() { session::default_shell() } else { (exe, args) };
    let cols = v["cols"].as_u64().unwrap_or(80).clamp(2, 1000) as u16;
    let rows = v["rows"].as_u64().unwrap_or(24).clamp(2, 1000) as u16;
    let cwd = v["cwd"].as_str().filter(|s| !s.is_empty()).map(PathBuf::from);

    // The window's environment, not this daemon's (which may be days old).
    let env = v["env"].as_object().filter(|m| !m.is_empty()).map(|m| {
        m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect::<Vec<_>>()
    });
    let h = session::spawn_local(SessionSpec { exe: exe.clone(), args: args.clone(), cwd, cols, rows, journal: false, env })?;
    let id = d.next_id.fetch_add(1, Ordering::Relaxed);
    let started = paths::unix_secs();
    let name = format!("{started}-{}-{id}", std::process::id());
    let journal_path = paths::journal_dir().join(format!("{name}.prtj"));
    let meta = serde_json::json!({ "v": 1, "session": name, "exe": exe, "args": args, "cols": cols, "rows": rows, "started_unix": started });
    let mut journal = JournalWriter::create(&journal_path, &meta)?;
    journal.record(journal::K_RESIZE, &session::resize_payload(cols, rows))?;

    let s = Arc::new(DSession {
        id,
        exe,
        started,
        journal_path,
        cmds: h.cmds.clone(),
        state: Mutex::new(DState { journal, clients: Vec::new(), exited: None, cols, rows }),
        agent: v["agent"].as_bool().unwrap_or(false),
        linger: v["linger"].as_bool().unwrap_or(false),
    });
    d.sessions.lock().unwrap().insert(id, s.clone());

    // Pump: one place where output is journaled and fanned out, under one
    // lock, which is what makes attach gap-free.
    let (s2, d2) = (s.clone(), d.clone());
    thread::Builder::new().name(format!("session-{id}")).spawn(move || {
        let mut held: Option<SessionEvent> = None;
        loop {
            let ev = match held.take() {
                Some(e) => Ok(e),
                None => h.events.recv_timeout(Duration::from_millis(250)),
            };
            match ev {
                Ok(SessionEvent::Output(mut b)) => {
                    // During a flood many chunks are already waiting: merge
                    // them so each costs one journal record and one frame.
                    while b.len() < 256 << 10 {
                        match h.events.try_recv() {
                            Ok(SessionEvent::Output(more)) => b.extend_from_slice(&more),
                            Ok(other) => {
                                held = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    let mut st = s2.state.lock().unwrap();
                    let _ = st.journal.record(journal::K_OUTPUT, &b);
                    st.clients.retain(|(_, tx)| tx.send((proto::D_OUTPUT, b.clone())).is_ok());
                }
                // A process under the shell started or ended: journaled and
                // fanned out under the same lock as output, so a window that
                // attaches gets each one exactly once.
                Ok(SessionEvent::Proc(ev)) => {
                    let b = ev.to_bytes();
                    let mut st = s2.state.lock().unwrap();
                    let _ = st.journal.record(journal::K_PROC, &b);
                    st.clients.retain(|(_, tx)| tx.send((proto::D_PROC, b.clone())).is_ok());
                }
                Ok(SessionEvent::Exit(code)) => {
                    let mut st = s2.state.lock().unwrap();
                    let _ = st.journal.record(journal::K_EXIT, &exit_payload(code));
                    let _ = st.journal.flush();
                    st.exited = Some(code);
                    for (_, tx) in &st.clients {
                        let _ = tx.send((proto::D_EXIT, exit_payload(code)));
                    }
                    break;
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    let _ = s2.state.lock().unwrap().journal.flush();
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        d2.sessions.lock().unwrap().remove(&s2.id);
        *d2.last_activity.lock().unwrap() = Instant::now();
        paths::log(&format!("daemon: session {} ended", s2.id));
    })?;
    paths::log(&format!("daemon: session {id} started"));
    Ok(s)
}

/// Replay the journal to a newly connected window, then add it to the live
/// fan-out. Holding the session lock throughout means no output can slip
/// between the end of the replay and the start of the live stream.
fn attach(s: &Arc<DSession>, client: u64, tx: &Sender<Frame>) {
    let mut st = s.state.lock().unwrap();
    let _ = st.journal.flush();
    // "journal" lets the window read the session's past (recall) and the
    // record times of what it is about to be replayed (world.rs); older
    // windows ignore fields they do not know.
    let info = serde_json::json!({
        "proto": proto::PROTO, "id": s.id, "exe": s.exe, "cols": st.cols, "rows": st.rows,
        "journal": s.journal_path.to_string_lossy(),
    });
    let _ = tx.send((proto::D_INFO, serde_json::to_vec(&info).unwrap_or_default()));
    if let Ok((_, recs)) = journal::read(&s.journal_path) {
        for r in recs {
            let ty = match r.kind {
                journal::K_RESIZE => proto::D_REPLAY_RESIZE,
                journal::K_OUTPUT => proto::D_OUTPUT,
                journal::K_PROC => proto::D_PROC,
                _ => continue,
            };
            let _ = tx.send((ty, r.data));
        }
    }
    let _ = tx.send((proto::D_REPLAY_DONE, Vec::new()));
    match st.exited {
        Some(code) => {
            let _ = tx.send((proto::D_EXIT, exit_payload(code)));
        }
        None => st.clients.push((client, tx.clone())),
    }
}

fn list(d: &Daemon) -> Vec<u8> {
    let sessions = d.sessions.lock().unwrap();
    let mut v: Vec<serde_json::Value> = sessions
        .values()
        .map(|s| {
            let mut st = s.state.lock().unwrap();
            // Whoever asks is about to read the journals: make them current.
            let _ = st.journal.flush();
            serde_json::json!({
                "id": s.id, "exe": s.exe, "started_unix": s.started, "windows": st.clients.len(),
                "cols": st.cols, "rows": st.rows, "journal": s.journal_path.to_string_lossy(),
                "agent": s.agent,
            })
        })
        .collect();
    v.sort_by_key(|s| s["id"].as_u64());
    serde_json::to_vec(&serde_json::json!({
        "pid": std::process::id(), "proto": proto::PROTO, "version": env!("CARGO_PKG_VERSION"),
        "build": crate::runtime::build_id(), "sessions": v,
        // What this daemon does beyond the first protocol, so a client can tell.
        "caps": ["sync", "agent"],
    }))
    .unwrap_or_default()
}

fn handle_conn(d: &Arc<Daemon>, stream: UnixStream) -> io::Result<()> {
    let mut rd = BufReader::new(stream.try_clone()?);
    let (tx, rx) = unbounded::<Frame>();
    thread::spawn(move || {
        let mut w = BufWriter::with_capacity(1 << 16, &stream);
        for (ty, p) in rx {
            if proto::write_frame(&mut w, ty, &p).is_err() {
                break;
            }
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
    let client = d.next_client.fetch_add(1, Ordering::Relaxed);
    let mut cur: Option<Arc<DSession>> = None;

    while let Ok((ty, p)) = proto::read_frame(&mut rd) {
        match ty {
            proto::C_SPAWN if cur.is_none() => {
                let v: serde_json::Value = serde_json::from_slice(&p).unwrap_or(serde_json::Value::Null);
                match spawn(d, &v) {
                    Ok(s) => {
                        attach(&s, client, &tx);
                        cur = Some(s);
                    }
                    Err(e) => {
                        let _ = tx.send((proto::D_ERROR, e.to_string().into_bytes()));
                    }
                }
            }
            proto::C_ATTACH if cur.is_none() => {
                let want = p.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
                let found = {
                    let sessions = d.sessions.lock().unwrap();
                    if want != 0 {
                        sessions.get(&want).cloned()
                    } else {
                        sessions
                            .values()
                            .filter(|s| {
                                let st = s.state.lock().unwrap();
                                st.clients.is_empty() && st.exited.is_none() && !s.agent
                            })
                            .max_by_key(|s| s.id)
                            .cloned()
                    }
                };
                match found {
                    Some(s) => {
                        attach(&s, client, &tx);
                        cur = Some(s);
                    }
                    None => {
                        let _ = tx.send((proto::D_ERROR, b"no session to attach".to_vec()));
                    }
                }
            }
            proto::C_INPUT => {
                if let Some(s) = &cur {
                    let _ = s.cmds.send(SessionCmd::Input(p));
                }
            }
            proto::C_RESIZE => {
                if let (Some(s), Some(b)) = (&cur, p.get(..4)) {
                    let cols = u16::from_le_bytes([b[0], b[1]]).max(2);
                    let rows = u16::from_le_bytes([b[2], b[3]]).max(2);
                    let mut st = s.state.lock().unwrap();
                    if (st.cols, st.rows) != (cols, rows) {
                        st.cols = cols;
                        st.rows = rows;
                        let _ = st.journal.record(journal::K_RESIZE, &session::resize_payload(cols, rows));
                        let _ = s.cmds.send(SessionCmd::Resize { cols, rows });
                    }
                }
            }
            proto::C_KILL => {
                if let Some(s) = &cur {
                    let _ = s.cmds.send(SessionCmd::Kill);
                }
            }
            proto::C_LIST => {
                let _ = tx.send((proto::D_LIST, list(d)));
            }
            proto::C_QUIT => {
                // A different build asks to take over. Only safe when nothing
                // is running; otherwise it keeps using this daemon.
                if d.may_exit && d.sessions.lock().unwrap().is_empty() {
                    paths::log("daemon: handing over to another build");
                    let _ = std::fs::remove_file(proto::socket_path());
                    std::process::exit(0);
                }
            }
            _ => {}
        }
    }
    if let Some(s) = cur {
        let mut st = s.state.lock().unwrap();
        st.clients.retain(|(id, _)| *id != client);
        // Nobody is left who could drive or watch an agent's session.
        if s.agent && !s.linger && st.clients.is_empty() && st.exited.is_none() {
            let _ = s.cmds.send(SessionCmd::Kill);
        }
    }
    Ok(())
}

/// Run the daemon. With `exit_when_idle` it leaves once it has had no
/// sessions and no windows for half a minute.
pub fn run(exit_when_idle: bool) -> Res<()> {
    let path = proto::socket_path();
    if UnixStream::connect(&path).is_ok() {
        return Ok(()); // one is already running
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    paths::log(&format!("daemon: start pid={} build={} socket={}", std::process::id(), crate::runtime::build_id(), path.display()));
    // Housekeeping happens here, once per daemon life, so nobody has to do it.
    let cfg = crate::config::Config::load();
    let pruned = journal::prune(&paths::journal_dir(), cfg.journal_days, cfg.journal_max_mb);
    if pruned > 0 {
        paths::log(&format!("daemon: removed {pruned} old journals"));
    }
    crate::runtime::sweep();
    let d = Arc::new(Daemon {
        sessions: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        next_client: AtomicU64::new(1),
        conns: AtomicUsize::new(0),
        last_activity: Mutex::new(Instant::now()),
        may_exit: exit_when_idle,
    });
    if exit_when_idle {
        let d = d.clone();
        let path = path.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(5));
            let idle = d.sessions.lock().unwrap().is_empty() && d.conns.load(Ordering::Relaxed) == 0;
            if idle && d.last_activity.lock().unwrap().elapsed() > Duration::from_secs(30) {
                paths::log("daemon: idle, exiting");
                let _ = std::fs::remove_file(&path);
                std::process::exit(0);
            }
        });
    }
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let d = d.clone();
        thread::spawn(move || {
            d.conns.fetch_add(1, Ordering::Relaxed);
            let _ = handle_conn(&d, stream);
            d.conns.fetch_sub(1, Ordering::Relaxed);
            *d.last_activity.lock().unwrap() = Instant::now();
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote;

    fn collect_until(h: &session::SessionHandle, secs: u64, stop: impl Fn(&[u8], &SessionEvent) -> bool) -> (Vec<u8>, bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut out = Vec::new();
        while let Ok(ev) = h.events.recv_deadline(deadline) {
            if let SessionEvent::Output(b) = &ev {
                out.extend_from_slice(b);
            }
            if stop(&out, &ev) {
                return (out, true);
            }
        }
        (out, false)
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    /// The property the daemon exists for: a window goes away, the session
    /// keeps running, and the next window gets everything back.
    /// One daemon for every test here, in the data folder all tests share
    /// (`procs_win::test_fixture`). Holding the guard also takes these tests
    /// one at a time: each leaves a session without a window for a moment,
    /// and another test's "pick up what lost its window" must not adopt it.
    fn daemon() -> std::sync::MutexGuard<'static, ()> {
        static TURN: Mutex<()> = Mutex::new(());
        static START: std::sync::Once = std::sync::Once::new();
        let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        START.call_once(|| {
            crate::procs_win::test_fixture();
            thread::spawn(|| {
                let _ = run(false);
            });
        });
        turn
    }

    #[test]
    fn session_survives_its_window() {
        let _turn = daemon();
        let connect = || {
            for _ in 0..100 {
                if let Ok(s) = remote::connect(false) {
                    return s;
                }
                thread::sleep(Duration::from_millis(30));
            }
            panic!("daemon did not come up");
        };
        let spec = SessionSpec { exe: "cmd.exe".into(), args: vec![], cwd: None, cols: 80, rows: 24, journal: false, env: None };

        // First window: start a shell, run a command, then vanish.
        let (h1, attached) = remote::open(connect(), &spec, false).unwrap();
        assert!(!attached);
        assert!(h1.journal_path.as_ref().is_some_and(|p| p.is_file()), "the window was not told where the journal is");
        // The window is told where the journal is (the World view reads history's times from it).
        assert!(h1.journal_path.as_deref().is_some_and(|p| p.is_file()), "{:?}", h1.journal_path);
        h1.cmds.send(SessionCmd::Input(b"echo first-window-marker\r".to_vec())).unwrap();
        let (_, ok) = collect_until(&h1, 20, |out, _| contains(out, b"first-window-marker\r\n"));
        assert!(ok, "command output never arrived");
        drop(h1);
        thread::sleep(Duration::from_millis(300));

        // Second window: must find that session and get its history replayed.
        let (h2, attached) = remote::open(connect(), &spec, true).unwrap();
        assert!(attached, "the orphaned session was not offered for reattach");
        let (replay, done) = collect_until(&h2, 20, |_, ev| matches!(ev, SessionEvent::ReplayDone));
        assert!(done, "replay never finished");
        assert!(contains(&replay, b"first-window-marker"), "replay lost the earlier output");

        // And it is still alive: new input produces new output.
        h2.cmds.send(SessionCmd::Input(b"echo second-window-marker\r".to_vec())).unwrap();
        let (_, ok) = collect_until(&h2, 20, |out, _| contains(out, b"second-window-marker\r\n"));
        assert!(ok, "session was not live after reattach");

        h2.cmds.send(SessionCmd::Kill).unwrap();
        let (_, exited) = collect_until(&h2, 20, |_, ev| matches!(ev, SessionEvent::Exit(_)));
        assert!(exited, "kill did not end the session");
    }

    /// Processes sensed in the daemon reach the window as they happen, and a
    /// window that attaches later is given the same history from the journal.
    #[test]
    fn a_window_gets_processes_live_and_again_when_it_reattaches() {
        use crate::procs::Ledger;
        use std::cell::RefCell;
        let _turn = daemon();
        let connect = || {
            for _ in 0..100 {
                if let Ok(s) = remote::connect(false) {
                    return s;
                }
                thread::sleep(Duration::from_millis(30));
            }
            panic!("daemon did not come up");
        };
        // Fold every process event into `ledger`; stop once a child that ran
        // `... /c exit N` is recorded as having exited with N.
        let until_exit = |h: &session::SessionHandle, ledger: &RefCell<Ledger>, n: u32| {
            let tail = format!("/c exit {n}");
            collect_until(h, 20, |_, ev| {
                if let SessionEvent::Proc(p) = ev {
                    ledger.borrow_mut().apply(p);
                }
                ledger.borrow().procs().iter().any(|p| p.cmd.as_deref().is_some_and(|c| c.trim().ends_with(&tail)) && p.exit_code() == Some(n))
            })
            .1
        };
        let spec = SessionSpec { exe: "cmd.exe".into(), args: vec![], cwd: None, cols: 80, rows: 24, journal: false, env: None };

        // First window: the shell runs one command, seen live.
        let (h1, _) = remote::open(connect(), &spec, false).unwrap();
        let live = RefCell::new(Ledger::new());
        h1.cmds.send(SessionCmd::Input(b"cmd /c exit 5\r".to_vec())).unwrap();
        assert!(until_exit(&h1, &live, 5), "the child's exit never arrived: {:?}", live.borrow().procs());
        let live = live.into_inner();
        let (shell, child) = (&live.procs()[0], &live.procs()[1]);
        assert!(shell.running() && shell.image.as_deref().unwrap().to_lowercase().ends_with("\\cmd.exe"), "{shell:?}");
        assert_eq!(shell.ppid, Some(std::process::id()), "the shell is the daemon's child");
        assert_eq!((child.parent, live.procs().len()), (Some(shell.id), 2));
        drop(h1);
        thread::sleep(Duration::from_millis(300));

        // Second window: the same two processes come back, replayed in order
        // before the history ends, with every detail intact.
        let (h2, attached) = remote::open(connect(), &spec, true).unwrap();
        assert!(attached);
        let again = RefCell::new(Ledger::new());
        let (_, done) = collect_until(&h2, 20, |_, ev| {
            if let SessionEvent::Proc(p) = ev {
                again.borrow_mut().apply(p);
            }
            matches!(ev, SessionEvent::ReplayDone)
        });
        assert!(done, "replay never finished");
        assert_eq!(again.borrow().procs(), live.procs());

        // Still sensing after the reattach, and the shell's own end is the
        // last process event before the session's.
        h2.cmds.send(SessionCmd::Input(b"cmd /c exit 6\r".to_vec())).unwrap();
        assert!(until_exit(&h2, &again, 6), "nothing sensed after reattach: {:?}", again.borrow().procs());
        h2.cmds.send(SessionCmd::Kill).unwrap();
        let (_, exited) = collect_until(&h2, 20, |_, ev| {
            if let SessionEvent::Proc(p) = ev {
                again.borrow_mut().apply(p);
            }
            matches!(ev, SessionEvent::Exit(_))
        });
        assert!(exited, "kill did not end the session");
        let again = again.into_inner();
        assert_eq!((again.procs().len(), again.live_count()), (3, 0), "{:?}", again.procs());
        assert_eq!(again.procs()[0].exit_code(), Some(1), "the killed shell's exit code");
    }

    /// A command line near the longest Windows allows, its long argument
    /// holding no ASCII at all, crosses the daemon whole: live, as one
    /// frame of about 90 KB, and again from the journal when the next window
    /// attaches.
    #[test]
    fn a_very_long_command_line_crosses_the_daemon_live_and_replayed() {
        use crate::procs_win::{long_arg, test_fixture, Probe};
        let _turn = daemon();
        let connect = || {
            for _ in 0..100 {
                if let Ok(s) = remote::connect(false) {
                    return s;
                }
                thread::sleep(Duration::from_millis(30));
            }
            panic!("daemon did not come up");
        };
        let want = long_arg();
        let whole = |p: &Probe| p.ledger.procs().iter().any(|q| q.exit_code() == Some(21) && q.cmd.as_deref().is_some_and(|c| c.ends_with(&want)));
        let spec = SessionSpec { exe: "cmd.exe".into(), args: vec![], cwd: None, cols: 100, rows: 30, journal: false, env: None };

        // First window: a cmd session is told to run this test binary as the
        // helper that starts a child with that command line.
        let mut first = Probe::new(remote::open(connect(), &spec, false).unwrap().0);
        assert!(first.until(30, |p| p.said(b">")), "no prompt");
        let out = test_fixture().join("helper-long-cmd-daemon.txt");
        first.send("set PROTEUS_TEST_HELPER=long-cmd\r");
        first.send(&format!("set PROTEUS_TEST_OUT={}\r", out.display()));
        first.send(&format!("\"{}\" procs_win::tests::helper --exact --nocapture\r", std::env::current_exe().unwrap().display()));
        assert!(first.until(60, whole), "the long command line did not arrive live: {} process events, longest command line {} units", first.events.len(), first.ledger.procs().iter().filter_map(|q| q.cmd.as_ref()).map(|c| c.encode_utf16().count()).max().unwrap_or(0));
        assert!(first.replayed <= 1, "at most the shell's own start came with the replay; the rest was live");
        // Let the helper that started it end too, so that what is compared below is settled.
        assert!(first.until(30, |p| p.ledger.live_count() == 1), "{:?}", first.ledger.procs().iter().filter(|q| q.running()).map(|q| q.pid).collect::<Vec<_>>());
        let live: Vec<crate::procs::Proc> = first.ledger.procs().to_vec();
        drop(first);
        thread::sleep(Duration::from_millis(300));

        // Second window: the same history, from the journal.
        let (h, attached) = remote::open(connect(), &spec, true).unwrap();
        assert!(attached);
        let mut second = Probe::new(h);
        assert!(second.until(30, |p| whole(p) && p.ledger.procs().len() >= live.len() && p.ledger.live_count() == 1), "the long command line did not come back from the journal");
        assert_eq!(second.replayed, second.events.len(), "it came with the replayed history");
        assert_eq!(&second.ledger.procs()[..live.len()], &live[..], "and every detail of every process with it");
        second.h.cmds.send(SessionCmd::Kill).unwrap();
        assert!(second.until(30, |p| p.exit.is_some()), "kill did not end the session");
    }
}
