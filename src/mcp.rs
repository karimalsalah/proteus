//! `proteus mcp`: the terminal an agent would ask for, as a Model Context
//! Protocol server on standard input and output (JSON-RPC 2.0, one message
//! per line).
//!
//! It speaks both generations of the protocol: the `initialize` handshake of
//! revisions 2024-11-05 to 2025-11-25, and the stateless revision 2026-07-28,
//! in which every request names its version in `_meta` and `server/discover`
//! replaces the handshake.
//!
//!   sessions  what the daemon is running
//!   spawn     a session of the agent's own; a person watches it with `proteus --attach ID`
//!   send      typing: text and named keys
//!   wait      block until a pattern, a silence or the exit, instead of sleeping and guessing
//!   read      what was printed since the last read; lean by default (lean.rs)
//!   screen    the screen as text, now or at any moment of the session
//!   shot      the same as a picture, through the renderer the window uses
//!   kill
//!
//! Safety. The daemon also holds the person's own sessions. This server may
//! look at any of them, but it types into or ends only the sessions it
//! spawned, and what proves that is the connection it spawned them on, never
//! a session number (numbers start again at 1 with every daemon).
//! `--drive-all` lifts this. In no case does it answer a program's questions
//! to its terminal, or resize a session: its emulators have no channel back
//! (`Terminal` without commands, `lean::Mute`) and it never sends a resize.
//!
//! Where the bytes come from. A session spawned here is read from the
//! connection it was spawned on: complete, in order, and a blocking read is
//! the wake-up, so a `wait` costs nothing while nothing happens. Any other
//! session is read from its journal file. Windows reports no change for a
//! file its writer keeps open (measured: no notification for 12 appends), so
//! a `wait` on those looks every `FOLLOW`; the cost is in forge/lanes/organs.json.
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi;
use serde_json::{json, Value};
use uds_windows::UnixStream;

use crate::journal;
use crate::lean::{self, Ribbon, Rules, Strip};
use crate::proto;
use crate::remote;
use crate::term::{Dim, Terminal, UiEvent};
use crate::theme::Theme;
use crate::Res;

/// Revisions with the `initialize` handshake, newest first.
const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// The stateless revision.
const MODERN: &str = "2026-07-28";
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";

/// How often a `wait` looks at a session it can only read from disk. Each
/// look asks the daemon to write its journals out, so this is also the
/// most such a wait lags. Measured at 100 ms: 0.9% of a core between the
/// two processes; at 250 ms the figures in forge/lanes/organs.json.
const FOLLOW: Duration = Duration::from_millis(250);
/// A match that ends where the output so far ends (a prompt, or half a
/// line) is taken once nothing more has arrived for this long.
const SETTLE: Duration = Duration::from_millis(40);
/// Output held in memory per connected session; older output is read back from its journal.
const MEMORY: usize = 64 << 20;
/// Ended sessions whose output stays in memory; older ones are read from their journals.
const KEEP_ENDED: usize = 8;
/// Default size of the text one `read` returns: about 6,000 tokens.
const PAGE: usize = 24_000;
/// Bytes fed to an emulator between looks at how much text has built up.
const STEP: usize = 16 << 10;
const BLOCK: usize = 256 << 10;
/// Bytes of a journal file read at a time while it is indexed.
const INDEX_BLOCK: usize = 4 << 20;
/// Widest and tallest grid an emulator here is ever given, whatever a journal says.
const MAX_SIDE: u16 = 1000;
/// Largest picture, in pixels a side.
const MAX_PICTURE: u32 = 8192;
/// How far past its size a read may run on to end where a line ends.
const RUN_ON: u64 = 8 << 10;
/// Scroll-back kept by the emulator behind `screen` and `shot`. Only the
/// screen is ever read from it; this much is what a resize to more rows can
/// bring back into view. (The window keeps 50,000 lines at 24 bytes a cell:
/// measured here as 164 MB held after one `screen` of a long session at 120
/// columns and 639 MB at 500.)
const SCROLLBACK: usize = MAX_SIDE as usize;
/// The longest command line Windows will start.
const MAX_COMMAND: usize = 32_767;

const INSTRUCTIONS: &str = "PROTEUS terminal sessions. spawn a shell or a command, send text and keys, then wait \
(for a regex, for the output to go quiet, or for the exit) instead of sleeping, and read. read is lean by default: escape \
codes removed, progress bars reduced to their final state, repeats counted, long output cut to head, tail and every error \
or warning line; mode \"plain\" or \"raw\" gives the rest. screen and shot show what a person would see, now or at any \
earlier second. Sessions this server did not spawn can be looked at but not typed into, unless it was started with --drive-all.";

// ---------------------------------------------------------------------------
// Journal on disk, read incrementally.

struct Mark {
    t_us: u64,
    /// Output bytes before this record.
    out: u64,
    /// Where the payload lies in the file.
    at: u64,
    len: u32,
    /// A resize record: the size from here on.
    size: Option<(u16, u16)>,
}

/// An index of a journal file that grows as the file does. Payloads stay on disk.
struct Tail {
    path: PathBuf,
    file: Option<std::fs::File>,
    /// File offset of the first byte not yet indexed.
    pos: u64,
    /// Output bytes indexed so far.
    out: u64,
    marks: Vec<Mark>,
    exit: Option<Option<u32>>,
}

impl Tail {
    fn new(path: PathBuf) -> Self {
        Self { path, file: None, pos: 0, out: 0, marks: Vec::new(), exit: None }
    }

    /// Index whatever has been appended since the last call.
    fn refresh(&mut self) -> io::Result<()> {
        if self.file.is_none() {
            self.file = Some(std::fs::File::open(&self.path)?);
        }
        let Some(f) = self.file.as_mut() else { return Ok(()) };
        let len = f.metadata()?.len();
        if self.pos == 0 {
            if len < 12 {
                return Ok(());
            }
            let mut head = [0u8; 12];
            f.seek(SeekFrom::Start(0))?;
            f.read_exact(&mut head)?;
            if &head[..8] != journal::MAGIC {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a PROTEUS journal"));
            }
            let meta = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as u64;
            if len < 12 + meta {
                return Ok(());
            }
            self.pos = 12 + meta;
        }
        // A window at a time: a journal of gigabytes is never held in memory whole.
        let mut window = INDEX_BLOCK;
        while self.pos < len {
            let left = len - self.pos;
            let mut buf = vec![0u8; window.min(left as usize)];
            f.seek(SeekFrom::Start(self.pos))?;
            f.read_exact(&mut buf)?;
            let mut i = 0usize;
            while i + 13 <= buf.len() {
                let kind = buf[i];
                let t_us = u64::from_le_bytes(buf[i + 1..i + 9].try_into().unwrap_or_default());
                let n = u32::from_le_bytes(buf[i + 9..i + 13].try_into().unwrap_or_default()) as usize;
                if i + 13 + n > buf.len() {
                    break; // this record goes on past the window, or its writer is in the middle of it
                }
                let p = &buf[i + 13..i + 13 + n];
                let at = self.pos + i as u64 + 13;
                match kind {
                    journal::K_OUTPUT => {
                        self.marks.push(Mark { t_us, out: self.out, at, len: n as u32, size: None });
                        self.out += n as u64;
                    }
                    journal::K_RESIZE if n >= 4 => {
                        let size = (u16::from_le_bytes([p[0], p[1]]), u16::from_le_bytes([p[2], p[3]]));
                        self.marks.push(Mark { t_us, out: self.out, at, len: 0, size: Some(size) });
                    }
                    journal::K_EXIT => {
                        let code = p.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                        self.exit = Some(code.filter(|&c| c != u32::MAX));
                    }
                    _ => {}
                }
                i += 13 + n;
            }
            self.pos += i as u64;
            if i == 0 {
                if buf.len() as u64 == left {
                    break; // the file ends inside a record: the rest of it is still to be written
                }
                window *= 2; // one record larger than the window
            }
        }
        Ok(())
    }

    /// Output bytes `from..to` of the stream.
    fn read(&mut self, from: u64, to: u64) -> io::Result<Vec<u8>> {
        let to = to.min(self.out);
        if from >= to {
            return Ok(Vec::new());
        }
        let first = self.marks.partition_point(|m| m.out + m.len as u64 <= from);
        let last = self.marks.partition_point(|m| m.out < to);
        let (a, b) = (self.marks[first].at, self.marks[last - 1].at + self.marks[last - 1].len as u64);
        let Some(f) = self.file.as_mut() else { return Ok(Vec::new()) };
        let mut span = vec![0u8; (b - a) as usize];
        f.seek(SeekFrom::Start(a))?;
        f.read_exact(&mut span)?;
        let mut out = Vec::with_capacity((to - from) as usize);
        for m in &self.marks[first..last] {
            let (lo, hi) = (from.max(m.out), to.min(m.out + m.len as u64));
            if lo < hi {
                let base = (m.at - a) as usize;
                out.extend_from_slice(&span[base + (lo - m.out) as usize..base + (hi - m.out) as usize]);
            }
        }
        Ok(out)
    }

    /// How much had been printed `t_us` microseconds into the session.
    fn offset_at(&self, t_us: u64) -> u64 {
        let i = self.marks.partition_point(|m| m.t_us <= t_us);
        if i == 0 {
            0
        } else {
            self.marks[i - 1].out + self.marks[i - 1].len as u64
        }
    }

    /// Every size the terminal has had: (output bytes before it, cols, rows).
    fn resizes(&self, until_us: u64) -> Vec<(u64, u16, u16)> {
        self.marks.iter().filter(|m| m.t_us <= until_us).filter_map(|m| m.size.map(|(c, r)| (m.out, c, r))).collect()
    }
}

// ---------------------------------------------------------------------------
// One session, as this server knows it.

struct Live {
    /// A connection to the daemon is delivering this session's output.
    linked: bool,
    /// History is still arriving after an attach.
    replaying: bool,
    /// `mem` holds output bytes `base..end`.
    base: u64,
    mem: Vec<u8>,
    end: u64,
    exit: Option<Option<u32>>,
    /// The daemon went away underneath the session.
    lost: bool,
    last_out: Instant,
    /// Where `read` and `wait` continue when no cursor is given.
    read_pos: u64,
    wait_pos: u64,
}

impl Live {
    /// The part of output `from..to` that is in memory. Never out of range,
    /// whatever is asked: a wrong slice here would take the whole server down.
    fn slice(&self, from: u64, to: u64) -> &[u8] {
        let a = from.clamp(self.base, self.end);
        let b = to.clamp(a, self.end);
        &self.mem[(a - self.base) as usize..(b - self.base) as usize]
    }

    /// Take in new output, letting go of the oldest once more than `cap` is held.
    fn push(&mut self, bytes: &[u8], cap: usize) {
        self.mem.extend_from_slice(bytes);
        self.end += bytes.len() as u64;
        if self.mem.len() > cap {
            let drop = self.mem.len() - cap / 2;
            self.mem.drain(..drop);
            self.base += drop as u64;
        }
    }
}

struct Sess {
    id: u64,
    /// The daemon that numbered it. The same number in another daemon is another session.
    daemon: u64,
    exe: String,
    journal: PathBuf,
    size0: (u16, u16),
    /// Spawned by this process, on a connection it still holds or held.
    owned: bool,
    /// The half of that connection we write on. Holding it is the right to drive.
    link: Mutex<Option<BufWriter<UnixStream>>>,
    st: Mutex<Live>,
    cv: Condvar,
    tail: Mutex<Tail>,
    lean: Mutex<Option<LeanView>>,
    view: Mutex<Option<View>>,
}

/// What one look at a session found, taken in one piece so that an exit is
/// never seen before the output that preceded it.
struct Take {
    bytes: Vec<u8>,
    end: u64,
    exit: Option<Option<u32>>,
    lost: bool,
    last_out: Option<Instant>,
}

impl Sess {
    fn new(id: u64, daemon: u64, exe: String, journal: PathBuf, size0: (u16, u16), owned: bool) -> Self {
        Self {
            id,
            daemon,
            exe,
            journal: journal.clone(),
            size0,
            owned,
            link: Mutex::new(None),
            st: Mutex::new(Live {
                linked: false,
                replaying: false,
                base: 0,
                mem: Vec::new(),
                end: 0,
                exit: None,
                lost: false,
                last_out: Instant::now(),
                read_pos: 0,
                wait_pos: 0,
            }),
            cv: Condvar::new(),
            tail: Mutex::new(Tail::new(journal)),
            lean: Mutex::new(None),
            view: Mutex::new(None),
        }
    }

    fn linked(&self) -> bool {
        self.st.lock().unwrap().linked
    }

    /// For a session someone else started: what it printed before this server
    /// first looked at it is history. A `wait` without a cursor must not find
    /// its pattern there and return at once. (`read` still starts at the top.)
    fn start_waiting_from_now(&self) {
        let end = {
            let mut tail = self.tail.lock().unwrap();
            let _ = tail.refresh();
            tail.out
        };
        self.st.lock().unwrap().wait_pos = end;
    }

    /// Take over a daemon connection that is attached to this session:
    /// everything it delivers from now on lands in `mem`.
    fn link(self: &Arc<Self>, stream: UnixStream, mut rd: BufReader<UnixStream>, replaying: bool) -> io::Result<()> {
        {
            let mut st = self.st.lock().unwrap();
            // While history is still arriving, readers keep using the journal.
            st.linked = !replaying;
            st.replaying = replaying;
            st.base = 0;
            st.end = 0;
            st.mem.clear();
            st.last_out = Instant::now();
        }
        *self.link.lock().unwrap() = Some(BufWriter::new(stream));
        let me = self.clone();
        thread::Builder::new().name(format!("session-{}", self.id)).spawn(move || {
            loop {
                let frame = proto::read_frame(&mut rd);
                let mut st = me.st.lock().unwrap();
                match frame {
                    Ok((proto::D_OUTPUT, p)) => {
                        st.push(&p, MEMORY);
                        if !st.replaying {
                            st.last_out = Instant::now();
                        }
                    }
                    Ok((proto::D_REPLAY_DONE, _)) => {
                        st.replaying = false;
                        st.linked = true;
                    }
                    Ok((proto::D_EXIT, p)) => {
                        let code = p.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                        st.exit = Some(code.filter(|&c| c != u32::MAX));
                        st.replaying = false;
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => {
                        if st.exit.is_none() {
                            st.exit = Some(None);
                            st.lost = true;
                        }
                        st.replaying = false;
                        break;
                    }
                }
                drop(st);
                me.cv.notify_all();
            }
            me.cv.notify_all();
            // Nothing more can be typed: let the connection go.
            *me.link.lock().unwrap() = None;
        })?;
        Ok(())
    }

    fn write(&self, ty: u8, payload: &[u8]) -> io::Result<()> {
        match self.link.lock().unwrap().as_mut() {
            Some(w) => proto::write_frame(w, ty, payload),
            None => Err(io::Error::new(io::ErrorKind::NotConnected, "not connected to this session")),
        }
    }

    /// Output from `from` up to `to` (or the end), and where it actually ended.
    fn bytes(&self, from: u64, to: Option<u64>) -> io::Result<(Vec<u8>, u64)> {
        let base = {
            let st = self.st.lock().unwrap();
            if st.linked {
                if from >= st.base {
                    let part = st.slice(from, to.unwrap_or(u64::MAX));
                    return Ok((part.to_vec(), from + part.len() as u64));
                }
                Some(st.base)
            } else {
                None
            }
        };
        let mut tail = self.tail.lock().unwrap();
        tail.refresh()?;
        let Some(base) = base else {
            let out = tail.read(from, to.unwrap_or(u64::MAX))?;
            let got = from + out.len() as u64;
            return Ok((out, got));
        };
        // Connected, but this part has left memory: the journal has it, memory has what follows.
        let mut out = tail.read(from, to.map_or(base, |t| t.min(base)))?;
        drop(tail);
        let got = from + out.len() as u64;
        if got == base && to.is_none_or(|t| t > base) {
            let st = self.st.lock().unwrap();
            // Memory may have moved on meanwhile; only join what is contiguous.
            if st.base <= got {
                out.extend_from_slice(st.slice(got, to.unwrap_or(u64::MAX)));
            }
        }
        let got = from + out.len() as u64;
        Ok((out, got))
    }

    /// How much the session has printed and whether it has ended. `running`
    /// is what the daemon's list said, for a session read from disk.
    fn state(&self, running: bool) -> io::Result<(u64, Option<Option<u32>>)> {
        {
            let st = self.st.lock().unwrap();
            if st.linked {
                return Ok((st.end, st.exit));
            }
        }
        let mut tail = self.tail.lock().unwrap();
        tail.refresh()?;
        Ok((tail.out, tail.exit.or(if running { None } else { Some(None) })))
    }

    /// Everything new since `pos`, with the session's state at that moment.
    fn take(&self, pos: u64, running: bool) -> io::Result<Take> {
        {
            let st = self.st.lock().unwrap();
            if st.linked && pos >= st.base {
                return Ok(Take {
                    bytes: st.slice(pos, u64::MAX).to_vec(),
                    end: st.end,
                    exit: st.exit,
                    lost: st.lost,
                    last_out: Some(st.last_out),
                });
            }
        }
        // The exit is looked at first, the output after: whatever was printed
        // before the exit is then certainly in what is read.
        let (_, exit) = self.state(running)?;
        let (bytes, end) = self.bytes(pos, None)?;
        Ok(Take { bytes, end: end.max(pos), exit, lost: false, last_out: None })
    }

    /// Every size the session has had up to `until_us` into it, from its journal.
    fn resizes(&self, until_us: u64) -> Vec<(u64, u16, u16)> {
        let mut tail = self.tail.lock().unwrap();
        let _ = tail.refresh();
        let mut v = tail.resizes(until_us);
        if v.first().is_none_or(|r| r.0 != 0) {
            v.insert(0, (0, self.size0.0, self.size0.1));
        }
        v
    }
}

// ---------------------------------------------------------------------------
// Emulators kept per session so that repeated calls cost only what is new.

/// The lean reader's ribbon, fed exactly up to the cursor last handed out.
struct LeanView {
    rib: Ribbon,
    at: u64,
    size: (u16, u16),
}

/// The screen as the window would show it.
struct View {
    term: Terminal,
    title: Arc<Mutex<String>>,
    fed: u64,
    size: (u16, u16),
    /// Resize records at or before `fed` when it was last brought up to date.
    applied: usize,
}

impl View {
    fn new(size: (u16, u16)) -> Self {
        let title = Arc::new(Mutex::new(String::new()));
        let t2 = title.clone();
        let notify: crate::term::Notify = Arc::new(move |e| {
            if let UiEvent::Title(t) = e {
                *t2.lock().unwrap() = t;
            }
        });
        // No command channel: this terminal can be read but can say nothing.
        let term = Terminal::new(size.0.clamp(2, MAX_SIDE) as usize, size.1.clamp(1, MAX_SIDE) as usize, Theme::void_light(), notify, None);
        if let Ok(mut t) = term.term.lock() {
            // As term.rs sets it up, but for the scroll-back.
            t.set_options(alacritty_terminal::term::Config {
                scrolling_history: SCROLLBACK,
                default_cursor_style: ansi::CursorStyle { shape: ansi::CursorShape::Beam, blinking: false },
                ..Default::default()
            });
        }
        Self { term, title, fed: 0, size, applied: 0 }
    }

