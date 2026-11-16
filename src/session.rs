//! A session owns one child process on a pseudo-terminal and journals its
//! output. The UI only ever sees two channels (events out, commands in), so the
//! same UI code can later talk to a session living in a separate daemon.
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::journal::{self, JournalWriter};
use crate::paths;

pub enum SessionEvent {
    Output(Vec<u8>),
    Exit(Option<u32>),
    /// Replay only: the size the terminal had at this point in its history.
    ReplayResize(u16, u16),
    /// Replay finished; everything after this is live.
    ReplayDone,
    /// A process started or ended under the session's shell (`procs.rs`).
    Proc(Box<crate::procs::ProcEvent>),
}

pub enum SessionCmd {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Kill,
    /// Internal: the child has exited, release the pseudo-terminal.
    Close,
}

#[derive(Clone)]
pub struct SessionSpec {
    pub exe: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    pub journal: bool,
    /// Environment for the child. `None` inherits this process's. The daemon
    /// is long-lived, so windows pass their own environment instead: a new
    /// window then sees today's PATH, not the one the daemon was born with.
    pub env: Option<Vec<(String, String)>>,
}

/// This process's environment minus variables that describe a parent
/// terminal or agent.
pub fn clean_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| !scrubbed(k))
        .collect()
}

pub struct SessionHandle {
    pub events: Receiver<SessionEvent>,
    pub cmds: Sender<SessionCmd>,
    pub journal_path: Option<PathBuf>,
    /// Total bytes the child has printed.
    pub bytes: Arc<std::sync::atomic::AtomicU64>,
}

/// Variables that describe the *parent* terminal or agent and would mislead
/// programs running inside this one.
const SCRUB_PREFIXES: &[&str] = &[
    "HERDR_", "CLAUDE_CODE_", "CLAUDECODE", "CLAUDE_PID", "WT_", "NO_COLOR", "TERM_PROGRAM",
    "STARSHIP_SHELL", "POSH_SESSION_ID", "GIT_TERMINAL_PROMPT",
];

/// Switches a person sets on purpose for the program inside; they share a
/// scrubbed prefix but say nothing about a parent session.
const SCRUB_KEEP: &[&str] = &[
    "CLAUDE_CODE_FORCE_SYNC_OUTPUT", "CLAUDE_CODE_NO_FLICKER", "CLAUDE_CODE_BS_AS_CTRL_BACKSPACE",
    "CLAUDE_CODE_ALT_SCREEN_FULL_REPAINT", "CLAUDE_CODE_DISABLE_TERMINAL_TITLE",
];

/// Should this variable be withheld from programs started in a session?
fn scrubbed(name: &str) -> bool {
    SCRUB_PREFIXES.iter().any(|p| name.starts_with(p)) && !SCRUB_KEEP.contains(&name)
}

fn to_io<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

