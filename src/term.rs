//! Terminal state: the byte stream parsed into a grid. The parser runs on its
//! own thread and wakes the UI only when a complete update is ready, which is
//! what makes "draw nothing while nothing changes" possible.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{self, Rgb};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::session::{SessionCmd, SessionEvent};
use crate::theme::Theme;

pub enum UiEvent {
    Wake,
    /// Benchmark clock.
    Tick,
    /// A reattached session's history has been replayed into the grid.
    Replayed,
    Title(String),
    Bell,
    Clipboard(String),
    Exit(Option<u32>),
    /// A process started or ended under the session's shell. `live` is false
    /// for history: one replayed from a reattached session's journal, which
    /// belongs in the ledger but did not just happen (the World view places
    /// it without lighting it).
    Proc {
        event: Box<crate::procs::ProcEvent>,
        live: bool,
    },
}

pub type Notify = Arc<dyn Fn(UiEvent) + Send + Sync>;

#[derive(Clone, Copy)]
pub struct Dim {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for Dim {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// State the parser thread needs in order to answer queries from programs.
pub struct Shared {
    pub theme: RwLock<Theme>,
    pub cell_w: AtomicU32,
    pub cell_h: AtomicU32,
    pub cols: AtomicU32,
    pub rows: AtomicU32,
    /// True while old output is being replayed. Queries found in history
    /// were answered when they were first asked; answering again would type
    /// the reply into whatever is running now.
    pub replaying: AtomicBool,
    /// Kitty keyboard flags in force where the parser last stopped (see `advance`).
    pub kitty_flags: AtomicU32,
}

/// Is this the terminal library's answer to "which keyboard flags are set?"
fn is_kitty_flags_reply(s: &str) -> bool {
    s.strip_prefix("\x1b[?").and_then(|r| r.strip_suffix('u')).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Parse `bytes`, stopping before every keyboard-flags query (`CSI ? u`) to
/// note the flags in force at that point. The terminal library answers the
/// query from its stack of pushed modes, which misses flags a program changed
/// in place with `CSI = flags u`; the listener sends the noted value instead.
fn advance(p: &mut Parser, t: &mut Term<Listener>, shared: &Shared, bytes: &[u8]) {
    const QUERY: &[u8] = b"\x1b[?u";
    let mut rest = bytes;
    loop {
        shared.kitty_flags.store(crate::input::kitty_flags(*t.mode()) as u32, Ordering::Release);
        let next = rest.windows(QUERY.len()).skip(1).position(|w| w == QUERY).map(|i| i + 1);
        let (now, later) = rest.split_at(next.unwrap_or(rest.len()));
        p.advance(t, now);
        if later.is_empty() {
            break;
        }
        rest = later;
    }
}

#[derive(Clone)]
pub struct Listener {
    cmds: Option<Sender<SessionCmd>>,
    notify: Notify,
    shared: Arc<Shared>,
}

impl Listener {
    /// A listener that answers nothing and notifies nobody: for terminals
    /// that only replay recorded output (recall.rs).
    pub fn silent() -> Self {
        let shared = Arc::new(Shared {
            theme: RwLock::new(Theme::void_light()),
            cell_w: AtomicU32::new(8),
            cell_h: AtomicU32::new(16),
            cols: AtomicU32::new(80),
            rows: AtomicU32::new(24),
            replaying: AtomicBool::new(true),
            kitty_flags: AtomicU32::new(0),
        });
        Listener { cmds: None, notify: Arc::new(|_| {}), shared }
    }

    fn reply(&self, s: String) {
        if let Some(c) = &self.cmds {
            let _ = c.send(SessionCmd::Input(s.into_bytes()));
        }
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if self.shared.replaying.load(Ordering::Acquire) {
            // History may rename the window; it may not speak, ring or copy.
            if let Event::Title(t) = event {
                (self.notify)(UiEvent::Title(t));
            }
            return;
        }
        match event {
            Event::PtyWrite(s) if is_kitty_flags_reply(&s) => {
                self.reply(format!("\x1b[?{}u", self.shared.kitty_flags.load(Ordering::Acquire)));
            }
            Event::PtyWrite(s) => self.reply(s),
            Event::ColorRequest(idx, f) => {
                let c = self.shared.theme.read().map(|t| t.query(idx)).unwrap_or([0, 0, 0]);
                self.reply(f(Rgb { r: c[0], g: c[1], b: c[2] }));
            }
            Event::TextAreaSizeRequest(f) => {
                let s = &self.shared;
                self.reply(f(WindowSize {
                    num_lines: s.rows.load(Ordering::Relaxed) as u16,
                    num_cols: s.cols.load(Ordering::Relaxed) as u16,
                    cell_width: s.cell_w.load(Ordering::Relaxed) as u16,
                    cell_height: s.cell_h.load(Ordering::Relaxed) as u16,
                }));
            }
            Event::Title(t) => (self.notify)(UiEvent::Title(t)),
            Event::Bell => (self.notify)(UiEvent::Bell),
            Event::ClipboardStore(_, s) => (self.notify)(UiEvent::Clipboard(s)),
            // Reading the clipboard on behalf of a program is refused: any
            // program (or text an agent was tricked into printing) could ask.
            _ => {}
        }
    }
}

type Parser = ansi::Processor<ansi::StdSyncHandler>;

/// How every terminal in this program is configured. Replays (recall.rs) must
/// use exactly this, or a replayed grid would differ from the live one.
pub fn vt_config() -> Config {
    Config {
        scrolling_history: 50_000,
        default_cursor_style: ansi::CursorStyle { shape: ansi::CursorShape::Beam, blinking: false },
        // Programs may ask for the kitty keyboard protocol (`input.rs` honours the flags).
        kitty_keyboard: true,
        ..Config::default()
    }
}

/// Resize a terminal. Every terminal in this program, live or replayed, is
/// resized through here, so a replay goes through exactly what the live one
/// did. On top of the terminal's own resize this does two things:
///
/// * A row counts how many of its cells have been touched, and a later clear
///   trusts that count. Widening appends untouched cells to rows whose count
///   may be zero while their cells are coloured; the next clear then skips
///   them and leaves stale stripes. Marking every row fully touched makes
///   the next clear complete (and leaves a row nothing to hide).
/// * While the alternate screen is up, the hidden primary screen's saved
///   cursor is set to its cursor, as entering the alternate screen sets it.
///   Reflow can pull the two apart into a state no terminal operation can
///   put back, which a keyframe therefore could not hold.
pub fn resize_term(term: &mut Term<Listener>, cols: usize, rows: usize) {
    use alacritty_terminal::index::Line;
    use alacritty_terminal::term::cell::Cell;
    use alacritty_terminal::term::TermMode;
    if (cols, rows) == (term.columns(), term.screen_lines()) {
        return;
    }
    term.resize(Dim { cols, rows });
    let touch = |g: &mut alacritty_terminal::Grid<Cell>| {
        for line in -(g.history_size() as i32)..g.screen_lines() as i32 {
            let _ = &mut g[Line(line)][..];
        }
    };
    // Swapping screens to reach the hidden one drops the selection; keep it.
    let selection = term.selection.take();
    touch(term.grid_mut());
    if term.mode().contains(TermMode::ALT_SCREEN) {
        // The swap back wipes the alternate screen: put it back as it was.
        let alt = term.grid().clone();
        term.swap_alt();
        touch(term.grid_mut());
        term.swap_alt();
        *term.grid_mut() = alt;
    } else {
        // The swap there overwrites the primary's saved cursor: put it back.
        let saved = term.grid().saved_cursor.clone();
        term.swap_alt();
        touch(term.grid_mut());
        term.swap_alt();
        term.grid_mut().saved_cursor = saved;
    }
    term.selection = selection;
}

pub struct Terminal {
    pub term: Arc<Mutex<Term<Listener>>>,
    parser: Arc<Mutex<Parser>>,
    cmds: Option<Sender<SessionCmd>>,
    notify: Notify,
    wake: Arc<AtomicBool>,
    pub shared: Arc<Shared>,
    pub cols: usize,
    pub rows: usize,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize, theme: Theme, notify: Notify, cmds: Option<Sender<SessionCmd>>) -> Self {
        let shared = Arc::new(Shared {
            theme: RwLock::new(theme),
            cell_w: AtomicU32::new(8),
            cell_h: AtomicU32::new(16),
            cols: AtomicU32::new(cols as u32),
            rows: AtomicU32::new(rows as u32),
            replaying: AtomicBool::new(false),
            kitty_flags: AtomicU32::new(0),
        });
        let listener = Listener { cmds: cmds.clone(), notify: notify.clone(), shared: shared.clone() };
        let term = Term::new(vt_config(), &Dim { cols, rows }, listener);
        Self {
            term: Arc::new(Mutex::new(term)),
            parser: Arc::new(Mutex::new(Parser::new())),
            cmds,
            notify,
            wake: Arc::new(AtomicBool::new(false)),
            shared,
            cols,
            rows,
        }
    }

    /// Start parsing a live session's output on a dedicated thread.
    pub fn attach(&self, events: Receiver<SessionEvent>) {
        let term = self.term.clone();
        let parser = self.parser.clone();
        let wake = self.wake.clone();
        let notify = self.notify.clone();
        let shared = self.shared.clone();
        let _ = thread::Builder::new().name("vt".into()).spawn(move || {
            let signal = || {
                if !wake.swap(true, Ordering::AcqRel) {
                    notify(UiEvent::Wake);
                }
            };
            // While a program holds a synchronized update open we must not
            // present a half-drawn frame; `deadline` bounds how long we wait.
            let mut deadline: Option<std::time::Instant> = None;
            // While history is being replayed the window stays quiet, then
            // shows the result in one frame.
            let mut replaying = false;
            loop {
                let ev = match deadline {
                    Some(d) => match events.recv_deadline(d) {
                        Ok(e) => e,
                        Err(RecvTimeoutError::Timeout) => {
                            if let (Ok(mut p), Ok(mut t)) = (parser.lock(), term.lock()) {
                                p.stop_sync(&mut *t);
                            }
                            deadline = None;
                            signal();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match events.recv() {
                        Ok(e) => e,
                        Err(_) => break,
                    },
                };
                match ev {
                    SessionEvent::Output(bytes) => {
                        if let (Ok(mut p), Ok(mut t)) = (parser.lock(), term.lock()) {
                            advance(&mut p, &mut t, &shared, &bytes);
                            deadline = if p.sync_bytes_count() > 0 { p.sync_timeout().sync_timeout() } else { None };
                        }
                        if deadline.is_none() && !replaying {
                            signal();
                        }
                    }
                    SessionEvent::ReplayResize(cols, rows) => {
                        replaying = true;
                        shared.replaying.store(true, Ordering::Release);
                        if let Ok(mut t) = term.lock() {
                            resize_term(&mut t, cols as usize, rows as usize);
                        }
                    }
                    SessionEvent::ReplayDone => {
                        replaying = false;
                        if let (Ok(mut p), Ok(mut t)) = (parser.lock(), term.lock()) {
                            if p.sync_bytes_count() > 0 {
                                p.stop_sync(&mut *t);
                            }
                        }
                        deadline = None;
                        shared.replaying.store(false, Ordering::Release);
                        notify(UiEvent::Replayed);
                    }
                    SessionEvent::Proc(event) => {
                        // History is what is replayed and old. Being replayed
                        // is not enough: a new session in the daemon has
                        // usually recorded its shell starting by the time its
                        // window attaches, so that arrives in the replay too,
                        // a few milliseconds after it happened.
                        let age_us = crate::procs_win::now_us().saturating_sub(event.seen_us());
                        notify(UiEvent::Proc { event, live: !replaying || age_us < 1_000_000 });
                    }
                    SessionEvent::Exit(code) => {
                        notify(UiEvent::Exit(code));
                        break;
                    }
                }
            }
        });
    }

    /// Parse bytes synchronously (replay and tests).
    pub fn feed(&self, bytes: &[u8]) {
        if let (Ok(mut p), Ok(mut t)) = (self.parser.lock(), self.term.lock()) {
            advance(&mut p, &mut t, &self.shared, bytes);
        }
    }

    /// Finish any synchronized update left open by a truncated stream.
    pub fn settle(&self) {
        if let (Ok(mut p), Ok(mut t)) = (self.parser.lock(), self.term.lock()) {
            if p.sync_bytes_count() > 0 {
                p.stop_sync(&mut *t);
            }
        }
    }

    /// Is new output waiting to be drawn? (Does not consume it.)
    pub fn has_wake(&self) -> bool {
        self.wake.load(Ordering::Acquire)
    }

    /// True once per batch of new output.
    pub fn take_wake(&self) -> bool {
        self.wake.swap(false, Ordering::AcqRel)
    }

    pub fn input(&self, bytes: Vec<u8>) {
        if let Some(c) = &self.cmds {
            let _ = c.send(SessionCmd::Input(bytes));
        }
    }

    pub fn kill(&self) {
        if let Some(c) = &self.cmds {
            let _ = c.send(SessionCmd::Kill);
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.force_resize(cols, rows);
    }

    /// Resize even if the size looks unchanged: after a replay the grid may
    /// be whatever size the session last had, not this window's.
    pub fn force_resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.shared.cols.store(cols as u32, Ordering::Relaxed);
        self.shared.rows.store(rows as u32, Ordering::Relaxed);
        if let Ok(mut t) = self.term.lock() {
            resize_term(&mut t, cols, rows);
        }
        if let Some(c) = &self.cmds {
            let _ = c.send(SessionCmd::Resize { cols: cols as u16, rows: rows as u16 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{bounded, unbounded};
    use std::time::Duration;

    /// A "what terminal are you?" query answered once in the past must not
    /// be answered again when that history is replayed.
    #[test]
    fn replayed_queries_are_not_answered_again() {
        let (cmd_tx, cmd_rx) = unbounded::<SessionCmd>();
        let (ev_tx, ev_rx) = bounded::<SessionEvent>(16);
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let notify: Notify = Arc::new(move |e| {
            if matches!(e, UiEvent::Replayed) {
                d2.store(true, Ordering::Release);
            }
        });
        let term = Terminal::new(80, 24, Theme::void_light(), notify, Some(cmd_tx));
        term.attach(ev_rx);
        let replies = |wait_ms: u64| {
            std::thread::sleep(Duration::from_millis(wait_ms));
            cmd_rx.try_iter().filter(|c| matches!(c, SessionCmd::Input(_))).count()
        };

        ev_tx.send(SessionEvent::ReplayResize(80, 24)).unwrap();
        ev_tx.send(SessionEvent::Output(b"old output \x1b[c\x1b]11;?\x07".to_vec())).unwrap();
        ev_tx.send(SessionEvent::ReplayDone).unwrap();
        assert_eq!(replies(150), 0, "history must stay silent");
        assert!(done.load(Ordering::Acquire));
        assert!(!term.has_wake(), "replay must not draw intermediate frames");

        ev_tx.send(SessionEvent::Output(b"\x1b[c".to_vec())).unwrap();
        assert_eq!(replies(150), 1, "a live query gets exactly one answer");
        assert!(term.has_wake());
    }

    /// Process events reach the window in order, each marked as history or
    /// as happening now, and by themselves cause nothing to be drawn.
    #[test]
    fn process_events_pass_through_and_history_is_marked() {
        use crate::procs::{ProcEvent, Start};
        let (ev_tx, ev_rx) = bounded::<SessionEvent>(16);
        let got = Arc::new(Mutex::new(Vec::new()));
        let g2 = got.clone();
        let notify: Notify = Arc::new(move |e| {
            if let UiEvent::Proc { event, live } = e {
                if let ProcEvent::Start(s) = *event {
                    g2.lock().unwrap().push((s.pid, live));
                }
            }
        });
        let term = Terminal::new(80, 24, Theme::void_light(), notify, None);
        term.attach(ev_rx);
        let now = crate::procs_win::now_us();
        let start = |pid, seen_us| SessionEvent::Proc(Box::new(ProcEvent::Start(Start { pid, ppid: None, image: None, cmd: None, start_us: None, seen_us })));
        let minute_ago = now - 60_000_000;
        ev_tx.send(start(1, now)).unwrap(); // an in-process session: live from the first moment
        ev_tx.send(SessionEvent::ReplayResize(80, 24)).unwrap();
        ev_tx.send(start(2, minute_ago)).unwrap(); // a reattached session's history
        ev_tx.send(start(3, now)).unwrap(); // replayed, but it happened a moment ago: a new session's shell
        ev_tx.send(SessionEvent::ReplayDone).unwrap();
        ev_tx.send(start(4, now)).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(*got.lock().unwrap(), vec![(1, true), (2, false), (3, true), (4, true)]);
        assert!(!term.has_wake(), "a process starting is not a reason to draw a frame");
    }
}