    fn resize(&mut self, size: (u16, u16)) {
        if size != self.size {
            self.size = size;
            if let Ok(mut t) = self.term.term.lock() {
                t.resize(Dim { cols: size.0.clamp(2, MAX_SIDE) as usize, rows: size.1.clamp(1, MAX_SIDE) as usize });
            }
        }
    }
}

/// Something output can be replayed into.
trait Emu {
    fn put(&mut self, bytes: &[u8]);
    fn size(&mut self, cols: u16, rows: u16);
    /// Bytes to take in between two looks at whether it is enough.
    fn step(&self) -> usize {
        STEP
    }
    /// Asked between steps: has enough built up to stop here?
    fn enough(&mut self) -> bool {
        false
    }
    /// Is this a place to stop: no line half written?
    fn at_rest(&mut self) -> bool {
        true
    }
}

impl Emu for View {
    fn put(&mut self, bytes: &[u8]) {
        self.term.feed(bytes);
    }
    fn size(&mut self, cols: u16, rows: u16) {
        self.resize((cols, rows));
    }
}

/// How much text a read may build up before it stops and leaves the rest.
struct Budget {
    lean: bool,
    rules: Rules,
    cap: usize,
    /// Scrolled-off weight at which the pending text is next worth measuring.
    check_at: usize,
}

struct Reader<'a> {
    rib: &'a mut Ribbon,
    budget: Option<Budget>,
}

impl Emu for Reader<'_> {
    fn put(&mut self, bytes: &[u8]) {
        self.rib.feed(bytes);
    }
    fn size(&mut self, cols: u16, rows: u16) {
        self.rib.resize(cols.min(MAX_SIDE) as usize, rows.min(MAX_SIDE) as usize);
    }
    fn step(&self) -> usize {
        // A small page is overshot by at most a quarter.
        self.budget.as_ref().map_or(STEP, |b| (b.cap / 4).clamp(256, STEP))
    }
    fn enough(&mut self) -> bool {
        let Some(b) = &mut self.budget else { return false };
        let w = self.rib.weight();
        let weight = if b.lean { w.signal_bytes } else { w.bytes };
        if weight < b.check_at {
            return false;
        }
        let have = shape(&self.rib.pending(true), b.lean, &b.rules, None).text.len();
        if have >= b.cap {
            return true;
        }
        // Lean text grows faster than the signal lines scrolling past: each
        // brings the line after it and a note for the gap before it.
        b.check_at = weight + ((b.cap - have) / if b.lean { 3 } else { 1 }).max(64);
        false
    }
    fn at_rest(&mut self) -> bool {
        self.rib.at_rest()
    }
}

/// Lines into the text one read returns: lean, plain, or a window of plain.
fn shape(lines: &[String], lean: bool, rules: &Rules, window: Option<(usize, usize)>) -> lean::Lean {
    if let (true, None) = (lean, window) {
        return lean::condense(lines, rules);
    }
    let part: &[String] = match window {
        Some((a, b)) => &lines[(a - 1).min(lines.len())..b.min(lines.len())],
        None => lines,
    };
    // Counted like lean, shown whole.
    let mut l = lean::condense(lines, rules);
    l.text = part.join("\n");
    l.lines_out = part.len();
    l
}

/// The glyph renderer without a window, made on first use and let go after a
/// minute unused.
struct Eyes {
    gpu: crate::gpu::Gpu,
    r: crate::renderer::Renderer,
    px: f32,
    /// The renderer's clock. It only moves forward, a long way per picture,
    /// so every picture is of a settled screen.
    clock: f64,
    used: Instant,
}

// ---------------------------------------------------------------------------

struct Flight {
    cancelled: AtomicBool,
    /// The session a `wait` is blocked on, so that cancelling can wake it.
    sess: Mutex<Option<Arc<Sess>>>,
}

enum Fail {
    Say(String),
    Cancelled,
}

impl From<String> for Fail {
    fn from(s: String) -> Self {
        Fail::Say(s)
    }
}
impl From<&str> for Fail {
    fn from(s: &str) -> Self {
        Fail::Say(s.to_string())
    }
}
impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Say(e.to_string())
    }
}

type Tool = Result<Vec<Value>, Fail>;

fn text(s: impl Into<String>) -> Value {
    json!({ "type": "text", "text": s.into() })
}

/// What the daemon says it is running.
struct Listing {
    pid: u64,
    caps: Vec<String>,
    raw: Value,
}

impl Listing {
    fn get() -> Option<Self> {
        let raw = remote::list().ok()?;
        let caps = raw["caps"].as_array().map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect()).unwrap_or_default();
        Some(Self { pid: raw["pid"].as_u64().unwrap_or(0), caps, raw })
    }

    fn sessions(&self) -> &[Value] {
        self.raw["sessions"].as_array().map(Vec::as_slice).unwrap_or(&[])
    }

    fn entry(&self, id: u64) -> Option<&Value> {
        self.sessions().iter().find(|e| e["id"].as_u64() == Some(id))
    }
}

fn size_of(e: &Value) -> (u16, u16) {
    (e["cols"].as_u64().unwrap_or(80) as u16, e["rows"].as_u64().unwrap_or(24) as u16)
}

pub struct Server {
    drive_all: bool,
    linger: bool,
    out: Mutex<Box<dyn Write + Send>>,
    sessions: Mutex<HashMap<u64, Arc<Sess>>>,
    flights: Mutex<HashMap<String, Arc<Flight>>>,
    eyes: Arc<Mutex<Option<Eyes>>>,
}

impl Server {
    pub fn new(drive_all: bool, linger: bool, out: Box<dyn Write + Send>) -> Arc<Self> {
        Arc::new(Self {
            drive_all,
            linger,
            out: Mutex::new(out),
            sessions: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            eyes: Arc::new(Mutex::new(None)),
        })
    }

    fn say(&self, v: &Value) {
        // serde_json never writes a raw newline, so one message is one line.
        let mut line = v.to_string();
        line.push('\n');
        let mut out = self.out.lock().unwrap();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }

    fn reply(&self, id: &Value, result: Value) {
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn refuse(&self, id: &Value, code: i64, message: &str, data: Option<Value>) {
        let mut error = json!({ "code": code, "message": message });
        if let Some(d) = data {
            error["data"] = d;
        }
        self.say(&json!({ "jsonrpc": "2.0", "id": id, "error": error }));
    }

    fn info() -> Value {
        json!({ "name": "proteus", "title": "PROTEUS terminal", "version": crate::runtime::build_id() })
    }

    /// One message from the client.
    pub fn handle(self: &Arc<Self>, msg: Value) {
        let Some(obj) = msg.as_object() else {
            self.refuse(&Value::Null, -32600, "expected one JSON-RPC message object (batches are not part of MCP)", None);
            return;
        };
        let id = obj.get("id").filter(|v| !v.is_null()).cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            if let (Some(id), false) = (&id, obj.contains_key("result") || obj.contains_key("error")) {
                self.refuse(id, -32600, "no method", None);
            }
            return;
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            if method == "notifications/cancelled" {
                let flight = self.flights.lock().unwrap().get(&params["requestId"].to_string()).cloned();
                if let Some(f) = flight {
                    f.cancelled.store(true, Ordering::Release);
                    if let Some(s) = f.sess.lock().unwrap().as_ref() {
                        let _guard = s.st.lock().unwrap();
                        s.cv.notify_all();
                    }
                }
            }
            return; // a notification gets no answer
        };

        // Which generation is this request speaking? The stateless one says so itself.
        let meta = &params["_meta"];
        let all: Vec<&str> = std::iter::once(MODERN).chain(LEGACY.iter().copied()).collect();
        let modern = match meta.get(META_VERSION) {
            None => false,
            Some(v) if v.as_str() == Some(MODERN) => {
                if meta.get(META_CAPS).is_none() {
                    self.refuse(&id, -32602, &format!("missing required _meta field {META_CAPS}"), None);
                    return;
                }
                true
            }
            Some(v) => {
                self.refuse(&id, -32022, "Unsupported protocol version", Some(json!({ "supported": all, "requested": v })));
                return;
            }
        };
        let done = |mut result: Value| {
            if modern {
                result["resultType"] = json!("complete");
                result["_meta"] = json!({ META_SERVER: Self::info() });
            }
            result
        };
        match (method, modern) {
            ("initialize", false) => {
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                let version = if LEGACY.contains(&asked) { asked } else { LEGACY[0] };
                self.reply(&id, json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": Self::info(),
                    "instructions": INSTRUCTIONS,
                }));
            }
            ("ping", false) => self.reply(&id, json!({})),
            ("server/discover", true) => self.reply(&id, done(json!({
                "supportedVersions": all,
                "capabilities": { "tools": {} },
                "instructions": INSTRUCTIONS,
                "ttlMs": 3_600_000,
                "cacheScope": "public",
            }))),
            ("server/discover", false) => self.refuse(&id, -32602, &format!("missing required _meta field {META_VERSION}"), None),
            ("tools/list", _) => {
                let mut r = json!({ "tools": tools() });
                if modern {
                    r["ttlMs"] = json!(3_600_000);
                    r["cacheScope"] = json!("public");
                }
                self.reply(&id, done(r));
            }
            ("tools/call", _) => {
                let name = params["name"].as_str().unwrap_or("").to_string();
                if !TOOL_NAMES.contains(&name.as_str()) {
                    self.refuse(&id, -32602, &format!("Unknown tool: {name}"), None);
                    return;
                }
                let args = match &params["arguments"] {
                    Value::Null => json!({}),
                    a @ Value::Object(_) => a.clone(),
                    _ => {
                        self.refuse(&id, -32602, "arguments must be an object", None);
                        return;
                    }
                };
                // Each call runs on its own thread: a `wait` may block for minutes
                // while other requests, and its own cancellation, keep arriving.
                let flight = Arc::new(Flight { cancelled: AtomicBool::new(false), sess: Mutex::new(None) });
                self.flights.lock().unwrap().insert(id.to_string(), flight.clone());
                let me = self.clone();
                let spawned = thread::Builder::new().name(format!("tool-{name}")).spawn(move || {
                    let outcome = me.call(&name, &args, &flight);
                    me.flights.lock().unwrap().remove(&id.to_string());
                    let result = match outcome {
                        Ok(content) => json!({ "content": content, "isError": false }),
                        Err(Fail::Say(why)) => json!({ "content": [text(why)], "isError": true }),
                        Err(Fail::Cancelled) => return, // the client has stopped listening
                    };
                    let result = if modern {
                        let mut r = result;
                        r["resultType"] = json!("complete");
                        r["_meta"] = json!({ META_SERVER: Self::info() });
                        r
                    } else {
                        result
                    };
                    me.reply(&id, result);
                });
                if let Err(e) = spawned {
                    crate::paths::log(&format!("mcp: could not start a tool thread: {e}"));
                }
            }
            _ => self.refuse(&id, -32601, &format!("Method not found: {method} (this server speaks MCP {})", all.join(", ")), None),
        }
    }

    fn call(self: &Arc<Self>, name: &str, a: &Value, flight: &Arc<Flight>) -> Tool {
        let a = &check_args(name, a)?;
        match name {
            "sessions" => self.tool_sessions(),
            "spawn" => self.tool_spawn(a),
            "send" => self.tool_send(a),
            "wait" => self.tool_wait(a, flight),
            "read" => self.tool_read(a),
            "screen" => self.tool_screen(a),
            "shot" => self.tool_shot(a),
            "kill" => self.tool_kill(a),
            _ => Err(format!("unknown tool {name}").into()),
        }
    }

    /// The session a call names. A number is looked up in the daemon that is
    /// running now; one this server already knows is kept after it has ended,
    /// so its output can still be read.
    fn resolve(&self, a: &Value) -> Result<Arc<Sess>, Fail> {
        let id = a["session"].as_u64().ok_or("give the session number; `sessions` lists them")?;
        let known = self.sessions.lock().unwrap().get(&id).cloned();
        if let Some(s) = &known {
            // Connected and running: no need to ask anyone.
            let st = s.st.lock().unwrap();
            if st.linked && st.exit.is_none() {
                return Ok(s.clone());
            }
        }
        let listing = Listing::get();
        let entry = listing.as_ref().and_then(|l| l.entry(id).map(|e| (l.pid, e)));
        match (known, entry) {
            (Some(s), Some((pid, _))) if s.daemon == pid => Ok(s),
            (_, Some((pid, e))) => {
                let exe = e["exe"].as_str().unwrap_or("").to_string();
                let journal = PathBuf::from(e["journal"].as_str().unwrap_or(""));
                let s = Arc::new(Sess::new(id, pid, exe, journal, size_of(e), false));
                s.start_waiting_from_now();
                self.sessions.lock().unwrap().insert(id, s.clone());
                Ok(s)
            }
            (Some(s), None) => Ok(s),
            (None, None) => {
                // Ended before this server looked: its journal is still there.
                let pid = listing.as_ref().map(|l| l.pid).unwrap_or(0);
                let suffix = format!("-{pid}-{id}.prtj");
                let found = std::fs::read_dir(crate::paths::journal_dir())
                    .ok()
                    .and_then(|d| d.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&suffix))));
                match found {
                    Some(journal) if pid != 0 => {
                        let s = Arc::new(Sess::new(id, pid, String::new(), journal, (80, 24), false));
                        s.start_waiting_from_now();
                        self.sessions.lock().unwrap().insert(id, s.clone());
                        Ok(s)
                    }
                    _ => Err(format!("there is no session {id}; `sessions` lists the ones that exist").into()),
                }
            }
        }
    }

    /// Is the daemon still running this session? Asking also makes the daemon
    /// write its journals out, so what is read next is current.
    fn running(&self, s: &Sess) -> (bool, Option<(u16, u16)>) {
        match Listing::get() {
            Some(l) if l.pid == s.daemon => match l.entry(s.id) {
                Some(e) => (true, Some(size_of(e))),
                None => (false, None),
            },
            _ => (false, None),
        }
    }

    /// May this server type into the session, and can it? Connects if allowed to.
    fn hands(&self, s: &Arc<Sess>) -> Result<(), Fail> {
        let connected = s.link.lock().unwrap().is_some();
        if let Some(code) = s.st.lock().unwrap().exit {
            return Err(format!("session {} has ended{}", s.id, code.map(|c| format!(" (exit code {c})")).unwrap_or_default()).into());
        }
        if connected && (s.owned || self.drive_all) {
            return Ok(());
        }
        if s.owned {
            return Err(format!("the connection to session {} is gone", s.id).into());
        }
        if !self.drive_all {
            return Err(format!(
                "session {} was not started by this server, so here it can be looked at (screen, shot, read, wait) but not typed into or ended. \
                 Start the server with --drive-all to drive sessions it did not spawn.",
                s.id
            )
            .into());
        }
        // Attaching replays the whole journal once; after that typing is one frame.
        let stream = remote::connect(false)?;
        let mut rd = BufReader::with_capacity(1 << 16, stream.try_clone()?);
        proto::write_frame(&mut BufWriter::new(&stream), proto::C_ATTACH, &s.id.to_le_bytes())?;
        let (ty, p) = proto::read_frame(&mut rd)?;
        if ty != proto::D_INFO {
            return Err(format!("session {}: {}", s.id, String::from_utf8_lossy(&p)).into());
        }
        s.link(stream, rd, true)?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut st = s.st.lock().unwrap();
        while st.replaying && Instant::now() < deadline {
            st = s.cv.wait_timeout(st, Duration::from_millis(200)).unwrap().0;
        }
        Ok(())
    }

    fn tool_sessions(&self) -> Tool {
        let listing = Listing::get();
        let now = crate::paths::unix_secs();
        let known = self.sessions.lock().unwrap().clone();
        let mut out = Vec::new();
        let mut listed = Vec::new();
        if let Some(l) = &listing {
            for e in l.sessions() {
                let id = e["id"].as_u64().unwrap_or(0);
                listed.push(id);
                let mine = known.get(&id).is_some_and(|s| s.daemon == l.pid && s.owned);
                let journal = e["journal"].as_str().unwrap_or("");
                out.push(json!({
                    "session": id,
                    "exe": e["exe"],
                    "cols": e["cols"],
                    "rows": e["rows"],
                    "age_s": now.saturating_sub(e["started_unix"].as_u64().unwrap_or(now)),
                    "windows": e["windows"],
                    "journal": journal,
                    "journal_bytes": std::fs::metadata(journal).map(|m| m.len()).unwrap_or(0),
                    "spawned_here": mine,
                    "can_drive": mine || self.drive_all,
                }));
            }
        }
        // Sessions this server knew that have since ended can still be read.
        let mut ended: Vec<Value> = known
            .values()
            .filter(|s| !listed.contains(&s.id) || listing.as_ref().is_none_or(|l| l.pid != s.daemon))
            .map(|s| json!({ "session": s.id, "exe": s.exe, "ended": true, "exit_code": s.st.lock().unwrap().exit.flatten(), "journal": s.journal.to_string_lossy(), "spawned_here": s.owned }))
            .collect();
        ended.sort_by_key(|v| v["session"].as_u64());
        let daemon = match &listing {
            Some(l) => json!({ "pid": l.pid, "build": l.raw["build"], "caps": l.caps }),
            None => Value::Null,
        };
        Ok(vec![text(json!({ "daemon": daemon, "drive_all": self.drive_all, "sessions": out, "ended": ended }).to_string())])
    }

    fn tool_spawn(&self, a: &Value) -> Tool {
        let command = a["command"].as_str().unwrap_or("").trim();
        let (exe, args) = match a["args"].as_array() {
            Some(list) => (command.to_string(), list.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()),
            None => {
                let mut parts = split_command(command).into_iter();
                match parts.next() {
                    Some(exe) => (exe, parts.collect()),
                    None => {
                        // No command: the person's shell, as a window would start it.
                        let cfg = crate::config::Config::load();
                        (cfg.shell.clone(), if cfg.shell.is_empty() { Vec::new() } else { cfg.shell_args.clone() })
                    }
                }
            }
        };
        let length = exe.len() + args.iter().map(|x| x.len() + 3).sum::<usize>();
        if length > MAX_COMMAND {
            return Err(format!("the command line is {length} characters long; Windows starts at most {MAX_COMMAND}").into());
        }
        let cols = a["cols"].as_u64().unwrap_or(120).clamp(20, 500) as u16;
        let rows = a["rows"].as_u64().unwrap_or(32).clamp(5, 200) as u16;
        let cwd = a["cwd"].as_str().unwrap_or("");
        if !cwd.is_empty() && !std::path::Path::new(cwd).is_dir() {
            return Err(format!("cwd {cwd:?} is not a directory").into());
        }
        let stream = remote::connect(true)?;
        let mut rd = BufReader::with_capacity(1 << 16, stream.try_clone()?);
        let env: std::collections::BTreeMap<String, String> = crate::session::clean_env().into_iter().collect();
        let req = json!({ "exe": exe, "args": args, "cwd": cwd, "cols": cols, "rows": rows, "env": env, "agent": true, "linger": self.linger });
        proto::write_frame(&mut BufWriter::new(&stream), proto::C_SPAWN, &serde_json::to_vec(&req).map_err(|e| e.to_string())?)?;
        let (ty, p) = proto::read_frame(&mut rd)?;
        if ty != proto::D_INFO {
            return Err(format!("could not start {exe:?}: {}", String::from_utf8_lossy(&p)).into());
        }
        let info: Value = serde_json::from_slice(&p).unwrap_or(Value::Null);
        let id = info["id"].as_u64().ok_or("the daemon did not say which session it started")?;
        // From this moment the connection is the session's lifeline: keep reading it.
        let listing = Listing::get();
        let entry = listing.as_ref().and_then(|l| l.entry(id));
        let journal = PathBuf::from(entry.and_then(|e| e["journal"].as_str()).unwrap_or(""));
        let exe = info["exe"].as_str().unwrap_or(&exe).to_string();
        let s = Arc::new(Sess::new(id, listing.as_ref().map(|l| l.pid).unwrap_or(0), exe.clone(), journal.clone(), (cols, rows), true));
        s.link(stream, rd, false)?;
        self.sessions.lock().unwrap().insert(id, s);
        self.forget_old();
        Ok(vec![text(
            json!({
                "session": id, "exe": exe, "args": args, "cols": cols, "rows": rows, "cursor": 0,
                "journal": journal.to_string_lossy(),
                "watch": format!("proteus --attach {id}"),
                "note": "nothing is printed for about 3 s while the console host starts: wait for a prompt (regex) or the exit, not for quiet",
            })
            .to_string(),
        )])
    }

    /// An agent may run hundreds of short commands. What the older ended ones
    /// printed is in their journals; it need not stay in memory as well.
    fn forget_old(&self) {
        let sessions = self.sessions.lock().unwrap();
        let mut ended: Vec<&Arc<Sess>> = sessions.values().filter(|s| s.st.lock().unwrap().exit.is_some()).collect();
        ended.sort_by_key(|s| std::cmp::Reverse(s.id));
        for s in ended.into_iter().skip(KEEP_ENDED) {
            let mut st = s.st.lock().unwrap();
            if st.linked && !st.lost {
                st.linked = false;
                st.mem = Vec::new();
                drop(st);
                // One that is being read this instant keeps its emulators until next time.
                if let Ok(mut lean) = s.lean.try_lock() {
                    *lean = None;
                }
                if let Ok(mut view) = s.view.try_lock() {
                    *view = None;
                }
            }
        }
    }

    fn tool_send(&self, a: &Value) -> Tool {
        let s = self.resolve(a)?;
        let mut bytes = Vec::new();
        if let Some(t) = a["text"].as_str() {
            // In a terminal the Enter key is a carriage return.
            bytes.extend_from_slice(t.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
        }
        let keys: Vec<&str> = match &a["keys"] {
            Value::String(k) => vec![k.as_str()],
            Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for k in keys {
            bytes.extend(key_bytes(k).ok_or_else(|| format!("unknown key {k:?}. Keys: Enter Tab Escape Backspace Space Up Down Left Right Home End PageUp PageDown Insert Delete F1-F12, a single character, with Ctrl+ Alt+ Shift+ in front (Ctrl+C)."))?);
        }
        if a["enter"].as_bool() == Some(true) {
            bytes.push(b'\r');
        }
        if bytes.is_empty() {
            return Err("nothing to send: give text, keys or enter".into());
        }
        self.hands(&s)?;
        // Whatever the session prints from here on is the answer to this input.
        let cursor = {
            let mut st = s.st.lock().unwrap();
            st.wait_pos = st.end;
            st.end
        };
        s.write(proto::C_INPUT, &bytes)?;
        Ok(vec![text(json!({ "session": s.id, "sent_bytes": bytes.len(), "cursor": cursor }).to_string())])
    }

    fn tool_kill(&self, a: &Value) -> Tool {
        let s = self.resolve(a)?;
        self.hands(&s)?;
        s.write(proto::C_KILL, &[])?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut st = s.st.lock().unwrap();
        while st.exit.is_none() && Instant::now() < deadline {
            st = s.cv.wait_timeout(st, deadline.saturating_duration_since(Instant::now())).unwrap().0;
        }
        Ok(vec![text(json!({ "session": s.id, "ended": st.exit.is_some(), "exit_code": st.exit.flatten() }).to_string())])
    }

    fn tool_wait(&self, a: &Value, flight: &Arc<Flight>) -> Tool {
        let s = self.resolve(a)?;
        let until = &a["until"];
        let mut matcher = match until["regex"].as_str() {
            Some(p) => Some(Matcher::new(p).map_err(|e| format!("regex: {e}"))?),
            None => None,
        };
        let quiet = until["quiet_ms"].as_u64().map(Duration::from_millis);
        let want_exit = until["exit"].as_bool() == Some(true);
        if matcher.is_none() && quiet.is_none() && !want_exit {
            return Err("say what to wait for: until {regex: \"...\"}, {quiet_ms: N} or {exit: true}".into());
        }
        let timeout = Duration::from_millis(a["timeout_ms"].as_u64().unwrap_or(30_000).min(3_600_000));
        let from = a["cursor"].as_u64().unwrap_or_else(|| s.st.lock().unwrap().wait_pos);
        *flight.sess.lock().unwrap() = Some(s.clone());

        let t0 = Instant::now();
        let mut pos = from;
        // For a session read from disk: when its journal was last seen to grow.
        let mut seen_growth = t0;
        // A heavy pattern sees less far back than an ordinary one: say so.
        let narrow = matcher.as_ref().filter(|m| m.reach() < BEFORE).map(|m| (m.weight_kb, m.reach()));
        let finish = |reason: &str, pos: u64, hit: Option<Hit>, exit: Option<Option<u32>>, lost: bool| -> Tool {
            let mut v = json!({ "session": s.id, "reason": reason, "elapsed_ms": t0.elapsed().as_millis() as u64, "cursor": pos, "from": from });
            if let Some((kb, reach)) = narrow {
                v["pattern_reach"] = json!(reach);
                v["pattern_note"] = json!(format!(
                    "this pattern compiles to more than {} KB, so it is searched {reach} characters at a time: a match longer than {reach} characters \
                     can be missed. Fewer or smaller counted repeats, or (?-u:\\w) for an ASCII class, make it lighter",
                    kb / 2
                ));
            }
            if let Some(h) = hit {
                v["match"] = json!(h.matched);
                v["line"] = json!(h.line);
                if h.groups.len() > 1 {
                    v["groups"] = json!(h.groups[1..]);
                }
            }
            if let Some(code) = exit {
                v["exited"] = json!(true);
                v["exit_code"] = json!(code);
            }
            if lost {
                v["note"] = json!("the daemon holding this session went away");
            }
            if reason != "timeout" {
                s.st.lock().unwrap().wait_pos = pos;
            }
            Ok(vec![text(v.to_string())])
        };
        // A match still open to change, and since when nothing has changed it.
        let mut tentative: Option<Instant> = None;
        loop {
            let linked = s.linked();
            let running = linked || self.running(&s).0;
            let take = s.take(pos, running)?;
            if !take.bytes.is_empty() {
                seen_growth = Instant::now();
                tentative = None;
            }
            let over = take.exit.is_some() || t0.elapsed() >= timeout;
            if let Some(m) = &mut matcher {
                let scan = m.scan(&take.bytes, pos, t0 + timeout, &flight.cancelled);
                let all = scan.taken == take.bytes.len();
                match scan.hit {
                    Some(hit) if hit.firm || (all && (over || tentative.is_some_and(|t| t.elapsed() >= SETTLE))) => {
                        let at = hit.cursor;
                        return finish("regex", at, Some(hit), take.exit, take.lost);
                    }
                    Some(_) if all => {
                        tentative.get_or_insert_with(Instant::now);
                    }
                    _ => tentative = None,
                }
                if !all {
                    // The pattern is slower than the output, and time is up or the
                    // wait was called off. The cursor says how far the search got:
                    // a wait given it goes on from there.
                    if flight.cancelled.load(Ordering::Acquire) {
                        return Err(Fail::Cancelled);
                    }
                    return finish("timeout", pos + scan.taken as u64, None, None, false);
                }
            }
            pos = take.end;
            if take.exit.is_some() {
                // Nothing more will ever come: whatever was asked for, this is the answer.
                return finish("exit", pos, None, take.exit, take.lost);
            }
            // Silence counts from the later of the call and the last output.
            let last = take.last_out.unwrap_or(seen_growth).max(t0);
            if quiet.is_some_and(|q| last.elapsed() >= q) {
                return finish("quiet", pos, None, None, false);
            }
            if t0.elapsed() >= timeout {
                return finish("timeout", pos, None, None, false);
            }
            if flight.cancelled.load(Ordering::Acquire) {
                return Err(Fail::Cancelled);
            }
            let mut wake = t0 + timeout;
            if let Some(q) = quiet {
                wake = wake.min(last + q);
            }
            if let Some(t) = tentative {
                wake = wake.min(t + SETTLE);
            }
            if linked {
                // Sleep until the connection delivers something, or the deadline.
                let mut st = s.st.lock().unwrap();
                while st.end == pos && st.exit.is_none() && !flight.cancelled.load(Ordering::Acquire) {
                    let now = Instant::now();
                    if now >= wake {
                        break;
                    }
                    st = s.cv.wait_timeout(st, wake - now).unwrap().0;
                }
            } else {
                thread::sleep(FOLLOW.min(wake.saturating_duration_since(Instant::now())));
            }
        }
    }

    fn tool_read(&self, a: &Value) -> Tool {
        let s = self.resolve(a)?;
        let mode = a["mode"].as_str().unwrap_or("lean");
        if !matches!(mode, "lean" | "plain" | "raw") {
            return Err(format!("mode {mode:?}: use \"lean\", \"plain\" (every line, no escape codes) or \"raw\" (the bytes as printed)").into());
        }
        let linked = s.linked();
        let (running, size_now) = if linked { (true, None) } else { self.running(&s) };
        let cursor = a["cursor"].as_u64().unwrap_or_else(|| s.st.lock().unwrap().read_pos);
        let cap = a["max_bytes"].as_u64().map(|n| n.clamp(1_000, 1 << 20) as usize).unwrap_or(PAGE);
        let (end, exit) = s.state(running)?;
        if cursor > end {
            return Err(format!("cursor {cursor} is past the end of the output ({end})").into());
        }
        let mut meta = json!({ "session": s.id, "mode": mode, "from": cursor });
        let (body, next) = if mode == "raw" {
            let (mut bytes, mut next) = s.bytes(cursor, Some(end.min(cursor + cap as u64)))?;
            // Do not cut a character in half; the rest comes with the next read.
            if next < end {
                let tail = bytes.iter().rev().take(4).take_while(|b| **b & 0xC0 == 0x80).count();
                let lead = bytes.len().checked_sub(tail + 1).map(|i| bytes[i]);
                let need = match lead {
                    Some(b) if b >= 0xF0 => 4,
                    Some(b) if b >= 0xE0 => 3,
                    Some(b) if b >= 0xC0 => 2,
                    _ => 0,
                };
                if need > tail + 1 && bytes.len() > tail + 1 {
                    bytes.truncate(bytes.len() - tail - 1);
                    next = cursor + bytes.len() as u64;
                }
            }
            meta["raw_bytes"] = json!(next - cursor);
            (String::from_utf8_lossy(&bytes).into_owned(), next)
        } else {
            let rules = Rules {
                head: a["head"].as_u64().map(|n| n.min(100_000) as usize).unwrap_or(Rules::default().head),
                tail: a["tail"].as_u64().map(|n| n.min(100_000) as usize).unwrap_or(Rules::default().tail),
                max_line: Rules::default().max_line,
            };
            let window = match a["lines"].as_array().map(|v| (v.first().and_then(Value::as_u64), v.get(1).and_then(Value::as_u64))) {
                Some((Some(a), Some(b))) if a >= 1 && b >= a => Some((a as usize, b as usize)),
                Some(_) => return Err("lines: give [first, last], counted from 1".into()),
                None => None,
            };
            let got = self.lean_read(&s, cursor, end, mode == "lean", cap, &rules, window, size_now)?;
            meta["raw_bytes"] = json!(got.next - cursor);
            meta["lines"] = json!(got.lines_in);
            meta["lines_shown"] = json!(got.lines_out);
            meta["errors"] = json!(got.errors);
            meta["warnings"] = json!(got.warnings);
            if got.full_screen {
                meta["full_screen"] = json!(true);
                meta["note"] = json!("a full-screen program is running: use screen or shot to see it");
            }
            if window.is_some() {
                meta["note"] = json!("a window onto earlier output: the read position has not moved");
                meta["lean_bytes"] = json!(got.text.len());
                meta["cursor"] = json!(got.next);
                let mut content = vec![text(meta.to_string())];
                if !got.text.is_empty() {
                    content.push(text(got.text));
                }
                return Ok(content);
            }
            (got.text, got.next)
        };
        let raw = next - cursor;
        meta["lean_bytes"] = json!(body.len());
        meta["saved_pct"] = json!(if raw > 0 { ((1.0 - body.len() as f64 / raw as f64) * 1000.0).round() / 10.0 } else { 0.0 });
        meta["cursor"] = json!(next);
        meta["more_bytes"] = json!(end - next);
        match exit {
            Some(code) => {
                meta["exited"] = json!(true);
                meta["exit_code"] = json!(code);
            }
            None => meta["exited"] = json!(false),
        }
        s.st.lock().unwrap().read_pos = next;
        let mut content = vec![text(meta.to_string())];
        if !body.is_empty() {
            content.push(text(body));
        }
        Ok(content)
    }

    /// Lines printed from `cursor` on, as they finally stood. Stops early,
    /// between two steps, once about `cap` bytes of text have built up; the
    /// rest waits for the next call.
    #[allow(clippy::too_many_arguments)]
    fn lean_read(&self, s: &Arc<Sess>, cursor: u64, end: u64, lean: bool, cap: usize, rules: &Rules, window: Option<(usize, usize)>, size_now: Option<(u16, u16)>) -> Result<LeanOut, Fail> {
        let resizes = s.resizes(u64::MAX);
        let size_at = |off: u64| resizes.iter().take_while(|r| r.0 <= off).last().map(|r| (r.1, r.2)).unwrap_or(s.size0);
        let mut slot = s.lean.lock().unwrap();
        // A window onto earlier output gets a ribbon of its own and leaves the kept one alone.
        let mut own: Option<LeanView> = None;
        let reuse = window.is_none() && slot.as_ref().is_some_and(|v| v.at == cursor && v.size == size_at(cursor));
        if !reuse {
            // Bring a new ribbon to the state the terminal was in at the cursor.
            let first = (resizes[0].1, resizes[0].2);
            let mut view = LeanView { rib: Ribbon::new(first.0.min(MAX_SIDE) as usize, first.1.min(MAX_SIDE) as usize), at: 0, size: first };
            replay(&mut |a, b| Ok(s.bytes(a, Some(b))?.0), &resizes, &mut view.at, cursor, &mut view.size, &mut Reader { rib: &mut view.rib, budget: None })?;
            view.rib.mark(true);
            view.at = cursor;
            if window.is_some() {
                own = Some(view);
            } else {
                *slot = Some(view);
            }
        }
        let Some(v) = own.as_mut().or(slot.as_mut()) else { return Err("no reader".into()) };
        let budget = window.is_none().then_some(Budget { lean, rules: *rules, cap, check_at: if lean { cap / 4 } else { cap } });
        let (mut at, mut size) = (v.at, v.size);
        replay(&mut |a, b| Ok(s.bytes(a, Some(b))?.0), &resizes, &mut at, end, &mut size, &mut Reader { rib: &mut v.rib, budget })?;
        // A resize the journal does not show yet (an older daemon writes it late).
        if let (Some(now), true) = (size_now, at == end) {
            if now != size {
                v.rib.resize(now.0.min(MAX_SIDE) as usize, now.1.min(MAX_SIDE) as usize);
                size = now;
            }
        }
        // Stopped with more to come: the line being written waits for the next
        // read, so no line is ever shown half and then again whole.
        let mut whole = at >= end;
        let mut lines = v.rib.pending(whole);
        if lines.is_empty() && !whole {
            whole = true;
            lines = v.rib.pending(true);
        }
        let l = shape(&lines, lean, rules, window);
        let full_screen = v.rib.full_screen();
        if window.is_none() {
            v.rib.mark(whole);
            v.at = at;
            v.size = size;
        }
        Ok(LeanOut { text: l.text, next: at, lines_in: l.lines_in, lines_out: l.lines_out, errors: l.errors, warnings: l.warnings, full_screen })
    }

    /// Bring the kept emulator for `s` up to now (or build one for a moment
    /// in the past) and hand it to `f` with the output position it shows.
    fn look<R>(&self, s: &Arc<Sess>, at: Option<f64>, f: impl FnOnce(&View, u64) -> R) -> Result<R, Fail> {
        // Asking the daemon first makes the journal current: sizes are only there.
        let (running, size_now) = self.running(s);
        if let Some(secs) = at {
            if !secs.is_finite() || secs < 0.0 {
                return Err("at: seconds since the session started, zero or more".into());
            }
            let until = (secs * 1e6) as u64;
            // The past is read from the journal alone: only it has times.
            let (mut resizes, upto) = {
                let mut tail = s.tail.lock().unwrap();
                tail.refresh()?;
                (tail.resizes(until), tail.offset_at(until))
            };
            if resizes.first().is_none_or(|r| r.0 != 0) {
                resizes.insert(0, (0, s.size0.0, s.size0.1));
            }
            let mut v = View::new((resizes[0].1, resizes[0].2));
            let (mut pos, mut size) = (0u64, v.size);
            replay(&mut |a, b| Ok(s.tail.lock().unwrap().read(a, b)?), &resizes, &mut pos, upto, &mut size, &mut v)?;
            v.term.settle();
            v.fed = pos;
            return Ok(f(&v, pos));
        }
        let (end, _) = s.state(s.linked() || running)?;
        let resizes = s.resizes(u64::MAX);
        let mut slot = s.view.lock().unwrap();
        // Start again if a resize turns out to lie behind what was already replayed.
        let behind = |v: &View| resizes.iter().filter(|r| r.0 < v.fed).count() > v.applied;
        if slot.as_ref().is_none_or(|v| behind(v) || v.fed > end) {
            *slot = Some(View::new((resizes[0].1, resizes[0].2)));
        }
        let Some(v) = slot.as_mut() else { return Err("no view".into()) };
        let (mut pos, mut size) = (v.fed, v.size);
        replay(&mut |a, b| Ok(s.bytes(a, Some(b))?.0), &resizes, &mut pos, end, &mut size, v)?;
        v.fed = pos;
        v.applied = resizes.iter().filter(|r| r.0 <= pos).count();
        v.term.settle();
        if let Some(now) = size_now {
            v.resize(now); // a resize the journal does not show yet
        }
        Ok(f(v, pos))
    }

    fn tool_screen(&self, a: &Value) -> Tool {
        let s = self.resolve(a)?;
        let at = a["at"].as_f64();
        let (mut meta, rows) = self.look(&s, at, |v, end| {
            let (mut meta, rows) = describe(v);
            meta.insert("cursor".into(), json!(end));
            (meta, rows)
        })?;
        meta.insert("session".into(), json!(s.id));
        if let Some(at) = at {
            meta.insert("at".into(), json!(at));
        }
        let mut content = vec![text(Value::Object(meta).to_string())];
        if !rows.is_empty() {
            content.push(text(rows.join("\n")));
        }
        Ok(content)
    }

    fn tool_shot(&self, a: &Value) -> Tool {
        let s = self.resolve(a)?;
        let at = a["at"].as_f64();
        let scale = a["scale"].as_f64().unwrap_or(1.0).clamp(0.5, 3.0) as f32;
        let px = crate::config::Config::load().font_pt * scale;
        let exact = a["exact"].as_bool() == Some(true);
        let eyes = self.eyes.clone();
        let shot = self.look(&s, at, |v, end| -> Result<(Value, Vec<u8>), String> {
            let (meta, _) = describe(v);
            let t = v.term.term.lock().map_err(|_| "terminal lock")?;
            let (w, h, rgba) = render(&eyes, &t, v.size, px).map_err(|e| e.to_string())?;
            let png = encode_png(w, h, &rgba, exact).map_err(|e| e.to_string())?;
            let mut m = json!({ "session": s.id, "at": at, "cursor": end, "width": w, "height": h, "png_bytes": png.len(), "exact": exact });
            for (k, val) in meta {
                m[k] = val;
            }
            Ok((m, png))
        })?;
        let (meta, png) = shot?;
        Ok(vec![text(meta.to_string()), json!({ "type": "image", "data": base64(&png), "mimeType": "image/png" })])
    }

    /// The client has gone. Sessions spawned here have nobody left to drive
    /// them. A current daemon ends them itself when this connection drops;
    /// an older one needs to be told.
    pub fn shutdown(&self) {
        if self.linger {
            return;
        }
        let Some(listing) = Listing::get() else { return };
        if listing.caps.iter().any(|c| c == "agent") {
            return;
        }
        for s in self.sessions.lock().unwrap().values().filter(|s| s.owned && s.daemon == listing.pid) {
            // One a person has a window on is theirs to close.
            if listing.entry(s.id).is_some_and(|e| e["windows"].as_u64().unwrap_or(0) <= 1) {
                let _ = s.write(proto::C_KILL, &[]);
            }
        }
        thread::sleep(Duration::from_millis(150)); // let the daemon read the frames
    }
}

struct LeanOut {
    text: String,
    next: u64,
    lines_in: usize,
    lines_out: usize,
    errors: usize,
    warnings: usize,
    full_screen: bool,
}

type Source<'a> = dyn FnMut(u64, u64) -> Result<Vec<u8>, Fail> + 'a;

/// Replay output `*pos..end` into an emulator in order, applying each resize
/// at the byte where it happened. Ends early if the emulator has had enough,
/// or if the source runs short.
///
/// An early end is placed where no line is half written: once the emulator
/// has had enough, output is fed on up to the next line feed that leaves it
/// at rest (at most `RUN_ON` bytes further). The cursor handed out there
/// means the same to every reader: one that starts at it later, with no
/// memory of this read, sees the same lines this one goes on to see. (A stop
/// in the middle of a line would hold that line back here, while a reader
/// starting there would take it as already told: the line would be lost.)
fn replay(source: &mut Source, resizes: &[(u64, u16, u16)], pos: &mut u64, end: u64, size: &mut (u16, u16), emu: &mut dyn Emu) -> Result<(), Fail> {
    let mut next = resizes.partition_point(|r| r.0 < *pos);
    // Where the emulator said it had enough, once it has.
    let mut closing: Option<u64> = None;
    loop {
        while let Some(r) = resizes.get(next).filter(|r| r.0 <= *pos) {
            if (r.1, r.2) != *size {
                *size = (r.1, r.2);
                emu.size(r.1, r.2);
            }
            next += 1;
        }
        if *pos >= end {
            return Ok(());
        }
        let upto = resizes.get(next).map_or(end, |r| r.0.min(end)).min(*pos + BLOCK as u64);
        let block = source(*pos, upto)?;
        if block.is_empty() {
            return Ok(());
        }
        let mut rest = &block[..];
        while !rest.is_empty() {
            let n = match closing {
                None => emu.step().min(rest.len()),
                // To the end of the line being written, and no further than allowed.
                Some(since) => {
                    let room = (RUN_ON.saturating_sub(*pos - since) as usize).clamp(1, rest.len());
                    rest[..room].iter().position(|&b| b == b'\n').map_or(room, |i| i + 1)
                }
            };
            let (step, tail) = rest.split_at(n);
            emu.put(step);
            *pos += n as u64;
            rest = tail;
            match closing {
                None if !emu.enough() => {}
                None if emu.at_rest() => return Ok(()),
                None => closing = Some(*pos),
                Some(since) if emu.at_rest() || *pos - since >= RUN_ON => return Ok(()),
                Some(_) => {}
            }
        }
    }
}

/// The screen as text, with where the cursor is and what the title says.
fn describe(v: &View) -> (serde_json::Map<String, Value>, Vec<String>) {
    let mut rows = Vec::new();
    let mut meta = serde_json::Map::new();
    if let Ok(t) = v.term.term.lock() {
        let grid = t.grid();
        for r in 0..grid.screen_lines() {
            rows.push(lean::row_text(&grid[Line(r as i32)]).0);
        }
        let c = grid.cursor.point;
        meta.insert("cols".into(), json!(grid.columns()));
        meta.insert("rows".into(), json!(grid.screen_lines()));
        meta.insert("cursor_row".into(), json!(c.line.0 + 1));
        meta.insert("cursor_col".into(), json!(c.column.0 + 1));
        meta.insert("cursor_visible".into(), json!(t.mode().contains(TermMode::SHOW_CURSOR)));
        meta.insert("full_screen".into(), json!(t.mode().contains(TermMode::ALT_SCREEN)));
    }
    meta.insert("title".into(), json!(v.title.lock().unwrap().clone()));
    // Rows below the last one in use say nothing.
    while rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    (meta, rows)
}

fn render(eyes: &Arc<Mutex<Option<Eyes>>>, term: &alacritty_terminal::term::Term<crate::term::Listener>, size: (u16, u16), px: f32) -> Res<(u32, u32, Vec<u8>)> {
    let mut slot = eyes.lock().map_err(|_| "renderer lock")?;
    if slot.is_none() {
        let gpu = crate::gpu::init(crate::gpu::instance(), None)?;
        let r = crate::renderer::Renderer::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb, px, Theme::void_light());
        *slot = Some(Eyes { gpu, r, px, clock: 0.0, used: Instant::now() });
        // Let the GPU go again once nobody has asked for a picture for a minute.
        let eyes = eyes.clone();
        thread::Builder::new().name("eyes-rest".into()).spawn(move || loop {
            thread::sleep(Duration::from_secs(20));
            let Ok(mut slot) = eyes.lock() else { return };
            if slot.as_ref().is_none_or(|e| e.used.elapsed() > Duration::from_secs(60)) {
                *slot = None;
                return;
            }
        })?;
    }
    let e = slot.as_mut().ok_or("renderer")?;
    if (e.px - px).abs() > 0.01 {
        e.r.set_font_px(&e.gpu.queue, px);
        e.px = px;
    }
    let (cols, rows) = (size.0.clamp(2, MAX_SIDE) as usize, size.1.clamp(1, MAX_SIDE) as usize);
    let m = e.r.fonts.metrics;
    let pad = (px * 0.8).round();
    // The picture is exactly the grid: no more columns or rows than the session has.
    let mut w = (cols as f32 * m.cell_w + 2.0 * pad).ceil() as u32 + 1;
    let mut h = (rows as f32 * m.cell_h + 2.0 * pad).ceil() as u32 + 1;
    for _ in 0..4 {
        if w > MAX_PICTURE || h > MAX_PICTURE {
            break;
        }
        e.r.resize(&e.gpu.device, w, h, pad);
        let got = e.r.grid();
        if got == (cols, rows) {
            break;
        }
        w = (w as i64 + ((cols as i64 - got.0 as i64) as f32 * m.cell_w).round() as i64).max(16) as u32;
        h = (h as i64 + ((rows as i64 - got.1 as i64) as f32 * m.cell_h).round() as i64).max(16) as u32;
    }
    // The renderer must hold exactly the session's grid before it is given the session's cells.
    if w > MAX_PICTURE || h > MAX_PICTURE || e.r.grid() != (cols, rows) {
        return Err(format!("a {cols}x{rows} screen at this size of type would be a picture of {w}x{h} pixels; the limit is {MAX_PICTURE} a side. Use a smaller scale, or screen").into());
    }
    e.clock += 100.0;
    e.used = Instant::now();
    e.r.build(&e.gpu.device, &e.gpu.queue, term, e.clock, true);
    Ok(e.r.capture(&e.gpu.device, &e.gpu.queue, e.clock + 30.0))
}