pub fn spawn_local(spec: SessionSpec) -> io::Result<SessionHandle> {
    crate::runtime::ensure_conpty();
    let cols = spec.cols.max(2);
    let rows = spec.rows.max(2);
    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(to_io)?;

    let mut cmd = CommandBuilder::new(&spec.exe);
    for a in &spec.args {
        cmd.arg(a);
    }
    let cwd = spec
        .cwd
        .clone()
        .filter(|p| p.is_dir())
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    cmd.cwd(&cwd);
    if let Some(env) = &spec.env {
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    for (k, _) in std::env::vars_os() {
        if let Some(k) = k.to_str() {
            if scrubbed(k) {
                cmd.env_remove(k);
            }
        }
    }
    let session_id = format!("{}-{}", paths::unix_secs(), std::process::id());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "proteus");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env("PROTEUS", "1");
    cmd.env("PROTEUS_SESSION", &session_id);

    // Process sensing (procs_win.rs). The job is made before the shell, so
    // that joining it is the first thing that happens once the shell exists.
    let procs = crate::procs_win::prepare();
    let mut child = pair.slave.spawn_command(cmd).map_err(to_io)?;
    if let Some(p) = &procs {
        p.adopt(child.as_raw_handle());
    }
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(to_io)?;
    let mut writer = pair.master.take_writer().map_err(to_io)?;
    let master = pair.master;
    let mut killer = child.clone_killer();

    let journal_path = if spec.journal { Some(paths::journal_dir().join(format!("{session_id}.prtj"))) } else { None };
    let journal = journal_path.as_ref().and_then(|p| {
        let meta = serde_json::json!({
            "v": 1, "session": session_id, "exe": spec.exe, "args": spec.args,
            "cwd": cwd.to_string_lossy(), "cols": cols, "rows": rows, "started_unix": paths::unix_secs(),
        });
        match JournalWriter::create(p, &meta) {
            Ok(mut j) => {
                let _ = j.record(journal::K_RESIZE, &resize_payload(cols, rows));
                Some(Arc::new(Mutex::new(j)))
            }
            Err(e) => {
                paths::log(&format!("journal disabled: {e}"));
                None
            }
        }
    });

    let (ev_tx, ev_rx) = bounded::<SessionEvent>(256);
    let (cmd_tx, cmd_rx) = unbounded::<SessionCmd>();
    let exit_code: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    if let Some(p) = &procs {
        p.attach(ev_tx.clone(), journal.clone());
    }

    // Reader: pseudo-terminal -> journal -> UI. The bounded channel gives the
    // child natural back-pressure when it prints faster than we can parse.
    let bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let j = journal.clone();
        let code = exit_code.clone();
        let bytes = bytes.clone();
        let reply = cmd_tx.clone();
        thread::Builder::new().name("pty-read".into()).spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            let mut scanner = crate::marks::Scanner::new(None);
            let mut found = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        bytes.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                        // A program asking whether this terminal takes status
                        // reports (OSC 7501) is answered here: once, on the live
                        // path only, and ahead of anything a window replies.
                        scanner.feed(&buf[..n], &mut found);
                        for f in found.drain(..) {
                            if f.mark == crate::marks::Mark::StatusQuery {
                                let _ = reply.send(SessionCmd::Input(crate::marks::STATUS_REPLY.to_vec()));
                            }
                        }
                        if let Some(j) = &j {
                            if let Ok(mut j) = j.lock() {
                                let _ = j.record(journal::K_OUTPUT, &buf[..n]);
                            }
                        }
                        if ev_tx.send(SessionEvent::Output(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
            // Every process event up to here reaches the window before the exit does.
            if let Some(p) = procs {
                p.finish();
            }
            let c = code.lock().ok().and_then(|c| *c);
            if let Some(j) = &j {
                if let Ok(mut j) = j.lock() {
                    let _ = j.record(journal::K_EXIT, &c.unwrap_or(0).to_le_bytes());
                    let _ = j.flush();
                }
            }
            let _ = ev_tx.send(SessionEvent::Exit(c));
        })?;
    }

    // Control: UI -> pseudo-terminal. Owns the master so dropping it on Close
    // shuts the pseudo-console and ends the reader.
    {
        let j = journal.clone();
        thread::Builder::new().name("pty-ctl".into()).spawn(move || {
            for cmd in cmd_rx {
                match cmd {
                    SessionCmd::Input(b) => {
                        if writer.write_all(&b).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                    SessionCmd::Resize { cols, rows } => {
                        let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                        if let Some(j) = &j {
                            if let Ok(mut j) = j.lock() {
                                let _ = j.record(journal::K_RESIZE, &resize_payload(cols, rows));
                            }
                        }
                    }
                    SessionCmd::Kill => {
                        let _ = killer.kill();
                    }
                    SessionCmd::Close => break,
                }
            }
            drop(writer);
            drop(master);
        })?;
    }

    // Waiter: notices the child exiting, lets trailing output drain, then closes.
    {
        let code = exit_code.clone();
        let tx = cmd_tx.clone();
        thread::Builder::new().name("pty-wait".into()).spawn(move || {
            let st = child.wait().ok().map(|s| s.exit_code());
            if let Ok(mut c) = code.lock() {
                *c = st;
            }
            thread::sleep(Duration::from_millis(60));
            let _ = tx.send(SessionCmd::Close);
        })?;
    }

    // Flusher: bounds how much journal a crash can lose to a quarter second.
    if let Some(j) = journal {
        thread::Builder::new().name("journal-flush".into()).spawn(move || loop {
            thread::sleep(Duration::from_millis(250));
            if Arc::strong_count(&j) <= 1 {
                break;
            }
            if let Ok(mut j) = j.lock() {
                let _ = j.flush();
            }
        })?;
    }

    Ok(SessionHandle { events: ev_rx, cmds: cmd_tx, journal_path, bytes })
}

pub fn resize_payload(cols: u16, rows: u16) -> [u8; 4] {
    let c = cols.to_le_bytes();
    let r = rows.to_le_bytes();
    [c[0], c[1], r[0], r[1]]
}

/// Pick the shell to start: `PROTEUS_SHELL`, then PowerShell 7, Windows
/// PowerShell, and finally `cmd`.
pub fn default_shell() -> (String, Vec<String>) {
    if let Ok(s) = std::env::var("PROTEUS_SHELL") {
        if !s.trim().is_empty() {
            return (s, vec![]);
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for name in ["pwsh.exe", "powershell.exe"] {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(name);
            if p.is_file() {
                return (p.to_string_lossy().into_owned(), vec!["-NoLogo".into()]);
            }
        }
    }
    (std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()), vec![])
}
