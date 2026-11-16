//! Window side of the daemon link. It yields the same `SessionHandle` as an
//! in-process session, so the rest of the window cannot tell the difference.
use std::io::{self, BufReader, BufWriter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded};
use uds_windows::UnixStream;

use crate::paths;
use crate::proto;
use crate::session::{SessionCmd, SessionEvent, SessionHandle, SessionSpec};

fn other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// Start the daemon as its own detached process so it outlives this window.
fn start_daemon() -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    // Run it from a private copy so the launched file is never held open.
    let exe = crate::runtime::daemon_exe();
    let launch = |flags: u32| {
        Command::new(&exe)
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    // Leaving the launcher's job keeps the daemon alive when the launcher is
    // torn down; where that is not permitted, start it inside the job.
    launch(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| launch(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP))
        .map(|_| ())
}

/// If the running daemon is a different build with nothing in it, ask it to
/// leave so this build's daemon can take its place. Returns true once it has
/// gone. A daemon with live sessions is left alone and simply used.
fn retire_other_build() -> bool {
    let Ok(v) = list() else { return false };
    if v["build"].is_null() {
        return false; // a daemon from before hand-over existed; it will leave when idle
    }
    let same = v["build"].as_str() == Some(crate::runtime::build_id().as_str());
    let empty = v["sessions"].as_array().is_some_and(|a| a.is_empty());
    if same || !empty {
        return false;
    }
    let path = proto::socket_path();
    if let Ok(s) = UnixStream::connect(&path) {
        let _ = proto::write_frame(&mut BufWriter::new(&s), proto::C_QUIT, &[]);
    }
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(50));
        if UnixStream::connect(&path).is_err() {
            return true;
        }
    }
    false
}

/// Connect to the daemon, starting it first if asked and it is not running.
pub fn connect(start: bool) -> io::Result<UnixStream> {
    let path = proto::socket_path();
    if let Ok(s) = UnixStream::connect(&path) {
        if !start || !retire_other_build() {
            return Ok(s);
        }
    } else if !start {
        return Err(io::Error::new(io::ErrorKind::NotFound, "daemon not running"));
    }
    start_daemon()?;
    for _ in 0..80 {
        thread::sleep(Duration::from_millis(50));
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "daemon did not start"))
}

/// Ask the daemon what it is running.
pub fn list() -> io::Result<serde_json::Value> {
    let stream = connect(false)?;
    let mut rd = BufReader::new(stream.try_clone()?);
    let mut wr = BufWriter::new(&stream);
    proto::write_frame(&mut wr, proto::C_LIST, &[])?;
    let (ty, p) = proto::read_frame(&mut rd)?;
    if ty != proto::D_LIST {
        return Err(other("unexpected reply"));
    }
    serde_json::from_slice(&p).map_err(other)
}

/// Type into a running session from outside its window: the first of the
/// tools an agent gets for driving a terminal it is not sitting in.
pub fn send(id: u64, bytes: &[u8]) -> io::Result<()> {
    let stream = connect(false)?;
    let mut rd = BufReader::new(stream.try_clone()?);
    let mut wr = BufWriter::new(&stream);
    proto::write_frame(&mut wr, proto::C_ATTACH, &id.to_le_bytes())?;
    let (ty, p) = proto::read_frame(&mut rd)?;
    if ty != proto::D_INFO {
        return Err(other(String::from_utf8_lossy(&p)));
    }
    proto::write_frame(&mut wr, proto::C_INPUT, bytes)?;
    // Let the daemon read the frame before the socket closes.
    thread::sleep(Duration::from_millis(100));
    Ok(())
}

/// Open a session through the daemon. With `reattach`, a session that has
/// lost its window is picked up in preference to starting a new one; the
/// returned flag says which happened.
pub fn open(stream: UnixStream, spec: &SessionSpec, reattach: bool) -> io::Result<(SessionHandle, bool)> {
    open_with(stream, spec, if reattach { Some(0) } else { None })
}