/// Channel values below this are the dark ground, not text.
const DARK: u8 = 64;
/// The ground is drawn with a grain finer than one colour step, so that its
/// gradient does not band on a display. In a file that grain is noise that
/// cannot be compressed (sizes measured with and without it are in
/// forge/lanes/organs.json). Unless the exact pixels are asked for, a pixel
/// dark in all three channels is rounded to the middle of a 16-step band;
/// anything brighter (text, colour, the cursor) is left as drawn.
fn calm(rgb: &mut [u8]) {
    for p in rgb.chunks_exact_mut(3) {
        if p[0] < DARK && p[1] < DARK && p[2] < DARK {
            for c in p {
                *c = (*c & 0xF0) | 8;
            }
        }
    }
}

fn encode_png(w: u32, h: u32, rgba: &[u8], exact: bool) -> Res<Vec<u8>> {
    // The frame is opaque: three channels are the whole picture.
    let mut rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    if !exact {
        calm(&mut rgb);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header()?;
        wr.write_image_data(&rgb)?;
    }
    Ok(out)
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------
// Matching a pattern against output as it arrives.

struct Hit {
    matched: String,
    groups: Vec<Option<String>>,
    line: String,
    /// Output offset just past the piece the match ended in.
    cursor: u64,
    /// The match ends inside a finished line, so more output cannot change
    /// it. Otherwise it touches the end of what has arrived: `$` and `.*`
    /// would match half a line there, and the other half may be on its way.
    firm: bool,
}

struct Matcher {
    re: regex::Regex,
    strip: Strip,
    text: String,
    /// Characters of `text` already thrown away, and searched.
    dropped: usize,
    searched: usize,
    /// (text length, output offset) after each piece fed.
    marks: VecDeque<(usize, u64)>,
    /// The kilobytes the pattern compiles within: how heavy it is.
    weight_kb: usize,
    /// Characters one search may cover: half of them new output, half a look
    /// back. Set by the pattern's weight alone, never by how the search is
    /// going: what a pattern can find must not depend on the clock.
    window: usize,
    /// The last search found a match that more output could still change.
    open: bool,
}

/// What one pass over new output found.
struct Scan {
    hit: Option<Hit>,
    /// How much of the output given was taken in: all of it, unless time ran out.
    taken: usize,
}

/// What one search may cover, in characters, times the kilobytes its pattern
/// compiles within. The slowest search a pattern can be driven to costs time
/// in proportion to its compiled size: of the 37 patterns and 5 texts tried
/// (`measure_pattern_cost`), the worst took 0.077 to 0.082 microseconds a
/// character for each kilobyte. A window of this many characters a kilobyte
/// is then searched in under 90 ms by every pattern tried, and that, not the
/// text that went before, bounds how late a pattern can make a wait. (Pacing
/// the search by how fast earlier text was searched does not: a pattern can
/// be quick on one kind of text and slow on the next. Measured that way: a
/// megabyte it rejected at a glance, then 300 KB it could not, and a 300 ms
/// timeout answered after 51 s.)
const REACH: usize = 1 << 20;
/// The lightest and the heaviest weights told apart, in kilobytes. An
/// ordinary pattern (a prompt, a word, a line of a log) is within the first;
/// past the last only the regex engine's own limit of 10 MB remains.
const WEIGHT_MIN_KB: usize = 8;
const WEIGHT_MAX_KB: usize = 8 << 10;
const WEIGHT_LIMIT_KB: usize = 10 << 10;
/// No window is smaller than this.
const WINDOW_MIN: usize = 96;
/// The most a search goes back before the line the last one ended in.
const BEFORE: usize = 2048;

impl Matcher {
    fn new(pattern: &str) -> Result<Self, regex::Error> {
        // `^` and `$` are line ends: output is lines.
        let build = |limit_kb: Option<usize>| {
            let mut b = regex::RegexBuilder::new(pattern);
            b.multi_line(true);
            if let Some(kb) = limit_kb {
                b.size_limit(kb << 10);
            }
            b.build()
        };
        // Weigh it: the first size, doubling, that it compiles within. (A
        // compile that outgrows its limit stops there, so the ones that fail
        // together cost about as much as the one that succeeds.)
        let mut weight_kb = WEIGHT_MIN_KB;
        let re = loop {
            match build(Some(weight_kb)) {
                Ok(re) => break re,
                Err(regex::Error::CompiledTooBig(_)) if weight_kb < WEIGHT_MAX_KB => weight_kb *= 2,
                Err(regex::Error::CompiledTooBig(_)) => {
                    weight_kb = WEIGHT_LIMIT_KB;
                    break build(None)?;
                }
                Err(e) => return Err(e),
            }
        };
        let window = (REACH / weight_kb).max(WINDOW_MIN);
        Ok(Self { re, strip: Strip::default(), text: String::new(), dropped: 0, searched: 0, marks: VecDeque::new(), weight_kb, window, open: false })
    }

    /// How far back a search goes, in characters. A match no longer than
    /// this is found wherever the pieces fall, if it lies on one line or is
    /// no longer than `BEFORE` either; a longer one only by luck.
    fn reach(&self) -> usize {
        self.window / 2
    }

    /// Take in `bytes` (output from offset `at` on) and search, a piece at a
    /// time, looking at the clock and at `stop` between pieces. A piece is
    /// half the pattern's window, the look back in `search` the other half,
    /// so no single search is longer than the window allows: megabytes of
    /// output are passed in 64 KB pieces for an ordinary pattern and in
    /// pieces of 250 bytes for one of nested counted repeats that compiles to
    /// 2 MB (which needs 150 s for a megabyte in one piece, measured).
    fn scan(&mut self, bytes: &[u8], at: u64, deadline: Instant, stop: &AtomicBool) -> Scan {
        if bytes.is_empty() {
            // Nothing new: only a match left open last time is worth another look.
            let hit = if self.open { self.search() } else { None };
            return Scan { hit, taken: 0 };
        }
        let mut taken = 0;
        loop {
            let n = (self.window / 2).min(bytes.len() - taken);
            self.feed(&bytes[taken..taken + n], at + taken as u64);
            taken += n;
            let hit = self.search();
            let out_of_time = Instant::now() >= deadline || stop.load(Ordering::Acquire);
            if taken == bytes.len() || hit.as_ref().is_some_and(|h| h.firm) || out_of_time {
                return Scan { hit, taken };
            }
        }
    }

    fn feed(&mut self, bytes: &[u8], at: u64) {
        let mut off = at;
        for line in bytes.split_inclusive(|&b| b == b'\n') {
            for piece in line.chunks(4096) {
                self.strip.push(piece, &mut self.text);
                off += piece.len() as u64;
                self.marks.push_back((self.dropped + self.text.len(), off));
            }
        }
    }

    fn search(&mut self) -> Option<Hit> {
        // Look again at the line the last search ended in, and a little before:
        // a match may have been half there. Never further back than the
        // pattern's reach: one endless line is not searched over and over,
        // and every character looked at again costs a heavy pattern dearly.
        let done = self.searched.saturating_sub(self.dropped).min(self.text.len());
        let reach = self.reach();
        let mut start = done.saturating_sub(BEFORE.min(reach));
        while !self.text.is_char_boundary(start) {
            start -= 1;
        }
        let mut floor = done.saturating_sub(reach);
        while !self.text.is_char_boundary(floor) {
            floor += 1;
        }
        start = self.text[..start].rfind('\n').map_or(0, |i| i + 1).max(floor.min(start));
        self.open = false;
        if let Some(c) = self.re.captures_at(&self.text, start) {
            let m = c.get(0)?;
            let end = self.dropped + m.end();
            let cursor = self.marks.iter().find(|(t, _)| *t >= end).or(self.marks.back()).map(|x| x.1).unwrap_or(0);
            let a = self.text[..m.start()].rfind('\n').map_or(0, |i| i + 1);
            let b = self.text[m.end()..].find('\n').map_or(self.text.len(), |i| m.end() + i);
            let clip = |s: &str| s.chars().take(2000).collect::<String>();
            let firm = m.end() < self.text.len() || self.text.ends_with('\n');
            self.open = !firm;
            return Some(Hit {
                matched: clip(m.as_str()),
                groups: c.iter().map(|g| g.map(|g| clip(g.as_str()))).collect(),
                line: clip(&self.text[a..b]),
                cursor,
                firm,
            });
        }
        self.searched = self.dropped + self.text.len();
        if self.text.len() > 1 << 20 {
            let mut cut = self.text.len() - (64 << 10);
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut;
            while self.marks.len() > 1 && self.marks[1].0 <= self.dropped {
                self.marks.pop_front();
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------

/// Split a command line the way a Windows shell would: blanks separate,
/// double quotes group, `\"` is a quote.
fn split_command(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut any) = (false, false);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A named key as the bytes a terminal sends for it.
fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let mut parts: Vec<&str> = name.split('+').collect();
    // "Ctrl++" and a bare "+" are the plus key; "" and "Ctrl+" name no key.
    let key = match parts.pop()? {
        "" => {
            if parts.pop() != Some("") {
                return None;
            }
            "+"
        }
        k => k,
    };
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    for m in parts {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "c" => ctrl = true,
            "alt" | "meta" | "m" => alt = true,
            "shift" | "s" => shift = true,
            _ => return None,
        }
    }
    let modifier = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi = |code: &str, last: char| -> Vec<u8> {
        match (modifier, code) {
            (1, "") => format!("\x1b[{last}"),
            (1, c) => format!("\x1b[{c}{last}"),
            (m, "") => format!("\x1b[1;{m}{last}"),
            (m, c) => format!("\x1b[{c};{m}{last}"),
        }
        .into_bytes()
    };
    let lower = key.to_ascii_lowercase();
    let named = match lower.as_str() {
        "up" => Some(csi("", 'A')),
        "down" => Some(csi("", 'B')),
        "right" => Some(csi("", 'C')),
        "left" => Some(csi("", 'D')),
        "home" => Some(csi("", 'H')),
        "end" => Some(csi("", 'F')),
        "insert" | "ins" => Some(csi("2", '~')),
        "delete" | "del" => Some(csi("3", '~')),
        "pageup" | "pgup" => Some(csi("5", '~')),
        "pagedown" | "pgdn" => Some(csi("6", '~')),
        "f1" | "f2" | "f3" | "f4" if modifier == 1 => Some(vec![0x1b, b'O', b'P' + (lower.as_bytes()[1] - b'1')]),
        "f1" => Some(csi("", 'P')),
        "f2" => Some(csi("", 'Q')),
        "f3" => Some(csi("", 'R')),
        "f4" => Some(csi("", 'S')),
        "f5" => Some(csi("15", '~')),
        "f6" => Some(csi("17", '~')),
        "f7" => Some(csi("18", '~')),
        "f8" => Some(csi("19", '~')),
        "f9" => Some(csi("20", '~')),
        "f10" => Some(csi("21", '~')),
        "f11" => Some(csi("23", '~')),
        "f12" => Some(csi("24", '~')),
        "tab" if shift => Some(b"\x1b[Z".to_vec()),
        _ => None,
    };
    if let Some(b) = named {
        return Some(b);
    }
    let mut base: Vec<u8> = match lower.as_str() {
        "enter" | "return" => vec![b'\r'],
        "tab" => vec![b'\t'],
        "escape" | "esc" => vec![0x1b],
        "backspace" => vec![0x7f],
        "space" => vec![b' '],
        _ => {
            let mut it = key.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None;
            }
            let c = if shift { c.to_ascii_uppercase() } else { c };
            c.to_string().into_bytes()
        }
    };
    if ctrl {
        base = match base[..] {
            [b' '] | [b'@'] => vec![0],
            [b] if b.is_ascii_alphabetic() => vec![b.to_ascii_uppercase() & 0x1f],
            [b @ (b'[' | b'\\' | b']' | b'^' | b'_')] => vec![b & 0x1f],
            [0x7f] => vec![0x08],
            _ => base,
        };
    }
    if alt {
        base.insert(0, 0x1b);
    }
    Some(base)
}

const TOOL_NAMES: &[&str] = &["sessions", "spawn", "send", "wait", "read", "screen", "shot", "kill"];

/// A tool's arguments, checked against the schema the tool itself publishes.
/// A value of the wrong kind is refused with its name, never quietly replaced
/// by a default: `timeout_ms: "5"` would otherwise wait the default 30 s,
/// `at: "3"` would show the present, and a misspelt name would do nothing.
/// Three kindnesses: a whole number written as `5.0` is 5, one key may be
/// given as a string, and a null counts as not given.
fn check_args(tool: &str, args: &Value) -> Result<Value, String> {
    static TOOLS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    let all = TOOLS.get_or_init(tools);
    let schema = all.as_array().and_then(|t| t.iter().find(|t| t["name"] == tool)).map(|t| &t["inputSchema"]);
    check_object("", args, schema.ok_or_else(|| format!("unknown tool {tool}"))?)
}

fn check_object(path: &str, value: &Value, schema: &Value) -> Result<Value, String> {
    let Some(given) = value.as_object() else { return Err(format!("{} must be an object", path.trim_end_matches('.'))) };
    let props = schema["properties"].as_object();
    let mut out = serde_json::Map::new();
    for (k, v) in given.iter().filter(|(_, v)| !v.is_null()) {
        let Some(kind) = props.and_then(|p| p.get(k)) else {
            let known: Vec<&str> = props.map(|p| p.keys().map(String::as_str).collect()).unwrap_or_default();
            let name: String = k.chars().take(60).collect();
            return Err(match known.is_empty() {
                true => format!("unknown argument {path}{name}: nothing is taken here"),
                false => format!("unknown argument {path}{name}; known here: {}", known.join(", ")),
            });
        };
        out.insert(k.clone(), check_value(&format!("{path}{k}"), v, kind)?);
    }
    for r in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !out.contains_key(r) {
            return Err(format!("{path}{r} is required"));
        }
    }
    Ok(Value::Object(out))
}

fn check_value(name: &str, v: &Value, schema: &Value) -> Result<Value, String> {
    let wrong = |what: &str| {
        let got: String = v.to_string().chars().take(60).collect();
        Err(format!("{name} must be {what}; got {got}"))
    };
    match schema["type"].as_str().unwrap_or("") {
        "integer" => match v.as_f64() {
            _ if v.is_u64() => Ok(v.clone()),
            // Exact as a float up to 2^53.
            Some(f) if f >= 0.0 && f.fract() == 0.0 && f <= 9_007_199_254_740_992.0 => Ok(json!(f as u64)),
            _ => wrong("a whole number, zero or more"),
        },
        "number" if v.is_number() => Ok(v.clone()),
        "number" => wrong("a number"),
        "string" if v.is_string() => Ok(v.clone()),
        "string" => wrong("text"),
        "boolean" if v.is_boolean() => Ok(v.clone()),
        "boolean" => wrong("true or false"),
        "array" => {
            let one = [v.clone()];
            let items: &[Value] = match v {
                Value::Array(list) => list,
                Value::String(_) if schema["items"]["type"] == "string" => &one,
                _ => return wrong("a list"),
            };
            let (least, most) = (schema["minItems"].as_u64().unwrap_or(0), schema["maxItems"].as_u64().unwrap_or(u64::MAX));
            if (items.len() as u64) < least || items.len() as u64 > most {
                return wrong(&format!("a list of {least} to {most} items"));
            }
            let checked: Result<Vec<Value>, String> = items.iter().enumerate().map(|(i, x)| check_value(&format!("{name}[{i}]"), x, &schema["items"])).collect();
            Ok(Value::Array(checked?))
        }
        "object" => check_object(&format!("{name}."), v, schema),
        _ => Ok(v.clone()),
    }
}

fn tools() -> Value {
    let session = || json!({ "type": "integer", "description": "Session number, from spawn or sessions." });
    json!([
        {
            "name": "sessions",
            "title": "List terminal sessions",
            "description": "List the terminal sessions the PROTEUS daemon is running: number, program, size, age, journal file, and whether this server may drive it. Sessions this server knew that have ended are listed too; their output can still be read.",
            "inputSchema": { "type": "object", "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "spawn",
            "title": "Start a terminal session",
            "description": "Start a program on a real terminal and get its session number. With no command it starts the person's shell. The session runs in the PROTEUS daemon; a person can watch it live with `proteus --attach N`. It ends when it exits, is killed, or this server stops. Then: send, wait, read. A new session prints nothing for about 3 seconds while the console host starts, so wait for the prompt with a regex (or for the exit), not for quiet; input sent meanwhile is not lost.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Program and arguments, e.g. \"cmd.exe\", \"pwsh -NoLogo\", \"cargo test\". Double quotes group." },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments given exactly; command is then the program alone." },
                    "cwd": { "type": "string", "description": "Directory to start in." },
                    "cols": { "type": "integer", "description": "Width in characters. Default 120." },
                    "rows": { "type": "integer", "description": "Height in lines. Default 32." }
                }
            }
        },
        {
            "name": "send",
            "title": "Type into a session",
            "description": "Type into a session: text exactly as given (a newline in it presses Enter), then the named keys, then Enter if asked. Returns a cursor: the output position at that moment, which is where wait starts looking. Only sessions this server spawned accept input, unless it runs with --drive-all.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session(),
                    "text": { "type": "string", "description": "Characters to type." },
                    "keys": { "type": "array", "items": { "type": "string" }, "description": "Keys to press after the text: Enter Tab Escape Backspace Space Up Down Left Right Home End PageUp PageDown Insert Delete F1-F12 or one character, optionally with Ctrl+ Alt+ Shift+ (\"Ctrl+C\")." },
                    "enter": { "type": "boolean", "description": "Press Enter at the end." }
                },
                "required": ["session"]
            }
        },
        {
            "name": "wait",
            "title": "Wait for output, silence or exit",
            "description": "Block until something happens in a session, instead of sleeping and looking. `until` takes any of: regex (searched in the output with escape codes removed, ^ and $ at line ends; the typed command is echoed, so anchor with ^ to match what the program prints rather than the echo), quiet_ms (no output for that long, counted from the later of this call and the last output), exit. The first one met wins; the exit always ends a wait. It looks at output from the last send (or the last wait) on, so nothing printed in between is missed; in a session this server did not spawn, from the moment it first looked at it. Returns reason (regex | quiet | exit | timeout), the match and its line, elapsed_ms and a cursor. It does not consume output: read still returns everything since the last read. No pattern holds a wait past timeout_ms: it is searched through a window sized by how large it compiles, 64,000 characters back for an ordinary pattern, a few hundred for a heavy one (counted repeats inside counted repeats; the result then carries pattern_reach), and a search that time overtakes ends with reason timeout and the cursor it got to.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session(),
                    "until": {
                        "type": "object",
                        "properties": {
                            "regex": { "type": "string" },
                            "quiet_ms": { "type": "integer" },
                            "exit": { "type": "boolean" }
                        }
                    },
                    "timeout_ms": { "type": "integer", "description": "Give up after this long. Default 30000." },
                    "cursor": { "type": "integer", "description": "Start looking at this output position instead." }
                },
                "required": ["session", "until"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "read",
            "title": "Read a session's output",
            "description": "Output since the last read (or since `cursor`). mode \"lean\" (default) is what the lines finally looked like, shortened: escape codes gone, a progress bar or spinner is its last state, 3+ identical lines and 4+ lines differing only in numbers are counted, and when more than head+tail lines remain only the first `head` (20), the last `tail` (40) and every line that looks like an error or warning are kept, with `[... N lines omitted (a-b) ...]` in each gap. The shortening never drops an error or warning line (text the program itself erased or overwrote before the read is gone, as on screen). mode \"plain\" is every line with nothing left out; with lines [a, b] it returns just those lines of the output after `cursor`, which is how to see an omitted gap. mode \"raw\" is the bytes as printed, escape codes included. The first block of the result is one line of JSON: raw_bytes and lean_bytes (what this call consumed and returned), cursor (pass it to continue), more_bytes (output not yet read: call again), exited. A read returns about max_bytes of text and leaves the rest for the next call; it stops where a line ends, so its cursor can be given to any later read, by this server or another, and no line is lost or told twice.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session(),
                    "mode": { "type": "string", "enum": ["lean", "plain", "raw"] },
                    "cursor": { "type": "integer", "description": "Output position to read from. Default: where the last read ended." },
                    "max_bytes": { "type": "integer", "description": "About how much text to return. Default 24000." },
                    "head": { "type": "integer" },
                    "tail": { "type": "integer" },
                    "lines": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2, "description": "[first, last] line of the output after `cursor`, from 1. Does not move the read position." }
                },
                "required": ["session"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "screen",
            "title": "See a session's screen as text",
            "description": "The screen exactly as a person sees it: every row of the grid as text, plus size, cursor position and window title. Use it for full-screen programs (editors, pagers, menus) and to check where a prompt stands. With `at`, the screen as it stood that many seconds into the session.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session(), "at": { "type": "number", "description": "Seconds since the session started." } },
                "required": ["session"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "shot",
            "title": "See a session's screen as a picture",
            "description": "The screen as a PNG, drawn by the renderer the PROTEUS window uses: colours, box drawing, layout. Use it when the look matters; screen is cheaper. With `at`, as it stood that many seconds into the session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session(),
                    "at": { "type": "number", "description": "Seconds since the session started." },
                    "scale": { "type": "number", "description": "1 (default) draws the configured font size; 2 doubles it." },
                    "exact": { "type": "boolean", "description": "Keep the renderer's pixels untouched. By default the fine grain of the dark background is flattened, which makes the file several times smaller and changes no text." }
                },
                "required": ["session"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "kill",
            "title": "End a session",
            "description": "End a session's program. Only sessions this server spawned, unless it runs with --drive-all.",
            "inputSchema": { "type": "object", "properties": { "session": session() }, "required": ["session"] },
            "annotations": { "destructiveHint": true }
        }
    ])
}