/// As `open`, with the session to pick up named: `Some(0)` is the most recent
/// one without a window; any other number is that session and no other (a
/// window watching a session an agent drives), and it is an error if it has gone.
pub fn open_with(stream: UnixStream, spec: &SessionSpec, attach: Option<u64>) -> io::Result<(SessionHandle, bool)> {
    let mut rd = BufReader::with_capacity(1 << 16, stream.try_clone()?);
    let mut wr = BufWriter::new(stream.try_clone()?);
    let mut info: Option<Vec<u8>> = None;
    let mut attached = false;
    if let Some(id) = attach {
        proto::write_frame(&mut wr, proto::C_ATTACH, &id.to_le_bytes())?;
        let (ty, p) = proto::read_frame(&mut rd)?;
        if ty == proto::D_INFO {
            info = Some(p);
            attached = true;
        } else if id != 0 {
            return Err(other(String::from_utf8_lossy(&p)));
        }
    }
    if info.is_none() {
        let req = serde_json::json!({
            "exe": spec.exe, "args": spec.args,
            "cwd": spec.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            "cols": spec.cols, "rows": spec.rows,
            "env": crate::session::clean_env().into_iter().collect::<std::collections::BTreeMap<_, _>>(),
        });
        proto::write_frame(&mut wr, proto::C_SPAWN, &serde_json::to_vec(&req).map_err(other)?)?;
        let (ty, p) = proto::read_frame(&mut rd)?;
        if ty != proto::D_INFO {
            return Err(other(String::from_utf8_lossy(&p)));
        }
        info = Some(p);
    }
    let info: serde_json::Value = serde_json::from_slice(&info.unwrap_or_default()).map_err(other)?;
    if info["proto"].as_u64() != Some(proto::PROTO) {
        return Err(other("daemon speaks a different protocol version"));
    }
    // Where the session's journal is, for time travel. A daemon from before
    // it said so here still names it in its session list; one that names it
    // nowhere leaves the window without (and it says "unavailable"; the
    // World view then leaves history's output off its timeline).
    let journal_path = info["journal"].as_str().filter(|s| !s.is_empty()).map(std::path::PathBuf::from).or_else(|| {
        let id = info["id"].as_u64()?;
        let all = list().ok()?;
        all["sessions"].as_array()?.iter().find(|s| s["id"].as_u64() == Some(id))?["journal"].as_str().map(std::path::PathBuf::from)
    });

    let (ev_tx, ev_rx) = bounded::<SessionEvent>(256);
    let (cmd_tx, cmd_rx) = unbounded::<SessionCmd>();
    let bytes = Arc::new(AtomicU64::new(0));

    let counter = bytes.clone();
    thread::Builder::new().name("daemon-read".into()).spawn(move || {
        loop {
            let ev = match proto::read_frame(&mut rd) {
                Ok((proto::D_OUTPUT, p)) => {
                    counter.fetch_add(p.len() as u64, Ordering::Relaxed);
                    SessionEvent::Output(p)
                }
                Ok((proto::D_REPLAY_RESIZE, p)) if p.len() >= 4 => {
                    SessionEvent::ReplayResize(u16::from_le_bytes([p[0], p[1]]), u16::from_le_bytes([p[2], p[3]]))
                }
                Ok((proto::D_REPLAY_DONE, _)) => SessionEvent::ReplayDone,
                // From a daemon that senses processes; one this build cannot read is skipped.
                Ok((proto::D_PROC, p)) => match crate::procs::ProcEvent::from_bytes(&p) {
                    Some(ev) => SessionEvent::Proc(Box::new(ev)),
                    None => continue,
                },
                Ok((proto::D_EXIT, p)) => {
                    let code = p.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).filter(|&c| c != u32::MAX);
                    let _ = ev_tx.send(SessionEvent::Exit(code));
                    return;
                }
                Ok(_) => continue,
                Err(e) => {
                    // The daemon went away; the session is gone with it.
                    paths::log(&format!("daemon link lost: {e}"));
                    let _ = ev_tx.send(SessionEvent::Exit(None));
                    return;
                }
            };
            if ev_tx.send(ev).is_err() {
                return;
            }
        }
    })?;

    thread::Builder::new().name("daemon-write".into()).spawn(move || {
        for cmd in cmd_rx {
            let r = match cmd {
                SessionCmd::Input(b) => proto::write_frame(&mut wr, proto::C_INPUT, &b),
                SessionCmd::Resize { cols, rows } => {
                    proto::write_frame(&mut wr, proto::C_RESIZE, &crate::session::resize_payload(cols, rows))
                }
                SessionCmd::Kill => proto::write_frame(&mut wr, proto::C_KILL, &[]),
                SessionCmd::Close => break,
            };
            if r.is_err() {
                break;
            }
        }
        // Dropping the handle is how a window detaches: close both halves so
        // the reader wakes and the daemon sees the window go.
        let _ = stream.shutdown(std::net::Shutdown::Both);
    })?;

    Ok((SessionHandle { events: ev_rx, cmds: cmd_tx, journal_path, bytes }, attached))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::{ProcEvent, Start};
    use std::time::Instant;

    /// A window of this build against a daemon of an older one, which does
    /// not name the journal when a session is opened: the window must still
    /// learn where it is (from the daemon's session list).
    ///
    /// Needs a real older build, and sets the process's data folder and
    /// socket, so it runs only when asked for and by itself:
    ///   $env:PROTEUS_OLD_EXE = "dist\old.exe"
    ///   cargo test --release older_daemon -- --ignored
    #[test]
    #[ignore = "needs PROTEUS_OLD_EXE, an older build to act as the daemon"]
    fn an_older_daemon_still_tells_the_window_where_the_journal_is() {
        let old = std::env::var("PROTEUS_OLD_EXE").expect("set PROTEUS_OLD_EXE to an older build");
        let dir = std::env::temp_dir().join(format!("proteus-old-daemon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("PROTEUS_DATA", &dir);
        std::env::set_var("PROTEUS_SOCKET", dir.join("old.sock"));
        std::env::set_var("PROTEUS_SOUND", "off");
        let mut daemon = std::process::Command::new(&old).arg("daemon").spawn().expect("the older build did not start");
        let result = (|| -> io::Result<(String, std::path::PathBuf)> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let stream = loop {
                match connect(false) {
                    Ok(s) => break s,
                    Err(e) if Instant::now() > deadline => return Err(e),
                    Err(_) => thread::sleep(Duration::from_millis(50)),
                }
            };
            let build = list()?["build"].as_str().unwrap_or("").to_string();
            let spec = SessionSpec { exe: "cmd.exe".into(), args: vec![], cwd: None, cols: 80, rows: 24, journal: false, env: None };
            let (h, _) = open(stream, &spec, false)?;
            let path = h.journal_path.clone().ok_or_else(|| other("the window was left without the journal's path"))?;
            let _ = h.cmds.send(SessionCmd::Kill);
            let end = Instant::now() + Duration::from_secs(10);
            while !matches!(h.events.recv_deadline(end), Ok(SessionEvent::Exit(_)) | Err(_)) {}
            Ok((build, path))
        })();
        // Only the process started here is stopped.
        let _ = daemon.kill();
        let _ = daemon.wait();
        let (build, path) = result.expect("talking to the older daemon");
        assert_ne!(build, crate::runtime::build_id(), "that is this build, not an older one");
        assert!(path.is_file(), "{} is not a file", path.display());
        let (_, records) = crate::journal::read(&path).unwrap();
        assert!(!records.is_empty());
        eprintln!("older daemon {build}: journal found at {}", path.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A daemon newer than the window may send frames the window has never
    /// heard of. They must be skipped, not end the session or be mistaken
    /// for something else; this is what lets a daemon gain a frame type
    /// (as it did with process events) without breaking older windows.
    #[test]
    fn frames_this_build_does_not_know_are_skipped() {
        // This daemon does not say where the journal is, so the window asks
        // the daemon's session list: that must be the tests' own socket.
        crate::procs_win::test_fixture();
        let path = std::env::temp_dir().join(format!("proteus-remote-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = uds_windows::UnixListener::bind(&path).unwrap();
        let known = ProcEvent::Start(Start { pid: 7, ppid: Some(1), image: None, cmd: Some("git status".into()), start_us: Some(5), seen_us: 9 });
        let sent = known.clone();
        // The other end of a real socket, speaking the daemon's side of the protocol.
        let daemon = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut rd = BufReader::new(s.try_clone().unwrap());
            let mut wr = BufWriter::new(&s);
            assert_eq!(proto::read_frame(&mut rd).unwrap().0, proto::C_SPAWN);
            let info = serde_json::json!({ "proto": proto::PROTO, "id": 1, "exe": "x", "cols": 80, "rows": 24 });
            let frames: Vec<proto::Frame> = vec![
                (proto::D_INFO, serde_json::to_vec(&info).unwrap()),
                (200, b"a frame from a later build".to_vec()),
                (proto::D_PROC, b"{\"ev\":\"a kind from a later build\",\"pid\":1,\"seen_us\":1}".to_vec()),
                (proto::D_PROC, b"not even json".to_vec()),
                (proto::D_PROC, sent.to_bytes()),
                (proto::D_OUTPUT, b"still here".to_vec()),
                (proto::D_EXIT, 3u32.to_le_bytes().to_vec()),
            ];
            for (ty, p) in frames {
                proto::write_frame(&mut wr, ty, &p).unwrap();
            }
            let _ = proto::read_frame(&mut rd); // stay until the window has gone
        });
        let spec = SessionSpec { exe: "x".into(), args: vec![], cwd: None, cols: 80, rows: 24, journal: false, env: None };
        let (h, attached) = open(UnixStream::connect(&path).unwrap(), &spec, false).unwrap();
        assert!(!attached);
        let next = || h.events.recv_timeout(Duration::from_secs(10)).ok();
        assert!(matches!(next(), Some(SessionEvent::Proc(p)) if *p == known), "the one readable process event, and nothing before it");
        assert!(matches!(next(), Some(SessionEvent::Output(b)) if b == b"still here"));
        assert!(matches!(next(), Some(SessionEvent::Exit(Some(3)))));
        drop(h);
        daemon.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