/// The longest message taken from the client. (The largest thing one
/// carries is text to type.) Without a limit, a client that never ended its
/// line would fill memory until the server, and its sessions, were gone.
const MAX_MESSAGE: usize = 64 << 20;

#[derive(Debug, PartialEq)]
enum Got {
    Whole,
    /// Longer than allowed: read past and thrown away, never held.
    TooLong,
    End,
}

/// The next line of `input` into `line`, holding at most `max` bytes of it.
fn next_line(input: &mut impl BufRead, line: &mut Vec<u8>, max: usize) -> io::Result<Got> {
    line.clear();
    if input.by_ref().take(max as u64 + 1).read_until(b'\n', line)? == 0 {
        return Ok(Got::End);
    }
    if line.len() <= max || line.ends_with(b"\n") {
        return Ok(Got::Whole);
    }
    *line = Vec::new();
    loop {
        let buf = input.fill_buf()?;
        let (n, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), buf.is_empty()),
        };
        input.consume(n);
        if done {
            return Ok(Got::TooLong);
        }
    }
}

#[cfg(windows)]
extern "system" {
    fn GetStdHandle(which: u32) -> isize;
    fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
}

/// The client's pipes are this process's and nobody else's. They arrive
/// marked to be inherited, and a process started from here inherits every
/// handle so marked: the daemon, which the first connection may start and
/// which then lives for hours, would hold the other end of the pipe the
/// client reads. That pipe would not read end-of-file when this server
/// leaves, only when the daemon does.
fn keep_the_pipes_to_ourselves() {
    #[cfg(windows)]
    unsafe {
        const HANDLE_FLAG_INHERIT: u32 = 1;
        // Standard input, output and error.
        for which in [-10i32, -11, -12] {
            let handle = GetStdHandle(which as u32);
            if handle != 0 && handle != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// `proteus mcp [--drive-all] [--linger]`
pub fn run(args: &[String]) -> Res<()> {
    keep_the_pipes_to_ourselves();
    let (mut drive_all, mut linger) = (false, false);
    for a in args {
        match a.as_str() {
            "--drive-all" => drive_all = true,
            "--linger" => linger = true,
            other => return Err(format!("unknown option {other}; usage: proteus mcp [--drive-all] [--linger]").into()),
        }
    }
    let server = Server::new(drive_all, linger, Box::new(io::stdout()));
    crate::paths::log(&format!("mcp: start pid={} drive_all={drive_all} linger={linger}", std::process::id()));
    // Read as bytes: a line that is not UTF-8 is a message that does not
    // parse, not the end of the conversation (and of every session spawned here).
    let mut input = io::stdin().lock();
    let mut line = Vec::new();
    loop {
        match next_line(&mut input, &mut line, MAX_MESSAGE) {
            Ok(Got::End) | Err(_) => break,
            Ok(Got::TooLong) => {
                server.refuse(&Value::Null, -32700, &format!("Parse error: a message longer than {} MB", MAX_MESSAGE >> 20), None);
                continue;
            }
            Ok(Got::Whole) => {}
        }
        let message = line.trim_ascii();
        if message.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Value>(message) {
            Ok(msg) => server.handle(msg),
            Err(e) => server.refuse(&Value::Null, -32700, &format!("Parse error: {e}"), None),
        }
    }
    // Standard input closed: the client has gone.
    server.shutdown();
    crate::paths::log("mcp: client gone, leaving");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_split_like_a_shell() {
        assert_eq!(split_command("cmd.exe"), ["cmd.exe"]);
        assert_eq!(split_command("  pwsh  -NoLogo -Command \"Get-Date; 'a b'\" "), ["pwsh", "-NoLogo", "-Command", "Get-Date; 'a b'"]);
        assert_eq!(split_command(r#""C:\Program Files\Git\bin\git.exe" log --format="%h \"%s\"" """#), [r"C:\Program Files\Git\bin\git.exe", "log", r#"--format=%h "%s""#, ""]);
        assert!(split_command("   ").is_empty());
    }

    #[test]
    fn keys_are_what_a_terminal_sends() {
        let k = |n: &str| key_bytes(n).map(|b| String::from_utf8_lossy(&b).into_owned());
        assert_eq!(k("Enter").as_deref(), Some("\r"));
        assert_eq!(k("Ctrl+C").as_deref(), Some("\x03"));
        assert_eq!(k("ctrl+d").as_deref(), Some("\x04"));
        assert_eq!(k("Ctrl+[").as_deref(), Some("\x1b"));
        assert_eq!(k("Escape").as_deref(), Some("\x1b"));
        assert_eq!(k("Up").as_deref(), Some("\x1b[A"));
        assert_eq!(k("Ctrl+Right").as_deref(), Some("\x1b[1;5C"));
        assert_eq!(k("Shift+Tab").as_deref(), Some("\x1b[Z"));
        assert_eq!(k("PageDown").as_deref(), Some("\x1b[6~"));
        assert_eq!(k("Shift+Delete").as_deref(), Some("\x1b[3;2~"));
        assert_eq!(k("F1").as_deref(), Some("\x1bOP"));
        assert_eq!(k("F4").as_deref(), Some("\x1bOS"));
        assert_eq!(k("F5").as_deref(), Some("\x1b[15~"));
        assert_eq!(k("Ctrl+F5").as_deref(), Some("\x1b[15;5~"));
        assert_eq!(k("Alt+x").as_deref(), Some("\x1bx"));
        assert_eq!(k("Backspace").as_deref(), Some("\x7f"));
        assert_eq!(k("q").as_deref(), Some("q"));
        assert_eq!(k("Shift+q").as_deref(), Some("Q"));
        assert_eq!(k("+").as_deref(), Some("+"));
        assert_eq!(k("Ctrl+Space").as_deref(), Some("\0"));
        assert_eq!(k("é").as_deref(), Some("é"));
        assert_eq!(k("Ctrl++").as_deref(), Some("+"));
        assert_eq!(k("Hyper+x"), None);
        assert_eq!(k("NoSuchKey"), None);
        assert_eq!(k(""), None, "no key is not the plus key");
        assert_eq!(k("Ctrl+"), None);
    }

    #[test]
    fn only_the_dark_ground_is_flattened_in_a_picture() {
        // Grain around the ground colour becomes one value; text and colour are untouched.
        let mut px = [5, 8, 13, 6, 7, 12, 4, 6, 14, 63, 63, 63, 64, 0, 0, 200, 10, 10, 230, 230, 230, 0, 0, 0];
        calm(&mut px);
        assert_eq!(px, [8, 8, 8, 8, 8, 8, 8, 8, 8, 56, 56, 56, 64, 0, 0, 200, 10, 10, 230, 230, 230, 8, 8, 8]);
        let rgba = [5u8, 8, 13, 255, 200, 10, 10, 255];
        let exact = encode_png(2, 1, &rgba, true).unwrap();
        let small = encode_png(2, 1, &rgba, false).unwrap();
        let decode = |b: &[u8]| {
            let mut r = png::Decoder::new(std::io::Cursor::new(b.to_vec())).read_info().unwrap();
            let mut buf = vec![0; r.output_buffer_size().unwrap()];
            let n = r.next_frame(&mut buf).unwrap().buffer_size();
            buf[..n].to_vec()
        };
        assert_eq!(decode(&exact), [5, 8, 13, 200, 10, 10]);
        assert_eq!(decode(&small), [8, 8, 8, 200, 10, 10]);
    }

    #[test]
    fn arguments_are_checked_against_the_schema() {
        let ok = |tool: &str, a: Value| check_args(tool, &a).unwrap_or_else(|e| panic!("{tool} {a}: {e}"));
        let no = |tool: &str, a: Value| check_args(tool, &a).expect_err(&format!("{tool} {a} should be refused"));
        // What the tools are really called with passes through unchanged.
        let w = json!({ "session": 3, "until": { "regex": ">$", "quiet_ms": 200, "exit": true }, "timeout_ms": 20000, "cursor": 0 });
        assert_eq!(ok("wait", w.clone()), w);
        let r = json!({ "session": 3, "mode": "plain", "cursor": 10, "max_bytes": 4000, "head": 5, "tail": 5, "lines": [1, 3] });
        assert_eq!(ok("read", r.clone()), r);
        assert_eq!(ok("sessions", json!({})), json!({}));
        assert_eq!(ok("screen", json!({ "session": 1, "at": 0.02 })), json!({ "session": 1, "at": 0.02 }));
        assert_eq!(ok("screen", json!({ "session": 1, "at": 3 }))["at"], 3);
        // The three kindnesses.
        assert_eq!(ok("wait", json!({ "session": 3.0, "until": { "quiet_ms": 2e2 }, "timeout_ms": null })), json!({ "session": 3, "until": { "quiet_ms": 200 } }));
        assert_eq!(ok("send", json!({ "session": 1, "keys": "Enter" })), json!({ "session": 1, "keys": ["Enter"] }));
        // A wrong kind is named, not replaced by a default.
        assert_eq!(no("wait", json!({ "session": 1, "until": { "quiet_ms": 100 }, "timeout_ms": "5" })), "timeout_ms must be a whole number, zero or more; got \"5\"");
        assert_eq!(no("wait", json!({ "session": 1, "until": { "quiet_ms": -5 } })), "until.quiet_ms must be a whole number, zero or more; got -5");
        assert_eq!(no("wait", json!({ "session": 1, "until": "x" })), "until must be an object");
        assert_eq!(no("wait", json!({ "session": 1, "until": { "regex": 5 } })), "until.regex must be text; got 5");
        assert_eq!(no("wait", json!({ "session": 1, "until": { "exit": "yes" } })), "until.exit must be true or false; got \"yes\"");
        assert_eq!(no("wait", json!({ "session": 1 })), "until is required");
        assert_eq!(no("screen", json!({ "session": 1, "at": "3" })), "at must be a number; got \"3\"");
        assert_eq!(no("screen", json!({ "session": "1" })), "session must be a whole number, zero or more; got \"1\"");
        assert_eq!(no("screen", json!({ "session": 1.5 })), "session must be a whole number, zero or more; got 1.5");
        assert_eq!(no("screen", json!({ "session": 1e30 })), "session must be a whole number, zero or more; got 1e+30");
        assert_eq!(no("screen", json!({})), "session is required");
        assert_eq!(no("spawn", json!({ "command": 5 })), "command must be text; got 5");
        assert_eq!(ok("spawn", json!({ "command": "cmd.exe", "args": "/c" }))["args"], json!(["/c"]));
        assert_eq!(no("spawn", json!({ "command": "cmd.exe", "args": 7 })), "args must be a list; got 7");
        assert_eq!(no("spawn", json!({ "command": "cmd.exe", "args": [1] })), "args[0] must be text; got 1");
        assert_eq!(no("send", json!({ "session": 1, "keys": [5] })), "keys[0] must be text; got 5");
        assert_eq!(no("read", json!({ "session": 1, "lines": [1] })), "lines must be a list of 2 to 2 items; got [1]");
        assert_eq!(no("read", json!({ "session": 1, "lines": ["a", "b"] })), "lines[0] must be a whole number, zero or more; got \"a\"");
        assert_eq!(no("shot", json!({ "session": 1, "exact": "yes" })), "exact must be true or false; got \"yes\"");
        // A name the tool does not know is a mistake worth hearing about.
        assert_eq!(no("wait", json!({ "session": 1, "until": { "quiet_ms": 1 }, "timeout": 5 })), "unknown argument timeout; known here: cursor, session, timeout_ms, until");
        assert_eq!(no("wait", json!({ "session": 1, "until": { "quiet": 1 } })), "unknown argument until.quiet; known here: exit, quiet_ms, regex");
        assert_eq!(no("sessions", json!({ "all": true })), "unknown argument all: nothing is taken here");
        // What is echoed back stays short, whatever was sent.
        assert!(no("screen", json!({ "session": "x".repeat(1 << 20) })).len() < 140);
        let mut odd = serde_json::Map::new();
        odd.insert("x".repeat(1 << 20), json!(1));
        assert!(no("screen", Value::Object(odd)).len() < 200);
    }

    /// The emulator behind `screen` holds the screen and a little above it,
    /// not the window's 50,000 lines.
    #[test]
    fn the_screen_emulator_keeps_little_scrollback() {
        let mut v = View::new((500, 40));
        let line = format!("{}\r\n", "x".repeat(400));
        for _ in 0..60 {
            v.put(line.repeat(1000).as_bytes());
        }
        let t = v.term.term.lock().unwrap();
        assert_eq!(t.grid().history_size(), SCROLLBACK, "60,000 lines went by");
        // The cursor is drawn as the window draws it.
        assert_eq!(t.cursor_style().shape, ansi::CursorShape::Beam);
        drop(t);
        let (meta, rows) = describe(&v);
        assert_eq!((meta["rows"].as_u64(), rows.len(), rows[38].len()), (Some(40), 39, 400));
    }

    /// A journal is indexed a window at a time; records that straddle a
    /// window, or are larger than one, are indexed all the same.
    #[test]
    fn a_long_journal_is_indexed_in_windows() {
        let p = std::env::temp_dir().join(format!("proteus-index-test-{}.prtj", std::process::id()));
        let mut w = journal::JournalWriter::create(&p, &json!({ "v": 1 })).unwrap();
        w.record(journal::K_RESIZE, &crate::session::resize_payload(80, 24)).unwrap();
        let mut all = Vec::new();
        let mut put = |w: &mut journal::JournalWriter, n: usize, tag: u8| {
            let rec: Vec<u8> = (0..n).map(|i| tag.wrapping_add((i % 251) as u8)).collect();
            w.record(journal::K_OUTPUT, &rec).unwrap();
            all.extend_from_slice(&rec);
        };
        for i in 0..7 {
            put(&mut w, 1_300_000, i); // 9.1 MB: the third and the sixth straddle a 4 MB window
        }
        put(&mut w, INDEX_BLOCK + 500_000, 99); // one record larger than a window
        put(&mut w, 10, 7);
        w.record(journal::K_EXIT, &3u32.to_le_bytes()).unwrap();
        w.flush().unwrap();
        let mut tail = Tail::new(p.clone());
        tail.refresh().unwrap();
        assert_eq!((tail.out, tail.exit, tail.marks.len()), (all.len() as u64, Some(Some(3)), 10));
        for (a, b) in [(0u64, 10u64), (1_299_990, 1_300_010), (3_999_000, 4_100_000), (9_099_990, 9_100_020), (all.len() as u64 - 30, all.len() as u64)] {
            assert_eq!(tail.read(a, b).unwrap(), &all[a as usize..b as usize], "{a}..{b}");
        }
        // Half a record at the end of the file is left for the next look.
        drop(w);
        let mut bytes = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        bytes.write_all(&[journal::K_OUTPUT, 1, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, b'x']).unwrap();
        drop(bytes);
        tail.refresh().unwrap();
        assert_eq!(tail.out, all.len() as u64);
        let _ = std::fs::remove_file(&p);
    }

    /// The cursor a read hands out when it stops early must mean the same to
    /// every reader. Real compiler output (forge/samples/organs/cargo-build.vt)
    /// is put in a real journal and read in pages of many sizes; then a second
    /// reader, with no memory of the first, starts at each page's cursor.
    #[test]
    fn a_reader_started_at_any_page_cursor_sees_what_the_paging_reader_went_on_to_see() {
        let raw: &[u8] = include_bytes!("../forge/samples/organs/cargo-build.vt");
        let p = std::env::temp_dir().join(format!("proteus-pages-test-{}.prtj", std::process::id()));
        let mut w = journal::JournalWriter::create(&p, &json!({ "v": 1 })).unwrap();
        w.record(journal::K_RESIZE, &crate::session::resize_payload(120, 8)).unwrap();
        for piece in raw.chunks(333) {
            w.record(journal::K_OUTPUT, piece).unwrap();
        }
        w.flush().unwrap();
        let (server, _) = server();
        let rules = Rules::default();
        let end = raw.len() as u64;
        let reader = || Arc::new(Sess::new(1, 1, String::new(), p.clone(), (120, 8), false));
        let all = server.lean_read(&reader(), 0, end, false, 1 << 20, &rules, None, None).unwrap_or_else(|_| panic!("read"));
        // A blank line at the end of a text is an empty piece after its last line break.
        fn lines_of(text: &str) -> Vec<&str> {
            if text.is_empty() {
                Vec::new()
            } else {
                text.split('\n').collect()
            }
        }
        let whole = lines_of(&all.text);
        assert!(whole.len() > 150 && all.next == end);
        let signals = whole.iter().filter(|l| lean::signal(l) != lean::Signal::None).count();
        assert!(signals >= 29, "3 error lines and the warnings of this capture: {signals}");

        let mut cuts = 0;
        for cap in (1000..3000).step_by(41) {
            for lean_mode in [false, true] {
                // One reader pages through from the top.
                let pager = reader();
                let mut pages: Vec<(u64, Vec<String>)> = Vec::new();
                let mut at = 0u64;
                while at < end {
                    let got = server.lean_read(&pager, at, end, lean_mode, cap, &rules, None, None).unwrap_or_else(|_| panic!("read"));
                    assert!(got.next > at, "a read moves on");
                    at = got.next;
                    pages.push((at, lines_of(&got.text).into_iter().map(String::from).collect()));
                }
                if !lean_mode {
                    let told: Vec<&str> = pages.iter().flat_map(|p| p.1.iter().map(String::as_str)).collect();
                    let d = told.iter().zip(&whole).position(|(a, b)| a != b).unwrap_or(told.len().min(whole.len()));
                    assert!(told == whole, "pages of {cap}: every line once, in order; first difference at line {d}: {:?} against {:?}; pages end at {:?}",
                        &told[d.saturating_sub(2)..(d + 3).min(told.len())], &whole[d.saturating_sub(2)..(d + 3).min(whole.len())], pages.iter().map(|p| (p.0, p.1.len())).collect::<Vec<_>>());
                }
                let kept = pages.iter().flat_map(|p| p.1.iter()).filter(|l| lean::signal(l) != lean::Signal::None).count();
                assert_eq!(kept, signals, "pages of {cap}, lean {lean_mode}: every error and warning line, once");
                // A stranger starts at each cursor handed out on the way.
                for (i, (cursor, _)) in pages.iter().enumerate().take(pages.len() - 1) {
                    cuts += 1;
                    let fresh = server.lean_read(&reader(), *cursor, end, lean_mode, 1 << 20, &rules, None, None).unwrap_or_else(|_| panic!("read"));
                    let want: Vec<&str> = pages[i + 1..].iter().flat_map(|p| p.1.iter().map(String::as_str)).collect();
                    let got = lines_of(&fresh.text);
                    if lean_mode {
                        // Lean shortens each read by itself; what must agree is the errors and warnings.
                        let sig = |v: &[&str]| v.iter().filter(|l| lean::signal(l) != lean::Signal::None).map(|l| l.to_string()).collect::<Vec<_>>();
                        assert_eq!(sig(&got), sig(&want), "pages of {cap}: from cursor {cursor} a line was lost or told twice");
                    } else {
                        assert_eq!(got, want, "pages of {cap}: from cursor {cursor} a line was lost or told twice");
                    }
                }
            }
        }
        assert!(cuts > 100, "{cuts} page ends tried");
        let _ = std::fs::remove_file(&p);
    }

    /// A pattern that takes minutes per megabyte gives way to the clock: on
    /// text it is slow on from the first byte, and on text that turns slow
    /// only after a megabyte it rejected at a glance.
    #[test]
    fn a_slow_pattern_is_searched_in_pieces_that_watch_the_clock() {
        let slow_text = format!("{} \r\n", "a".repeat(90)).repeat(3000).into_bytes(); // 279 KB
        let never = AtomicBool::new(false);
        let heavy = "(?:(?:(?:a{1,30}){1,30}){1,30})Q";
        let mut slow = Matcher::new(heavy).unwrap();
        assert_eq!((slow.weight_kb, slow.window), (2048, 512), "it compiles to between 1 and 2 MB");
        let t = Instant::now();
        let scan = slow.scan(&slow_text, 0, t + Duration::from_millis(150), &never);
        let took = t.elapsed();
        assert!(scan.hit.is_none() && scan.taken > 0 && scan.taken < slow_text.len(), "{} of {} taken", scan.taken, slow_text.len());
        assert!(took < Duration::from_millis(2500), "150 ms were allowed, {took:?} taken");
        // Called off: it stops at the next piece.
        let stop = AtomicBool::new(true);
        let t = Instant::now();
        let more = slow.scan(&slow_text[scan.taken..], scan.taken as u64, t + Duration::from_secs(60), &stop);
        assert!(more.taken <= 256 && t.elapsed() < Duration::from_millis(2500));

        // The turn. A search paced by how fast the text before was searched
        // gives the slow text a piece sized for the quick text: 51 s here.
        let mut turn = format!("{} \r\n", "x".repeat(90)).repeat(11_300).into_bytes(); // 1 MB
        let quick_part = turn.len();
        turn.extend_from_slice(&slow_text);
        let mut m = Matcher::new(heavy).unwrap();
        let t = Instant::now();
        let scan = m.scan(&turn, 0, t + Duration::from_millis(400), &never);
        let took = t.elapsed();
        println!("the turn: 400 ms allowed, {took:?} taken, {} of {} bytes searched ({quick_part} of them quick)", scan.taken, turn.len());
        assert!(scan.hit.is_none() && scan.taken < turn.len(), "{} of {} taken", scan.taken, turn.len());
        // (Generous: the machine this runs on may be busy. Paced by the past, it was 51 s.)
        assert!(took < Duration::from_millis(3000), "400 ms were allowed, {took:?} taken");

        // An ordinary pattern finds what a single search finds.
        let mut quick = Matcher::new(r"^(a+) $").unwrap();
        let scan = quick.scan(&slow_text, 0, Instant::now() + Duration::from_secs(60), &never);
        let hit = scan.hit.expect("the first line");
        assert_eq!((hit.matched.len(), hit.firm, hit.cursor), (91, true, 93));
        let mut none = Matcher::new("no such text").unwrap();
        let scan = none.scan(&turn, 0, Instant::now() + Duration::from_secs(60), &never);
        assert!(scan.hit.is_none() && scan.taken == turn.len());
        // A match that ends where the text ends stays open, and is looked at again when nothing new comes.
        let mut prompt = Matcher::new(r">\s*$").unwrap();
        let first = prompt.scan(b"C:\\work>", 0, Instant::now() + Duration::from_secs(60), &never);
        assert!(first.hit.is_some_and(|h| !h.firm));
        assert!(prompt.scan(b"", 8, Instant::now() + Duration::from_secs(60), &never).hit.is_some_and(|h| !h.firm && h.cursor == 8));
        assert!(none.scan(b"", 0, Instant::now() + Duration::from_secs(60), &never).hit.is_none());
    }

    /// How far a search may reach is set by how large its pattern compiles,
    /// before a byte of output is seen.
    #[test]
    fn a_pattern_is_weighed_by_what_it_compiles_to() {
        let weigh = |p: &str| Matcher::new(p).map(|m| (m.weight_kb, m.window, m.reach()));
        // What an agent waits for: the lightest weight, the full reach.
        for p in [r">\s*$", r"^hello\b", r"^error\[(E\d+)\]: (.*)$", r"(?i)\b(error|warning|failed|panic)\b.*$", r"^TICK (\d+)", r"Finished|FAILED|test result: (ok|FAILED)\. \d+ passed"] {
            assert_eq!(weigh(p).unwrap(), (8, 131_072, 65_536), "{p}");
        }
        // Classes of all Unicode weigh more than they look, and still reach as far back across lines as any.
        let (kb, _, reach) = weigh(r"\b\w+@\w+\.\w+\b").unwrap();
        assert!(kb > 8 && reach >= BEFORE, "{kb} KB, reach {reach}");
        // Counted repeats inside counted repeats multiply.
        assert_eq!(weigh("(?:(?:(?:a{1,30}){1,30}){1,30})Q").unwrap(), (2048, 512, 256));
        assert_eq!(weigh(r"\w*a\w{100}Q").unwrap(), (8192, 128, 64));
        // Past the regex engine's own limit, and what is not a pattern: refused, and soon.
        let t = Instant::now();
        assert!(matches!(Matcher::new(r"\p{L}{1,300}Q"), Err(regex::Error::CompiledTooBig(_))));
        assert!(matches!(Matcher::new(r"a{1000}{1000}{1000}"), Err(regex::Error::CompiledTooBig(_))));
        assert!(matches!(Matcher::new("(unclosed"), Err(regex::Error::Syntax(_))));
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    /// A heavy pattern is given output in small pieces. A match that lies
    /// across two of them is found all the same, wherever the cut falls.
    #[test]
    fn a_match_across_two_pieces_is_found_wherever_the_cut_falls() {
        let never = AtomicBool::new(false);
        // (pattern, the match, how far the pattern reaches, how many places to try it in).
        // The first match is exactly as long as its pattern reaches, and slow to find: a few places.
        let cases = [
            ("(?:(?:(?:a{1,30}){1,30}){1,30})Q", format!("{}Q", "a".repeat(255)), 256, 3),
            ("(?:(?:(?:a{1,30}){1,30}){1,30})Q", format!("{}Q", "a".repeat(20)), 256, 24),
            (r"[\w-]{1,64}: ok", "build-step-17: ok".to_string(), 128, 24),
            (r"^[\w.-]{3,30}@", "some.one-else@".to_string(), 256, 24),
            (r"\b\w+@\w+\.\w+\b", "someone@example.org".to_string(), 2048, 24),
            (r"^done: (\d+) crates$", "done: 200 crates".to_string(), 65_536, 24),
        ];
        for (pattern, needle, reach, places) in cases {
            assert_eq!(Matcher::new(pattern).unwrap().reach(), reach, "{pattern}");
            for lines in [true, false] {
                for shift in (0..places).map(|i| i * 11) {
                    // Filler the pattern cannot match, in lines or as one endless line.
                    let filler = if lines { "- . - . - . - . - . - . - . - .\r\n".repeat(40) } else { "- . ".repeat(330) };
                    let mut text = filler.clone();
                    text.push_str(&"=".repeat(shift));
                    text.push_str("\r\n");
                    let at = text.len();
                    text.push_str(&needle);
                    text.push_str("\r\n");
                    text.push_str(&filler);
                    let mut m = Matcher::new(pattern).unwrap();
                    let scan = m.scan(text.as_bytes(), 0, Instant::now() + Duration::from_secs(600), &never);
                    let hit = scan.hit.unwrap_or_else(|| panic!("{pattern} did not find {needle:?} at byte {at} (pieces of {})", m.window / 2));
                    assert_eq!((hit.matched.as_str(), hit.firm), (needle.as_str(), true), "{pattern} at byte {at}");
                    assert!(hit.cursor as usize >= at + needle.len() && hit.cursor as usize <= at + needle.len() + 2, "{pattern}: cursor {} for a match ending at {}", hit.cursor, at + needle.len());
                }
            }
        }
    }

    /// How slow a pattern can be driven, against how large it compiles: the
    /// figures `REACH` is set from. Not run by `cargo test`:
    ///   cargo test --release --bin proteus measure_pattern_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn measure_pattern_cost() {
        let a_lines = format!("{} \r\n", "a".repeat(90)).repeat(200); // 18.6 KB
        let words = "the quick brown fox jumps over the lazy dog ".repeat(420);
        let az = "abcdefghijklmnopqrstuvwxyz".repeat(700);
        let mixed = "aXa aa aaa aaaa b ab ba aab ".repeat(660);
        let mut x = 0x2545F4914F6CDD1Du64;
        let ab: String = (0..18_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if x & 1 == 0 { 'a' } else { 'b' }
            })
            .collect();
        let texts: [(&str, &str); 5] = [("lines of a", &a_lines), ("words", &words), ("a to z", &az), ("mixed", &mixed), ("random a and b", &ab)];
        let alternation: String = (0..3000).map(|i| format!("w{i}x{}", i * 7919 % 1000)).collect::<Vec<_>>().join("|");
        let patterns: Vec<String> = [
            r"(?:(?:(?:a{1,30}){1,30}){1,30})Q", r"(?:(?:(?:a{1,30}){1,30}){1,10})Q", r"(?:(?:(?:a{1,30}){1,30}){1,3})Q", r"(?:(?:a{1,30}){1,30})Q",
            r"(?:(?:a{1,30}){1,10})Q", r"(?:(?:a{1,10}){1,10})Q", r"(?:a?){40}a{40}Q", r"(?:a?){400}a{400}Q", r"(?:a?){2000}a{2000}Q",
            r"[a-z]{1,500}[a-z]{1,500}[a-z]{1,500}Q", r"(?:[a-z ]{1,100}){1,100}Q", r"(\w+\s?)+Q", r"(?s)(.*)(.*)(.*)(.*)(.*)(.*)(.*)(.*)(.*)Q",
            r"(?:.*a){20}Q", r"(?:.*a){200}Q", r"(?s)(?:.*a){200}Q", r"(?:a|aa)+Q", r"(a+)+b", r"(?i)(?:[a-zà-ÿ]{1,40}){1,40}Q",
            r"[ab]*a[ab]{50}Q", r"[ab]*a[ab]{200}Q", r"[ab]*a[ab]{500}Q", r"[ab]*a[ab]{1000}Q", r"[ab]*a[ab]{2000}Q", r"[ab]*a[ab]{5000}Q",
            r"[ab]*a[ab]{20000}Q", r"(?:[ab]*a[ab]{300}){3}Q", r"\w*a\w{100}Q", r"(?s).*a.{500}Q", r"(?s).*a.{3000}Q", r"(?:a|b)*a(?:a|b){800}Q",
            r"(?i)[ab]*a(?:[ab]|[à-ÿ]){600}Q", r"^error\[(E\d+)\]: (.*)$", r">\s*$", r"(?i)\b(error|warning|failed|panic)\b.*$", r"\b\w+@\w+\.\w+\b",
        ]
        .iter()
        .map(|p| p.to_string())
        .chain([format!("(?:{alternation})Q")])
        .collect();
        let mut worst_of_all = (0.0f64, String::new());
        for p in &patterns {
            let t = Instant::now();
            let m = Matcher::new(p).unwrap();
            let weigh_ms = t.elapsed().as_secs_f64() * 1000.0;
            let mut worst = (0.0f64, "");
            for (name, text) in texts {
                // A short run first, so that a slow pattern is not given the whole text.
                let probe = &text[..2048];
                let t = Instant::now();
                let _ = m.re.captures_at(probe, 0);
                let mut per = t.elapsed().as_secs_f64() * 1e6 / probe.len() as f64;
                if per < 2.0 {
                    let t = Instant::now();
                    let _ = m.re.captures_at(text, 0);
                    per = t.elapsed().as_secs_f64() * 1e6 / text.len() as f64;
                }
                if per > worst.0 {
                    worst = (per, name);
                }
            }
            let per_kb = worst.0 / m.weight_kb as f64;
            if per_kb > worst_of_all.0 {
                worst_of_all = (per_kb, p.chars().take(40).collect());
            }
            let r = |x: f64| (x * 10_000.0).round() / 10_000.0;
            println!("MEASURE {}", json!({ "what": "pattern cost", "pattern": p.chars().take(48).collect::<String>(), "weight_kb": m.weight_kb, "window": m.window,
                "worst_us_per_char": r(worst.0), "on": worst.1, "us_per_char_per_kb": r(per_kb), "one_window_ms": r(worst.0 * m.window as f64 / 1000.0), "weigh_ms": r(weigh_ms) }));
        }
        println!("MEASURE {}", json!({ "what": "pattern cost, the worst found", "us_per_char_per_kb": (worst_of_all.0 * 10_000.0).round() / 10_000.0, "pattern": worst_of_all.1,
            "one_window_ms_at_that_rate": (worst_of_all.0 * REACH as f64 / 100.0).round() / 10.0 }));
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        // RFC 4648, section 10.
        for (plain, coded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(plain.as_bytes()), coded);
        }
        assert_eq!(base64(&[0xfb, 0xff, 0xfe]), "+//+");
    }

    /// A journal written by the real writer, read while it grows.
    #[test]
    fn a_journal_is_followed_as_it_grows() {
        let p = std::env::temp_dir().join(format!("proteus-tail-test-{}.prtj", std::process::id()));
        let mut w = journal::JournalWriter::create(&p, &json!({ "v": 1 })).unwrap();
        let mut tail = Tail::new(p.clone());
        tail.refresh().unwrap();
        assert_eq!((tail.out, tail.marks.len()), (0, 0));

        w.record(journal::K_RESIZE, &crate::session::resize_payload(100, 30)).unwrap();
        w.record(journal::K_OUTPUT, b"hello ").unwrap();
        w.record(journal::K_OUTPUT, b"world\r\n").unwrap();
        w.flush().unwrap();
        tail.refresh().unwrap();
        assert_eq!(tail.out, 13);
        assert_eq!(tail.read(0, 13).unwrap(), b"hello world\r\n");
        assert_eq!(tail.read(4, 8).unwrap(), b"o wo");
        assert_eq!(tail.read(6, 99).unwrap(), b"world\r\n");
        assert_eq!(tail.resizes(u64::MAX), [(0, 100, 30)]);
        // When the first part had been printed, by the journal's own clock.
        let first_part = tail.marks.last().map(|m| m.t_us).unwrap();

        std::thread::sleep(Duration::from_millis(30));
        w.record(journal::K_RESIZE, &crate::session::resize_payload(80, 24)).unwrap();
        w.record(journal::K_OUTPUT, b"later").unwrap();
        w.record(journal::K_EXIT, &7u32.to_le_bytes()).unwrap();
        w.flush().unwrap();
        assert_eq!(tail.exit, None, "nothing is seen until asked");
        tail.refresh().unwrap();
        assert_eq!((tail.out, tail.exit), (18, Some(Some(7))));
        assert_eq!(tail.read(11, 18).unwrap(), b"\r\nlater");
        assert_eq!(tail.resizes(u64::MAX), [(0, 100, 30), (13, 80, 24)]);
        // Time travel: at that moment, and for 15 ms after it, only the first
        // part had been printed. (Not "15 ms into the session": on a busy
        // machine the first record itself can be later than that.)
        assert_eq!(tail.offset_at(first_part), 13);
        assert_eq!(tail.offset_at(first_part + 15_000), 13);
        assert_eq!(tail.offset_at(0), 0);
        assert_eq!(tail.offset_at(u64::MAX), 18);
        assert_eq!(tail.resizes(first_part + 15_000), [(0, 100, 30)]);

        // Half a record on disk (the writer was interrupted) is left for later.
        drop(w);
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.extend_from_slice(&[journal::K_OUTPUT, 1, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, b'x']);
        std::fs::write(&p, &bytes).unwrap();
        tail.refresh().unwrap();
        assert_eq!(tail.out, 18);
        let _ = std::fs::remove_file(&p);
    }

    /// Output that has left memory is read back from the journal, joined to
    /// what memory still holds, and no request can reach outside either.
    #[test]
    fn what_memory_let_go_is_read_from_the_journal() {
        let mut live = Sess::new(1, 1, String::new(), PathBuf::new(), (80, 24), true).st.into_inner().unwrap();
        live.push(b"0123456789", 16);
        assert_eq!((live.base, live.end, live.mem.as_slice()), (0, 10, &b"0123456789"[..]));
        live.push(b"abcdefghij", 16);
        assert_eq!((live.base, live.end, live.mem.as_slice()), (12, 20, &b"cdefghij"[..]), "over 16 held: cut back to 8");
        assert_eq!(live.slice(14, 17), b"efg");
        assert_eq!(live.slice(0, 14), b"cd", "what is gone is not invented");
        assert_eq!(live.slice(18, 999), b"ij");
        assert_eq!(live.slice(500, 999), b"");
        assert_eq!(live.slice(17, 3), b"");

        let p = std::env::temp_dir().join(format!("proteus-memory-test-{}.prtj", std::process::id()));
        let mut w = journal::JournalWriter::create(&p, &json!({ "v": 1 })).unwrap();
        w.record(journal::K_RESIZE, &crate::session::resize_payload(80, 24)).unwrap();
        w.record(journal::K_OUTPUT, b"0123456").unwrap();
        w.record(journal::K_OUTPUT, b"789abcdefghij").unwrap();
        w.flush().unwrap();
        let s = Sess::new(1, 1, String::new(), p.clone(), (80, 24), true);
        {
            let mut st = s.st.lock().unwrap();
            st.linked = true;
            st.push(b"0123456789", 16);
            st.push(b"abcdefghij", 16);
        }
        let all = b"0123456789abcdefghij";
        for (from, to) in [(0, None), (0, Some(5)), (3, Some(12)), (3, Some(15)), (11, None), (12, None), (15, Some(18)), (20, None), (0, Some(999))] {
            let (got, end) = s.bytes(from, to).unwrap();
            let want = &all[from as usize..to.unwrap_or(20).min(20) as usize];
            assert_eq!((got.as_slice(), end), (want, from + want.len() as u64), "{from}..{to:?}");
        }
        // Past the end, or backwards: nothing, and no panic.
        assert_eq!(s.bytes(25, None).unwrap(), (vec![], 25));
        assert_eq!(s.bytes(25, Some(3)).unwrap(), (vec![], 25));
        assert_eq!(s.take(99, true).unwrap().bytes, b"");
        assert_eq!(s.take(2, true).unwrap().bytes, &all[2..]);
        // Never connected: the journal alone.
        let cold = Sess::new(2, 1, String::new(), p.clone(), (80, 24), false);
        assert_eq!(cold.bytes(5, Some(9)).unwrap(), (b"5678".to_vec(), 9));
        assert_eq!(cold.bytes(99, None).unwrap(), (vec![], 99));
        cold.start_waiting_from_now();
        let st = cold.st.lock().unwrap();
        assert_eq!((st.wait_pos, st.read_pos), (20, 0), "waiting starts at the end, reading at the top");
        drop(st);
        drop(w);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_pattern_is_found_across_pieces_and_colour_codes() {
        let raw: &[u8] = include_bytes!("../forge/samples/organs/cargo-build.vt");
        // Delivered in ragged pieces, as a socket would.
        let mut m = Matcher::new(r"^error\[(E\d+)\]: (.*)$").unwrap();
        let mut off = 0u64;
        let mut hit = None;
        let mut halves = 0;
        for piece in raw.chunks(97) {
            m.feed(piece, off);
            off += piece.len() as u64;
            match m.search() {
                // Half the line has arrived: `(.*)$` matches it, but not for good.
                Some(h) if !h.firm => {
                    assert!("error[E0308]: mismatched types".starts_with(&h.matched) && h.matched.len() < 30, "{}", h.matched);
                    halves += 1;
                }
                Some(h) => {
                    hit = Some(h);
                    break;
                }
                None => {}
            }
        }
        assert_eq!(halves, 1, "this capture splits the line once at 97 bytes a piece");
        let hit = hit.expect("the compiler error is in this capture");
        assert_eq!(hit.matched, "error[E0308]: mismatched types");
        assert_eq!(hit.groups[1].as_deref(), Some("E0308"));
        assert_eq!(hit.line, "error[E0308]: mismatched types");
        // The cursor is just past that line in the raw bytes, colour codes and all.
        let line_end = raw.windows(16).position(|w| w == b"mismatched types").unwrap();
        let after = &raw[hit.cursor as usize..];
        assert!(hit.cursor as usize > line_end && hit.cursor as usize <= line_end + 40, "{} vs {line_end}", hit.cursor);
        assert!(after.starts_with(b" ") || after.starts_with(b"\x1b"), "what follows is the location line: {:?}", String::from_utf8_lossy(&after[..30]));
        assert!(off < raw.len() as u64, "found before the build finished printing");

        let mut never = Matcher::new("no such text anywhere").unwrap();
        never.feed(raw, 0);
        assert!(never.search().is_none());
        assert!(Matcher::new("(unclosed").is_err());
    }

    fn server() -> (Arc<Server>, Arc<Mutex<Vec<u8>>>) {
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let buf = Arc::new(Mutex::new(Vec::new()));
        (Server::new(false, false, Box::new(Sink(buf.clone()))), buf)
    }

    fn ask(msg: Value) -> Vec<Value> {
        let (s, buf) = server();
        s.handle(msg);
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        out.lines().map(|l| serde_json::from_str(l).expect("every line is one JSON message")).collect()
    }

    #[test]
    fn handshake_of_the_earlier_revisions() {
        let r = &ask(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "0" } } }))[0];
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18", "a revision we speak is echoed");
        assert_eq!(r["result"]["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(r["result"]["serverInfo"]["name"], "proteus");
        assert!(r["result"].get("resultType").is_none());
        // One we do not speak gets our newest handshake revision back.
        let r = &ask(json!({ "jsonrpc": "2.0", "id": "a", "method": "initialize", "params": { "protocolVersion": "1999-01-01" } }))[0];
        assert_eq!((r["id"].as_str(), r["result"]["protocolVersion"].as_str()), (Some("a"), Some("2025-11-25")));
        assert_eq!(ask(json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }))[0]["result"], json!({}));
        assert!(ask(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_empty(), "a notification is never answered");
        let list = &ask(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }))[0]["result"];
        let names: Vec<&str> = list["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
        assert!(list["tools"].as_array().unwrap().iter().all(|t| t["inputSchema"]["type"] == "object" && t["description"].as_str().unwrap().len() > 40));
    }

    #[test]
    fn the_stateless_revision() {
        let meta = |v: &str| json!({ META_VERSION: v, META_CAPS: {}, "io.modelcontextprotocol/clientInfo": { "name": "t", "version": "0" } });
        let r = &ask(json!({ "jsonrpc": "2.0", "id": "d", "method": "server/discover", "params": { "_meta": meta(MODERN) } }))[0]["result"];
        assert_eq!(r["resultType"], "complete");
        assert_eq!(r["supportedVersions"][0], MODERN);
        assert_eq!(r["capabilities"]["tools"], json!({}));
        assert_eq!(r["_meta"][META_SERVER]["name"], "proteus");
        let r = &ask(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "_meta": meta(MODERN) } }))[0]["result"];
        assert_eq!((r["resultType"].as_str(), r["cacheScope"].as_str(), r["tools"].as_array().map(Vec::len)), (Some("complete"), Some("public"), Some(8)));
        assert!(r["ttlMs"].as_u64().is_some());
        // A revision we do not speak: the defined error, with the ones we do.
        let e = &ask(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": { "_meta": meta("2031-01-01") } }))[0]["error"];
        assert_eq!((e["code"].as_i64(), e["data"]["requested"].as_str()), (Some(-32022), Some("2031-01-01")));
        assert!(e["data"]["supported"].as_array().unwrap().contains(&json!(MODERN)));
        // The required capabilities field missing.
        let e = &ask(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": { "_meta": { META_VERSION: MODERN } } }))[0]["error"];
        assert_eq!(e["code"], -32602);
        // ping and initialize left the protocol with this revision.
        for gone in ["ping", "initialize"] {
            let e = &ask(json!({ "jsonrpc": "2.0", "id": 4, "method": gone, "params": { "_meta": meta(MODERN) } }))[0]["error"];
            assert_eq!(e["code"], -32601, "{gone}");
        }
    }

    /// A message is one line. A line longer than the limit is read past, not
    /// held, and the line after it is read from its start.
    #[test]
    fn a_line_from_the_client_is_never_held_beyond_its_limit() {
        let text = b"short\n0123456789abcdef\n0123456789abcdefX and on and on and on\n\nlast".to_vec();
        // A reader that hands over seven bytes at a time, as a pipe hands over what it has.
        let mut input = BufReader::with_capacity(7, io::Cursor::new(text));
        let mut line = Vec::new();
        let mut next = |line: &mut Vec<u8>| (next_line(&mut input, line, 16).unwrap(), String::from_utf8(line.clone()).unwrap());
        assert_eq!(next(&mut line), (Got::Whole, "short\n".to_string()));
        assert_eq!(next(&mut line), (Got::Whole, "0123456789abcdef\n".to_string()), "exactly the limit");
        assert_eq!(next(&mut line), (Got::TooLong, String::new()));
        assert_eq!(next(&mut line), (Got::Whole, "\n".to_string()), "the next line starts where the long one ended");
        assert_eq!(next(&mut line), (Got::Whole, "last".to_string()), "the last line need not end in a line feed");
        assert_eq!(next(&mut line), (Got::End, String::new()));
        // One that never ends: read to the end of the input without being kept.
        let mut endless = BufReader::with_capacity(7, io::Cursor::new(vec![b'x'; 1000]));
        assert_eq!(next_line(&mut endless, &mut line, 16).unwrap(), Got::TooLong);
        assert_eq!(line.capacity(), 0, "nothing of it is held");
        assert_eq!(next_line(&mut endless, &mut line, 16).unwrap(), Got::End);
    }

    #[test]
    fn what_is_not_a_request_is_refused_properly() {
        assert_eq!(ask(json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }]))[0]["error"]["code"], -32600);
        assert_eq!(ask(json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }))[0]["error"]["code"], -32601);
        let e = &ask(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "format_disk" } }))[0]["error"];
        assert_eq!((e["code"].as_i64(), e["message"].as_str()), (Some(-32602), Some("Unknown tool: format_disk")));
        assert_eq!(ask(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "read", "arguments": [1] } }))[0]["error"]["code"], -32602);
        assert!(ask(json!({ "jsonrpc": "2.0", "id": 5, "result": {} })).is_empty(), "a stray response is ignored");
        assert!(ask(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 99 } })).is_empty());
    }
}
