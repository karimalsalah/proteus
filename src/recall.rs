//! Recall: nothing a session printed is lost, and any instant of it can be
//! reached in milliseconds.
//!
//! The journal is the record; this module makes it navigable.
//!
//! * A **keyframe** is the whole terminal state at one record boundary:
//!   both screens, a tail of the scrollback, cursors, modes, palette, tab
//!   stops, scroll region, charsets and titles. Keyframes live in a sidecar
//!   file beside the journal (`x.prtj` -> `x.prtk`) which is built lazily,
//!   extended as the journal grows, checked on load (against the journal's
//!   header and its first records) and safe to delete.
//! * **Seek** restores the nearest keyframe and replays only the records
//!   after it.
//! * **Find** replays once and reports every interval a text was on screen.
//!
//! Why a serialised grid and not the state re-emitted as escape sequences:
//! several things a grid holds cannot be produced by printing. A soft-wrap
//! flag on an arbitrary row, half of a wide character orphaned by an insert,
//! a tab cell, a pending wrap on a saved cursor: each needs its own trick as
//! escape sequences and is one assignment as data. Cells are written straight
//! into the grid; only state the terminal keeps private goes through the same
//! handler calls a program's output would make.
//!
//! Three rules keep a keyframe exact:
//!
//! 1. It is taken only where the parser is provably at rest: the last
//!    printable byte of the record, fed alone, made the terminal act, and no
//!    synchronized update is open. (Feeding a byte alone and seeing a handler
//!    call means a print or a completed sequence, both of which leave the
//!    parser in its ground state.)
//! 2. State the terminal does not expose (scroll region, tab stops, active
//!    charset, title stack) is tracked by a handler that sits in front of the
//!    terminal while an index is being built, and never guessed.
//! 3. Every keyframe is restored and compared with the terminal it was taken
//!    from before it is stored. One that does not match is dropped.
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use alacritty_terminal::grid::{Cursor, Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::color::COUNT as COLOR_COUNT;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{
    self, Attr, CharsetIndex, ClearMode, Color, CursorShape, CursorStyle, Handler, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, NamedColor, NamedMode, NamedPrivateMode, PrivateMode, Rgb,
    StandardCharset, TabulationClearMode,
};

use crate::journal;
use crate::term::{resize_term, vt_config, Dim, Listener};

/// A keyframe is written once this much journal has gone by since the last...
pub const KF_BYTES: u64 = 256 << 10;
/// ...or this many times the last keyframe's own size, whichever is more.
pub const KF_RATIO: u64 = 12;
/// Rows of scrollback an index keyframe carries (the newest ones). Enough
/// for a viewer taller than the session was; a resize that wants more makes
/// the seek start over from the beginning of the journal.
pub const HIST_TAIL: usize = 120;

const SIDE_MAGIC: &[u8; 8] = b"PRTK0002";
/// Bump when the snapshot layout or the terminal's configuration changes:
/// older sidecars are then rebuilt instead of trusted.
const ENGINE: &str = "alacritty_terminal-0.25 hist50000 snap1";
const ESU: &[u8] = b"\x1b[?2026l";
/// How long the terminal leaves a synchronized update unshown before it
/// stops waiting for it to close (alacritty_terminal's own limit, which
/// `Terminal::attach` obeys for the live window).
const SYNC_WAIT_US: u64 = 150_000;
/// An index is tied to its journal by the journal's header and by this much
/// of its first records. A header can be shared: the same command recorded
/// again under the same name within a second, or a journal whose oldest
/// output was trimmed away. The records' own timestamps cannot.
const LEAD: u64 = 64 << 10;

/// What the parser is given in place of a clock. The live window times a
/// synchronized update by the wall clock; a replay has to go by the journal's
/// time instead, so all this keeps is whether an update is open and how many
/// times one has been begun, or begun again (which starts the wait afresh).
/// The parser itself says when that happens, wherever records were cut.
#[derive(Default)]
struct SyncMark {
    open: bool,
    begun: u64,
}

impl ansi::Timeout for SyncMark {
    fn set_timeout(&mut self, _: std::time::Duration) {
        self.open = true;
        self.begun += 1;
    }
    fn clear_timeout(&mut self) {
        self.open = false;
    }
    fn pending_timeout(&self) -> bool {
        self.open
    }
}

type Processor = ansi::Processor<SyncMark>;

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn fnv64(seed: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(seed, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}
const FNV0: u64 = 0xcbf2_9ce4_8422_2325;

// ------------------------------------------------------------------ journal

/// A journal's header.
#[derive(Clone)]
pub struct Head {
    pub meta: serde_json::Value,
    /// Offset of the first record.
    pub data_start: u64,
    /// Hash of the header bytes: tells one journal from another.
    pub id: u64,
}

impl Head {
    pub fn size(&self) -> (usize, usize) {
        let n = |k: &str, d: u64| self.meta[k].as_u64().unwrap_or(d).clamp(2, 4000) as usize;
        (n("cols", 80), n("rows", 24))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RecHead {
    pub kind: u8,
    pub t_us: u64,
    pub len: u32,
}

/// Reads a journal record by record without loading the file. A record cut
/// short by a crash, or still being written, ends the stream.
pub struct Reader {
    f: BufReader<File>,
    /// Offset of the next unread byte.
    pub pos: u64,
    len: u64,
}

impl Reader {
    pub fn open(path: &Path) -> io::Result<(Head, Reader)> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut f = BufReader::with_capacity(1 << 16, file);
        let mut h = [0u8; 12];
        if f.read_exact(&mut h).is_err() || &h[..8] != journal::MAGIC {
            return Err(bad("not a PROTEUS journal"));
        }
        let mlen = u32::from_le_bytes([h[8], h[9], h[10], h[11]]) as u64;
        if mlen > 1 << 20 || 12 + mlen > len {
            return Err(bad("journal header truncated"));
        }
        let mut m = vec![0u8; mlen as usize];
        f.read_exact(&mut m)?;
        let meta = serde_json::from_slice(&m).unwrap_or(serde_json::Value::Null);
        let head = Head { meta, data_start: 12 + mlen, id: fnv64(fnv64(FNV0, &h), &m) };
        Ok((head, Reader { f, pos: 12 + mlen, len }))
    }

    /// Start reading at a record boundary found earlier.
    pub fn at(path: &Path, pos: u64) -> io::Result<Reader> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(pos))?;
        Ok(Reader { f: BufReader::with_capacity(1 << 16, file), pos, len })
    }

    /// The next complete record's header; its payload must then be read or skipped.
    pub fn next(&mut self) -> Option<RecHead> {
        if self.pos + 13 > self.len {
            return None;
        }
        let mut h = [0u8; 13];
        self.f.read_exact(&mut h).ok()?;
        let len = u32::from_le_bytes([h[9], h[10], h[11], h[12]]);
        if self.pos + 13 + len as u64 > self.len {
            return None;
        }
        let t_us = u64::from_le_bytes([h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8]]);
        let r = RecHead { kind: h[0], t_us, len };
        self.pos += 13;
        Some(r)
    }

    pub fn payload(&mut self, h: &RecHead, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.resize(h.len as usize, 0);
        self.f.read_exact(buf)?;
        self.pos += h.len as u64;
        Ok(())
    }

    pub fn skip(&mut self, h: &RecHead) -> io::Result<()> {
        self.f.seek_relative(h.len as i64)?;
        self.pos += h.len as u64;
        Ok(())
    }
}

fn resize_of(p: &[u8]) -> Option<(usize, usize)> {
    let (c, r) = (u16::from_le_bytes([*p.first()?, *p.get(1)?]), u16::from_le_bytes([*p.get(2)?, *p.get(3)?]));
    (c > 0 && r > 0).then_some((c as usize, r as usize))
}

/// One record of a journal, without its payload.
#[derive(Clone, Copy)]
pub struct Ev {
    pub t_us: u64,
    pub len: u32,
    pub kind: u8,
}

/// Every record of a journal in order: what the scrub bar draws and what
/// stepping through time walks. A *position* is a number of records applied,
/// from 0 (nothing yet) to `evs.len()` (everything).
pub struct Timeline {
    pub head: Head,
    pub evs: Vec<Ev>,
    /// For each output record, the position just after it.
    pub outs: Vec<u32>,
    /// Offset after the last complete record.
    pub end: u64,
    pub exit: Option<u32>,
    pub out_bytes: u64,
}

impl Timeline {
    pub fn scan(path: &Path) -> io::Result<Timeline> {
        let (head, rd) = Reader::open(path)?;
        let mut t = Timeline { end: head.data_start, head, evs: Vec::new(), outs: Vec::new(), exit: None, out_bytes: 0 };
        t.read(rd)?;
        Ok(t)
    }

    /// Pick up records appended since the last scan. Returns how many.
    pub fn refresh(&mut self, path: &Path) -> io::Result<usize> {
        let before = self.evs.len();
        let rd = Reader::at(path, self.end)?;
        self.read(rd)?;
        Ok(self.evs.len() - before)
    }

    fn read(&mut self, mut rd: Reader) -> io::Result<()> {
        let mut buf = Vec::new();
        while let Some(h) = rd.next() {
            match h.kind {
                journal::K_EXIT => {
                    rd.payload(&h, &mut buf)?;
                    self.exit = buf.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                }
                _ => rd.skip(&h)?,
            }
            if h.kind == journal::K_OUTPUT {
                self.out_bytes += h.len as u64;
                self.outs.push(self.evs.len() as u32 + 1);
            }
            self.evs.push(Ev { t_us: h.t_us, len: h.len, kind: h.kind });
            self.end = rd.pos;
        }
        Ok(())
    }

    /// Length of the session so far, in microseconds.
    pub fn duration_us(&self) -> u64 {
        self.evs.last().map_or(0, |e| e.t_us)
    }

    pub fn time_at(&self, pos: usize) -> u64 {
        match pos.min(self.evs.len()) {
            0 => 0,
            p => self.evs[p - 1].t_us,
        }
    }

    /// The position holding everything up to and including time `t_us`.
    pub fn pos_at(&self, t_us: u64) -> usize {
        self.evs.partition_point(|e| e.t_us <= t_us)
    }

    /// How many output records have been applied at `pos`.
    pub fn outputs_at(&self, pos: usize) -> usize {
        self.outs.partition_point(|&p| p as usize <= pos)
    }

    /// Move `n` output records back (negative) or forward from `pos`,
    /// landing just after an output record. `None` means the last output or
    /// beyond: that is the present, not a moment in the past.
    pub fn step(&self, pos: usize, n: i64) -> Option<usize> {
        let last = self.outs.len() as i64 - 1;
        let to = if n < 0 {
            // The present counts as standing on the last output.
            let pos = pos.min(self.outs.last().map_or(0, |&p| p as usize));
            // Never before the first output: an empty screen is not a moment worth visiting.
            (self.outs.partition_point(|&p| (p as usize) < pos) as i64 + n).max(0)
        } else {
            (self.outs.partition_point(|&p| p as usize <= pos) as i64 + n - 1).max(0)
        };
        (to < last).then(|| self.outs[to as usize] as usize)
    }

    /// Bytes of output in each of `buckets` equal slices of the session's life.
    pub fn histogram(&self, buckets: usize) -> Vec<u64> {
        let mut h = vec![0u64; buckets.max(1)];
        let span = self.duration_us() + 1;
        for e in self.evs.iter().filter(|e| e.kind == journal::K_OUTPUT) {
            let i = ((e.t_us as u128 * h.len() as u128) / span as u128) as usize;
            h[i.min(buckets.max(1) - 1)] += e.len as u64;
        }
        h
    }
}

// --------------------------------------------- state the terminal keeps private

/// What `Term` holds but does not show: tracked by sitting between the
/// parser and the terminal (`Tee`). Each field mirrors a private field of
/// `alacritty_terminal::Term` 0.25 and is changed in exactly the places that
/// field is.
#[derive(Clone, Debug, PartialEq)]
struct Shadow {
    /// Scroll region as `start..end` lines.
    region: (i32, i32),
    tabs: Vec<bool>,
    charset: u8,
    title: Option<String>,
    titles: Vec<Option<String>>,
    /// Handler calls so far: how a record's last byte is seen to have acted.
    calls: u64,
    /// The last character printed (what a repeat sequence would repeat).
    last: Option<char>,
    /// The alternate screen has been entered at least once since a reset.
    alt_seen: bool,
    /// The scrollback was thrown away, so none of it can be missing.
    hist_cleared: bool,
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0).collect()
}

impl Shadow {
    fn new(cols: usize, rows: usize) -> Self {
        Shadow {
            region: (0, rows as i32),
            tabs: default_tabs(cols),
            charset: 0,
            title: None,
            titles: Vec::new(),
            calls: 0,
            last: None,
            alt_seen: false,
            hist_cleared: false,
        }
    }

    fn set_region(&mut self, top: usize, bottom: Option<usize>, lines: usize) {
        let bottom = bottom.unwrap_or(lines);
        if top >= bottom {
            return;
        }
        let lines = lines as i32;
        self.region = ((top as i32 - 1).min(lines), (bottom as i32).min(lines));
    }

    fn resized(&mut self, cols: usize, rows: usize, cols_changed: bool) {
        self.region = (0, rows as i32);
        if cols_changed {
            let mut i = self.tabs.len();
            self.tabs.resize_with(cols, || {
                i += 1;
                (i - 1) % 8 == 0
            });
        }
    }
}

struct Tee<'a> {
    t: &'a mut Term<Listener>,
    s: &'a mut Shadow,
}

macro_rules! pass {
    ($($name:ident($($a:ident: $t:ty),*);)*) => {
        $(#[inline]
        fn $name(&mut self $(, $a: $t)*) {
            self.s.calls += 1;
            Handler::$name(&mut *self.t $(, $a)*)
        })*
    };
}

impl Handler for Tee<'_> {
    // Everything `Term` implements, passed straight through.
    pass! {
        set_cursor_style(s: Option<CursorStyle>);
        set_cursor_shape(s: CursorShape);
        goto(l: i32, c: usize);
        goto_line(l: i32);
        goto_col(c: usize);
        insert_blank(n: usize);
        move_up(n: usize);
        move_down(n: usize);
        identify_terminal(i: Option<char>);
        device_status(n: usize);
        move_forward(n: usize);
        move_backward(n: usize);
        move_down_and_cr(n: usize);
        move_up_and_cr(n: usize);
        put_tab(n: u16);
        backspace();
        carriage_return();
        linefeed();
        bell();
        substitute();
        newline();
        scroll_up(n: usize);
        scroll_down(n: usize);
        insert_blank_lines(n: usize);
        delete_lines(n: usize);
        erase_chars(n: usize);
        delete_chars(n: usize);
        move_backward_tabs(n: u16);
        move_forward_tabs(n: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(m: LineClearMode);
        reverse_index();
        terminal_attribute(a: Attr);
        set_mode(m: Mode);
        unset_mode(m: Mode);
        report_mode(m: Mode);
        report_private_mode(m: PrivateMode);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        configure_charset(i: CharsetIndex, c: StandardCharset);
        set_color(i: usize, c: Rgb);
        dynamic_color_sequence(p: String, i: usize, t: &str);
        reset_color(i: usize);
        clipboard_store(c: u8, d: &[u8]);
        clipboard_load(c: u8, t: &str);
        decaln();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(h: Option<ansi::Hyperlink>);
        report_keyboard_mode();
        push_keyboard_mode(m: KeyboardModes);
        pop_keyboard_modes(n: u16);
        set_keyboard_mode(m: KeyboardModes, b: KeyboardModesApplyBehavior);
    }

    #[inline]
    fn input(&mut self, c: char) {
        self.s.calls += 1;
        self.s.last = Some(c);
        self.t.input(c)
    }

    fn set_horizontal_tabstop(&mut self) {
        self.s.calls += 1;
        let col = self.t.grid().cursor.point.column.0;
        if let Some(t) = self.s.tabs.get_mut(col) {
            *t = true;
        }
        self.t.set_horizontal_tabstop()
    }

    fn clear_tabs(&mut self, mode: TabulationClearMode) {
        self.s.calls += 1;
        match mode {
            TabulationClearMode::Current => {
                let col = self.t.grid().cursor.point.column.0;
                if let Some(t) = self.s.tabs.get_mut(col) {
                    *t = false;
                }
            }
            TabulationClearMode::All => self.s.tabs.iter_mut().for_each(|t| *t = false),
        }
        self.t.clear_tabs(mode)
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        self.s.calls += 1;
        self.s.set_region(top, bottom, self.t.screen_lines());
        self.t.set_scrolling_region(top, bottom)
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        self.s.calls += 1;
        self.private(mode);
        self.t.set_private_mode(mode)
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        self.s.calls += 1;
        self.private(mode);
        self.t.unset_private_mode(mode)
    }

    fn set_active_charset(&mut self, index: CharsetIndex) {
        self.s.calls += 1;
        self.s.charset = index as u8;
        self.t.set_active_charset(index)
    }

    fn set_title(&mut self, title: Option<String>) {
        self.s.calls += 1;
        self.s.title.clone_from(&title);
        self.t.set_title(title)
    }

    fn push_title(&mut self) {
        self.s.calls += 1;
        if self.s.titles.len() >= 4096 {
            self.s.titles.remove(0);
        }
        self.s.titles.push(self.s.title.clone());
        self.t.push_title()
    }

    fn pop_title(&mut self) {
        self.s.calls += 1;
        if let Some(t) = self.s.titles.pop() {
            self.s.title = t;
        }
        self.t.pop_title()
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        self.s.calls += 1;
        // Only the screen in view loses its scrollback, and the alternate
        // screen has none to lose: asked there, the terminal erases nothing.
        if matches!(mode, ClearMode::Saved) && !self.t.mode().contains(TermMode::ALT_SCREEN) {
            self.s.hist_cleared = true;
        }
        self.t.clear_screen(mode)
    }

    fn reset_state(&mut self) {
        let (calls, last) = (self.s.calls + 1, self.s.last);
        *self.s = Shadow::new(self.t.columns(), self.t.screen_lines());
        self.s.calls = calls;
        self.s.last = last;
        self.s.hist_cleared = true;
        self.t.reset_state()
    }
}

impl Tee<'_> {
    fn private(&mut self, mode: PrivateMode) {
        match mode {
            // Changing the column mode resets the scroll region as a side effect.
            PrivateMode::Named(NamedPrivateMode::ColumnMode) => self.s.set_region(1, None, self.t.screen_lines()),
            PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) => self.s.alt_seen = true,
            _ => {}
        }
    }
}

/// Swallows everything: used to prime a fresh parser's memory of the last
/// printed character without touching a terminal.
struct Null;
impl Handler for Null {}

fn pending(p: &Processor) -> bool {
    p.sync_bytes_count() > 0 || p.sync_timeout().open
}

/// Where the last character of `d` starts, if `d` ends in one complete
/// printable character.
fn last_char_start(d: &[u8]) -> Option<usize> {
    let last = *d.last()?;
    if (0x20..=0x7e).contains(&last) {
        return Some(d.len() - 1);
    }
    if last < 0x80 {
        return None;
    }
    let mut i = d.len() - 1;
    while i > 0 && d[i] & 0xc0 == 0x80 && d.len() - i < 4 {
        i -= 1;
    }
    let need = match d[i] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    (need == d.len() - i && std::str::from_utf8(&d[i..]).is_ok()).then_some(i)
}

// ------------------------------------------------------------------- replay

/// A terminal being fed a journal. `tracked` replays can take keyframes.
pub struct Replay {
    pub term: Term<Listener>,
    proc: Processor,
    track: Option<Box<Shadow>>,
    /// The parser is at rest and nothing is buffered: a keyframe may be taken.
    clean: bool,
    /// Older scrollback exists than this replay holds.
    truncated: bool,
    /// ...and what it holds starts in the middle of a wrapped line (one
    /// longer than a keyframe's tail). Re-wrapping that line to another
    /// width would break it in different places than the real one broke.
    midline: bool,
    /// A resize wanted scrollback this replay does not hold: the screen may
    /// differ from the real one and must be rebuilt from further back.
    pub short: bool,
    poisoned: bool,
    /// Journal time at which the synchronized update now open was begun (or
    /// begun again); `None` when none is open.
    sync_since: Option<u64>,
}

impl Replay {
    pub fn new(cols: usize, rows: usize, tracked: bool) -> Self {
        let (cols, rows) = (cols.max(2), rows.max(1));
        Replay {
            term: Term::new(vt_config(), &Dim { cols, rows }, Listener::silent()),
            proc: Processor::new(),
            track: tracked.then(|| Box::new(Shadow::new(cols, rows))),
            clean: true,
            truncated: false,
            midline: false,
            short: false,
            poisoned: false,
            sync_since: None,
        }
    }

    pub fn tracked(&self) -> bool {
        self.track.is_some()
    }

    /// The parser is at rest and nothing is buffered: a keyframe could be taken here.
    pub fn at_rest(&self) -> bool {
        self.clean
    }

    pub fn size(&self) -> (usize, usize) {
        (self.term.columns(), self.term.screen_lines())
    }

    /// Feed one record of output.
    pub fn feed(&mut self, data: &[u8]) {
        if self.track.is_none() {
            return self.untracked(|proc, term| proc.advance(term, data));
        }
        let Some(sh) = self.track.as_deref_mut() else { return };
        let mut tee = Tee { t: &mut self.term, s: &mut *sh };
        // Control characters after the last printable one run in whatever
        // state the parser is in and leave it there.
        let end = data.len() - data.iter().rev().take_while(|&&b| b < 0x20 && b != 0x1b).count();
        if end == 0 {
            self.proc.advance(&mut tee, data);
        } else {
            match last_char_start(&data[..end]) {
                None => {
                    self.proc.advance(&mut tee, data);
                    self.clean = false;
                }
                Some(j) => {
                    self.proc.advance(&mut tee, &data[..j]);
                    let was_idle = !pending(&self.proc);
                    let before = tee.s.calls;
                    self.proc.advance(&mut tee, &data[j..end]);
                    let acted = tee.s.calls > before;
                    self.proc.advance(&mut tee, &data[end..]);
                    // A synchronized update that this very byte closed is
                    // also at rest: its buffer ended with the closing sequence.
                    let closed = data[..end].ends_with(ESU);
                    self.clean = acted && (was_idle || closed) && !pending(&self.proc);
                }
            }
        }
        if sh.hist_cleared {
            sh.hist_cleared = false;
            (self.truncated, self.midline) = (false, false);
        }
    }

    /// Run output through a terminal that no tracking handler sits in front
    /// of. Nothing there sees the scrollback erased, but an erase is the only
    /// thing output can do that leaves the main screen with less of it than
    /// before.
    fn untracked(&mut self, run: impl FnOnce(&mut Processor, &mut Term<Listener>)) {
        let held = (self.truncated && !self.term.mode().contains(TermMode::ALT_SCREEN)).then(|| self.term.history_size());
        run(&mut self.proc, &mut self.term);
        if held.is_some_and(|n| !self.term.mode().contains(TermMode::ALT_SCREEN) && self.term.history_size() < n) {
            (self.truncated, self.midline) = (false, false);
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (old_cols, old_rows) = self.size();
        if cols == 0 || rows == 0 || (cols, rows) == (old_cols, old_rows) {
            return;
        }
        resize_term(&mut self.term, cols, rows);
        if let Some(sh) = self.track.as_deref_mut() {
            sh.resized(cols, rows, cols != old_cols);
        }
        // The main screen is the one with scrollback, and it is reflowed and
        // refilled from it even while it is hidden behind the alternate one.
        if (self.truncated && main_history(&mut self.term) == 0) || (self.midline && cols != old_cols) {
            self.short = true;
        }
    }

    /// Apply one journal record.
    pub fn apply(&mut self, kind: u8, data: &[u8]) {
        match kind {
            journal::K_OUTPUT => self.feed(data),
            journal::K_RESIZE => {
                if let Some((c, r)) = resize_of(data) {
                    self.resize(c, r);
                }
            }
            _ => {}
        }
    }

    /// Apply one journal record, written `t_us` into the session. Going by
    /// the journal's time is what lets a replay show a synchronized update
    /// when the window showed it: not before it closed, unless the window
    /// had stopped waiting for it (see `settle_at`).
    pub fn apply_at(&mut self, kind: u8, data: &[u8], t_us: u64) {
        // The window stops waiting at its deadline whatever arrives after it:
        // what the update held by then is on screen before this record lands.
        self.settle_at(t_us);
        let begun = self.sync_begun();
        self.apply(kind, data);
        self.note_sync(begun, t_us);
    }

    /// How many times a synchronized update has been begun, or begun again.
    fn sync_begun(&self) -> u64 {
        self.proc.sync_timeout().begun
    }

    /// After feeding output written at `t_us`: remember since when the
    /// update now open (if any) has been waited for. `begun` is `sync_begun`
    /// as it was before that output.
    fn note_sync(&mut self, begun: u64, t_us: u64) {
        if !pending(&self.proc) {
            self.sync_since = None;
        } else if self.sync_since.is_none() || self.sync_begun() != begun {
            self.sync_since = Some(t_us);
        }
    }

    /// The moment the window stopped waiting for the update now open, if
    /// that moment is `t_us` or earlier.
    fn overdue(&self, t_us: u64) -> Option<u64> {
        self.sync_since.map(|began| began.saturating_add(SYNC_WAIT_US)).filter(|&shown| shown <= t_us)
    }

    /// Finish a synchronized update that is open: everything it held goes
    /// onto the grid, as when the window stops waiting for it to close.
    pub fn settle(&mut self) {
        if !pending(&self.proc) {
            return;
        }
        // (Through the tracking handler, if there is one: what was held back
        // may set a scroll region or erase the scrollback like any other output.)
        match self.track.as_deref_mut() {
            Some(sh) => {
                self.proc.stop_sync(&mut Tee { t: &mut self.term, s: &mut *sh });
                if sh.hist_cleared {
                    sh.hist_cleared = false;
                    (self.truncated, self.midline) = (false, false);
                }
            }
            None => self.untracked(|proc, term| proc.stop_sync(term)),
        }
        self.sync_since = None;
        // Where the held bytes left the parser is not known: the next record
        // has to prove a point of rest again before a keyframe is taken.
        self.clean = false;
    }

    /// Leave the screen as the window showed it at `t_us`. A synchronized
    /// update is not on screen until it closes; the window waits for that,
    /// but only for `SYNC_WAIT_US`, and then shows what has arrived. So an
    /// update open for less than that stays off the grid here too (a seek
    /// must not show a half-drawn frame nobody saw), and one open for longer
    /// is put on it. Needs the records to have been given with `apply_at`.
    pub fn settle_at(&mut self, t_us: u64) {
        if self.overdue(t_us).is_some() {
            self.settle();
        }
    }

    // ------------------------------------------------------------ snapshot

    /// The whole terminal state as bytes, carrying at most `hist_rows` rows
    /// of scrollback (`usize::MAX` for all of it). `None` when this is not a
    /// moment a keyframe can be taken.
    pub fn snapshot(&mut self, hist_rows: usize) -> Option<Vec<u8>> {
        let in_alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        if self.track.is_none() || !self.clean || self.poisoned || self.short {
            return None;
        }
        // Switching screens (done below to read the hidden one) resets the
        // keyboard flags to the top of that screen's stack.
        let kitty_shown = *self.term.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL;
        let mut kitty_hidden = TermMode::empty();
        let mut e = Enc::default();
        let (cols, rows) = self.size();
        let mut flags = 0u64;
        let hist_n;
        if in_alt {
            // The primary screen is hidden behind the alternate one. Swap to
            // read it, swap back, and put the alternate screen (which the
            // swap wipes) back exactly as it was.
            let alt = self.term.grid().clone();
            self.term.swap_alt();
            kitty_hidden = *self.term.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL;
            let g = self.term.grid();
            let restorable = cursor_eq(&g.cursor, &g.saved_cursor);
            let (n, cut) = e.grid(g, hist_rows);
            hist_n = n;
            flags |= cut | self.truncated as u64 | (self.midline as u64) << 2;
            e.cursor(&g.cursor);
            self.term.swap_alt();
            *self.term.grid_mut() = alt;
            if !restorable {
                // Entering the alternate screen makes the hidden screen's
                // cursor and saved cursor equal, and `resize_term` keeps them
                // so. Had they differed, the swap back just lost the saved
                // one and this replay is no longer exact.
                self.poisoned = true;
                return None;
            }
            let g = self.term.grid();
            e.grid(g, 0);
            e.cursor(&g.cursor);
            e.cursor(&g.saved_cursor);
        } else {
            let g = self.term.grid();
            let (n, cut) = e.grid(g, hist_rows);
            hist_n = n;
            flags |= cut | self.truncated as u64 | (self.midline as u64) << 2;
            e.cursor(&g.cursor);
            e.cursor(&g.saved_cursor);
            if self.track.as_ref().is_some_and(|s| s.alt_seen) {
                // The alternate screen keeps its saved cursor between visits.
                let keep = self.term.grid().saved_cursor.clone();
                self.term.swap_alt();
                kitty_hidden = *self.term.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL;
                e.cursor(&self.term.grid().saved_cursor);
                self.term.swap_alt();
                self.term.grid_mut().saved_cursor = keep;
                flags |= 2;
            }
        }
        if *self.term.mode() & TermMode::KITTY_KEYBOARD_PROTOCOL != kitty_shown {
            self.term.set_keyboard_mode(kitty_of(kitty_shown), KeyboardModesApplyBehavior::Replace);
        }
        flags |= (kitty_hidden.bits() as u64) << 32;
        let sh = self.track.as_deref()?;
        let mut out = Vec::with_capacity(e.out.len() + 256);
        put_u(&mut out, 1);
        put_u(&mut out, cols as u64);
        put_u(&mut out, rows as u64);
        put_u(&mut out, self.term.mode().bits() as u64);
        let style = self.term.cursor_style();
        put_u(&mut out, shape_code(style.shape));
        put_u(&mut out, style.blinking as u64);
        let colors: Vec<(usize, Rgb)> = (0..COLOR_COUNT).filter_map(|i| self.term.colors()[i].map(|c| (i, c))).collect();
        put_u(&mut out, colors.len() as u64);
        for (i, c) in colors {
            put_u(&mut out, i as u64);
            out.extend_from_slice(&[c.r, c.g, c.b]);
        }
        put_i(&mut out, sh.region.0 as i64);
        put_i(&mut out, sh.region.1 as i64);
        put_u(&mut out, sh.tabs.len() as u64);
        for chunk in sh.tabs.chunks(8) {
            out.push(chunk.iter().enumerate().fold(0u8, |b, (i, &t)| b | ((t as u8) << i)));
        }
        put_u(&mut out, sh.charset as u64);
        put_opt_str(&mut out, sh.title.as_deref());
        put_u(&mut out, sh.titles.len() as u64);
        for t in &sh.titles {
            put_opt_str(&mut out, t.as_deref());
        }
        put_u(&mut out, sh.last.map_or(0, |c| c as u64 + 1));
        put_u(&mut out, flags);
        put_u(&mut out, e.links.len() as u64);
        for (id, uri) in &e.links {
            put_str(&mut out, id);
            put_str(&mut out, uri);
        }
        put_u(&mut out, e.attrs.len() as u64);
        for a in &e.attrs {
            a.iter().for_each(|&x| put_u(&mut out, x as u64));
        }
        put_u(&mut out, hist_n as u64);
        out.extend_from_slice(&e.out);
        Some(out)
    }

    /// Rebuild a terminal from a snapshot. A `tracked` one can go on to take
    /// further keyframes.
    pub fn restore(data: &[u8], tracked: bool) -> io::Result<Replay> {
        let mut d = Dec { b: data, i: 0 };
        if d.u()? != 1 {
            return Err(bad("unknown snapshot version"));
        }
        let (cols, rows) = (d.u()? as usize, d.u()? as usize);
        if !(2..=4000).contains(&cols) || !(1..=4000).contains(&rows) {
            return Err(bad("snapshot size"));
        }
        let mode = TermMode::from_bits_truncate(d.u()? as u32);
        let style = CursorStyle { shape: shape_of(d.u()?), blinking: d.u()? != 0 };
        let mut colors = Vec::new();
        for _ in 0..d.u()? {
            let i = d.u()? as usize;
            let c = d.bytes(3)?;
            if i >= COLOR_COUNT {
                return Err(bad("snapshot colour"));
            }
            colors.push((i, Rgb { r: c[0], g: c[1], b: c[2] }));
        }
        let region = (d.i()? as i32, d.i()? as i32);
        let ntabs = d.u()? as usize;
        if ntabs != cols {
            return Err(bad("snapshot tab stops"));
        }
        let packed = d.bytes(ntabs.div_ceil(8))?;
        let tabs: Vec<bool> = (0..ntabs).map(|i| packed[i / 8] >> (i % 8) & 1 == 1).collect();
        let charset = d.u()? as u8;
        let title = d.opt_str()?;
        let mut titles = Vec::new();
        for _ in 0..d.u()? {
            titles.push(d.opt_str()?);
        }
        let rep = match d.u()? {
            0 => None,
            c => Some(char::from_u32(c as u32 - 1).ok_or_else(|| bad("snapshot char"))?),
        };
        let flags = d.u()?;
        let mut links = Vec::new();
        for _ in 0..d.u()? {
            let (id, uri) = (d.str()?, d.str()?);
            links.push(Hyperlink::new(Some(id), uri));
        }
        let mut protos = Vec::new();
        for _ in 0..d.u()? {
            let a = [d.u()? as u32, d.u()? as u32, d.u()? as u32, d.u()? as u32, d.u()? as u32];
            let mut c = Cell { fg: color_of(a[0])?, bg: color_of(a[1])?, flags: Flags::from_bits_truncate(a[3] as u16), ..Cell::default() };
            if a[2] != NO_COLOR {
                c.set_underline_color(Some(color_of(a[2])?));
            }
            if a[4] != 0 {
                c.set_hyperlink(Some(links.get(a[4] as usize - 1).ok_or_else(|| bad("snapshot link"))?.clone()));
            }
            protos.push(c);
        }
        let hist_n = d.u()? as usize;
        if hist_n > vt_config().scrolling_history {
            return Err(bad("snapshot history"));
        }

        let mut term = Term::new(vt_config(), &Dim { cols, rows }, Listener::silent());
        // Rows go in oldest first. Once the screen is full, each further
        // batch scrolls it up, which is what puts rows into the scrollback.
        let screen = Line(0)..Line(rows as i32);
        let total = hist_n + rows;
        let mut j = 0;
        while j < total {
            let batch = if j < rows { rows } else { rows.min(total - j) };
            if j >= rows {
                term.grid_mut().scroll_up(&screen, batch);
            }
            for k in 0..batch {
                d.row(&mut term.grid_mut()[Line((rows - batch + k) as i32)], &protos, cols)?;
            }
            j += batch;
        }
        let cursor = d.cursor(&protos, cols, rows)?;

        // State that only the terminal's own handlers can set. Several of
        // these move the cursor, so cursors are assigned afterwards.
        for (i, c) in colors {
            term.set_color(i, c);
        }
        if style != vt_config().default_cursor_style {
            term.set_cursor_style(Some(style));
        }
        if tabs != default_tabs(cols) {
            term.clear_tabs(TabulationClearMode::All);
            for (i, _) in tabs.iter().enumerate().filter(|(_, &t)| t) {
                term.grid_mut().cursor.point.column = Column(i);
                term.set_horizontal_tabstop();
            }
        }
        if charset != 0 {
            term.set_active_charset([CharsetIndex::G0, CharsetIndex::G1, CharsetIndex::G2, CharsetIndex::G3][charset as usize & 3]);
        }
        for t in &titles {
            term.set_title(t.clone());
            term.push_title();
        }
        if title.is_some() || !titles.is_empty() {
            term.set_title(title.clone());
        }
        if region != (0, rows as i32) {
            // `start+1 .. end`, widened where the terminal would otherwise
            // refuse a one-line or empty region; it clamps to the same thing.
            term.set_scrolling_region((region.0 + 1) as usize, Some(region.1.max(region.0 + 2) as usize));
        }
        for &(bit, how) in MODES {
            if mode.contains(bit) != term.mode().contains(bit) {
                set_mode(&mut term, how, mode.contains(bit));
            }
        }

        let kitty_hidden = TermMode::from_bits_truncate((flags >> 32) as u32) & TermMode::KITTY_KEYBOARD_PROTOCOL;
        if mode.contains(TermMode::ALT_SCREEN) {
            term.grid_mut().cursor = cursor;
            push_kitty(&mut term, kitty_hidden);
            term.swap_alt();
            for k in 0..rows {
                d.row(&mut term.grid_mut()[Line(k as i32)], &protos, cols)?;
            }
            term.grid_mut().cursor = d.cursor(&protos, cols, rows)?;
            term.grid_mut().saved_cursor = d.cursor(&protos, cols, rows)?;
        } else {
            let saved = d.cursor(&protos, cols, rows)?;
            if flags & 2 != 0 {
                let alt_saved = d.cursor(&protos, cols, rows)?;
                term.swap_alt();
                push_kitty(&mut term, kitty_hidden);
                term.grid_mut().saved_cursor = alt_saved;
                term.swap_alt();
            }
            term.grid_mut().cursor = cursor;
            term.grid_mut().saved_cursor = saved;
        }
        // The shown screen's keyboard flags, last: switching screens resets them.
        push_kitty(&mut term, mode);

        if *term.mode() != mode {
            return Err(bad("snapshot modes could not be restored"));
        }

        let mut proc = Processor::new();
        if let Some(c) = rep {
            proc.advance(&mut Null, c.encode_utf8(&mut [0u8; 4]).as_bytes());
        }
        let track = tracked.then(|| {
            Box::new(Shadow {
                region,
                tabs,
                charset,
                title,
                titles,
                calls: 0,
                last: rep,
                alt_seen: mode.contains(TermMode::ALT_SCREEN) || flags & 2 != 0,
                hist_cleared: false,
            })
        });
        Ok(Replay { term, proc, track, clean: true, truncated: flags & 1 != 0, midline: flags & 4 != 0, short: false, poisoned: false, sync_since: None })
    }
}

/// Rows of scrollback behind the main screen, whichever screen is showing.
/// Call it only straight after `resize_term`, which leaves the hidden main
/// screen's saved cursor equal to its cursor: the look costs a swap there and
/// back, and the swap back sets exactly that.
fn main_history(term: &mut Term<Listener>) -> usize {
    if !term.mode().contains(TermMode::ALT_SCREEN) {
        return term.history_size();
    }
    // The swap back wipes the alternate screen: put it back as it was.
    let alt = term.grid().clone();
    term.swap_alt();
    let rows = term.history_size();
    term.swap_alt();
    *term.grid_mut() = alt;
    rows
}

/// The kitty keyboard flags in a set of modes, as the parser's own type.
fn kitty_of(mode: TermMode) -> KeyboardModes {
    let mut k = KeyboardModes::NO_MODE;
    for (bit, flag) in [
        (TermMode::DISAMBIGUATE_ESC_CODES, KeyboardModes::DISAMBIGUATE_ESC_CODES),
        (TermMode::REPORT_EVENT_TYPES, KeyboardModes::REPORT_EVENT_TYPES),
        (TermMode::REPORT_ALTERNATE_KEYS, KeyboardModes::REPORT_ALTERNATE_KEYS),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, KeyboardModes::REPORT_ALL_KEYS_AS_ESC),
        (TermMode::REPORT_ASSOCIATED_TEXT, KeyboardModes::REPORT_ASSOCIATED_TEXT),
    ] {
        if mode.contains(bit) {
            k |= flag;
        }
    }
    k
}

/// Keyboard flags as the top of a screen's stack, which is where they must
/// be to survive a switch between screens. Each screen keeps its own stack;
/// a keyframe carries the top of each and nothing beneath: a program that
/// pops twice after a keyframe is back at plain keys one pop early.
fn push_kitty(term: &mut Term<Listener>, mode: TermMode) {
    if mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) {
        term.push_keyboard_mode(kitty_of(mode));
    }
}

const MODES: &[(TermMode, u16)] = &[
    (TermMode::SHOW_CURSOR, 25),
    (TermMode::APP_CURSOR, 1),
    (TermMode::APP_KEYPAD, 0),
    (TermMode::MOUSE_REPORT_CLICK, 1000),
    (TermMode::MOUSE_DRAG, 1002),
    (TermMode::MOUSE_MOTION, 1003),
    (TermMode::BRACKETED_PASTE, 2004),
    (TermMode::SGR_MOUSE, 1006),
    (TermMode::UTF8_MOUSE, 1005),
    (TermMode::LINE_WRAP, 7),
    (TermMode::LINE_FEED_NEW_LINE, 20),
    (TermMode::INSERT, 4),
    (TermMode::FOCUS_IN_OUT, 1004),
    (TermMode::ALTERNATE_SCROLL, 1007),
    (TermMode::URGENCY_HINTS, 1042),
    (TermMode::ORIGIN, 6),
];

fn set_mode(term: &mut Term<Listener>, how: u16, on: bool) {
    use NamedPrivateMode as P;
    let private = |m: P| PrivateMode::Named(m);
    let m = match how {
        0 => {
            return if on { term.set_keypad_application_mode() } else { term.unset_keypad_application_mode() };
        }
        4 | 20 => {
            let m = Mode::Named(if how == 4 { NamedMode::Insert } else { NamedMode::LineFeedNewLine });
            return if on { term.set_mode(m) } else { term.unset_mode(m) };
        }
        1 => private(P::CursorKeys),
        6 => private(P::Origin),
        7 => private(P::LineWrap),
        25 => private(P::ShowCursor),
        1000 => private(P::ReportMouseClicks),
        1002 => private(P::ReportCellMouseMotion),
        1003 => private(P::ReportAllMouseMotion),
        1004 => private(P::ReportFocusInOut),
        1005 => private(P::Utf8Mouse),
        1006 => private(P::SgrMouse),
        1007 => private(P::AlternateScroll),
        1042 => private(P::UrgencyHints),
        _ => private(P::BracketedPaste),
    };
    if on {
        term.set_private_mode(m)
    } else {
        term.unset_private_mode(m)
    }
}

fn shape_code(s: CursorShape) -> u64 {
    match s {
        CursorShape::Block => 0,
        CursorShape::Underline => 1,
        CursorShape::Beam => 2,
        CursorShape::HollowBlock => 3,
        CursorShape::Hidden => 4,
    }
}

fn shape_of(c: u64) -> CursorShape {
    match c {
        0 => CursorShape::Block,
        1 => CursorShape::Underline,
        3 => CursorShape::HollowBlock,
        4 => CursorShape::Hidden,
        _ => CursorShape::Beam,
    }
}

const NO_COLOR: u32 = 3 << 24;
const NAMED: [NamedColor; 29] = [
    NamedColor::Black,
    NamedColor::Red,
    NamedColor::Green,
    NamedColor::Yellow,
    NamedColor::Blue,
    NamedColor::Magenta,
    NamedColor::Cyan,
    NamedColor::White,
    NamedColor::BrightBlack,
    NamedColor::BrightRed,
    NamedColor::BrightGreen,
    NamedColor::BrightYellow,
    NamedColor::BrightBlue,
    NamedColor::BrightMagenta,
    NamedColor::BrightCyan,
    NamedColor::BrightWhite,
    NamedColor::Foreground,
    NamedColor::Background,
    NamedColor::Cursor,
    NamedColor::DimBlack,
    NamedColor::DimRed,
    NamedColor::DimGreen,
    NamedColor::DimYellow,
    NamedColor::DimBlue,
    NamedColor::DimMagenta,
    NamedColor::DimCyan,
    NamedColor::DimWhite,
    NamedColor::BrightForeground,
    NamedColor::DimForeground,
];

fn color_code(c: Color) -> u32 {
    match c {
        Color::Named(n) => n as u32,
        Color::Spec(c) => 1 << 24 | (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32,
        Color::Indexed(i) => 2 << 24 | i as u32,
    }
}

fn color_of(x: u32) -> io::Result<Color> {
    Ok(match x >> 24 {
        0 => Color::Named(*NAMED.iter().find(|&&n| n as u32 == x).ok_or_else(|| bad("snapshot colour name"))?),
        1 => Color::Spec(Rgb { r: (x >> 16) as u8, g: (x >> 8) as u8, b: x as u8 }),
        2 => Color::Indexed(x as u8),
        _ => return Err(bad("snapshot colour")),
    })
}

fn put_u(v: &mut Vec<u8>, mut x: u64) {
    while x >= 0x80 {
        v.push(x as u8 | 0x80);
        x >>= 7;
    }
    v.push(x as u8);
}

fn put_i(v: &mut Vec<u8>, x: i64) {
    put_u(v, ((x << 1) ^ (x >> 63)) as u64);
}

fn put_str(v: &mut Vec<u8>, s: &str) {
    put_u(v, s.len() as u64);
    v.extend_from_slice(s.as_bytes());
}

fn put_opt_str(v: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => put_u(v, 0),
        Some(s) => {
            put_u(v, s.len() as u64 + 1);
            v.extend_from_slice(s.as_bytes());
        }
    }
}

fn is_blank(c: &Cell) -> bool {
    c.c == ' '
        && c.extra.is_none()
        && c.flags.is_empty()
        && c.fg == Color::Named(NamedColor::Foreground)
        && c.bg == Color::Named(NamedColor::Background)
}

/// Writes rows as runs of cells that share their attributes. Attributes and
/// hyperlinks are numbered as they are first met.
#[derive(Default)]
struct Enc {
    out: Vec<u8>,
    attrs: Vec<[u32; 5]>,
    attr_ix: HashMap<[u32; 5], u32>,
    links: Vec<(String, String)>,
    link_ix: HashMap<(String, String), u32>,
    last: Option<([u32; 5], u32)>,
    run: Vec<u8>,
}

impl Enc {
    fn attr(&mut self, c: &Cell) -> u32 {
        let mut key = [color_code(c.fg), color_code(c.bg), NO_COLOR, c.flags.bits() as u32, 0];
        if c.extra.is_some() {
            key[2] = c.underline_color().map_or(NO_COLOR, color_code);
            if let Some(h) = c.hyperlink() {
                let k = (h.id().to_string(), h.uri().to_string());
                let n = self.links.len() as u32 + 1;
                key[4] = *self.link_ix.entry(k.clone()).or_insert_with(|| {
                    self.links.push(k);
                    n
                });
            }
        }
        if let Some((k, ix)) = self.last {
            if k == key {
                return ix;
            }
        }
        let n = self.attrs.len() as u32;
        let ix = *self.attr_ix.entry(key).or_insert_with(|| {
            self.attrs.push(key);
            n
        });
        self.last = Some((key, ix));
        ix
    }

    fn row(&mut self, cells: &[Cell]) {
        let n = cells.iter().rposition(|c| !is_blank(c)).map_or(0, |i| i + 1);
        put_u(&mut self.out, n as u64);
        let mut i = 0;
        let mut marks = 0usize;
        while i < n {
            let ix = self.attr(&cells[i]);
            let start = i;
            self.run.clear();
            while i < n && (i == start || self.attr(&cells[i]) == ix) {
                let c = &cells[i];
                self.run.extend_from_slice(c.c.encode_utf8(&mut [0u8; 4]).as_bytes());
                marks += c.zerowidth().is_some_and(|z| !z.is_empty()) as usize;
                i += 1;
            }
            put_u(&mut self.out, ix as u64);
            put_u(&mut self.out, (i - start) as u64);
            self.out.extend_from_slice(&self.run);
        }
        // Combining marks are rare: listed after the row instead of costing every cell a byte.
        put_u(&mut self.out, marks as u64);
        if marks > 0 {
            for (col, c) in cells[..n].iter().enumerate() {
                if let Some(z) = c.zerowidth().filter(|z| !z.is_empty()) {
                    put_u(&mut self.out, col as u64);
                    put_u(&mut self.out, z.len() as u64);
                    z.iter().for_each(|&ch| put_u(&mut self.out, ch as u64));
                }
            }
        }
    }

    /// The newest `hist_rows` rows of scrollback and the screen. The cut is
    /// moved up to where a line starts, by up to 200 rows, so that what is
    /// kept can be re-wrapped by itself. Returns how many scrollback rows
    /// were written, and flag bits: 1 if older rows were left out, 4 if the
    /// cut still fell inside a wrapped line.
    fn grid(&mut self, g: &Grid<Cell>, hist_rows: usize) -> (usize, u64) {
        let all = g.history_size();
        let mut n = all.min(hist_rows);
        // Does scrollback row `k` (1 = newest) wrap into the row below it?
        let wraps = |k: usize| g[Line(-(k as i32))][..].last().is_some_and(|c| c.flags.contains(Flags::WRAPLINE));
        let limit = (n + 200).min(all);
        while n < limit && wraps(n + 1) {
            n += 1;
        }
        for line in -(n as i32)..g.screen_lines() as i32 {
            self.row(&g[Line(line)][..]);
        }
        (n, (n < all) as u64 | ((n < all && wraps(n + 1)) as u64) << 2)
    }

    fn cursor(&mut self, c: &Cursor<Cell>) {
        put_i(&mut self.out, c.point.line.0 as i64);
        put_u(&mut self.out, c.point.column.0 as u64);
        put_u(&mut self.out, c.input_needs_wrap as u64);
        for i in [CharsetIndex::G0, CharsetIndex::G1, CharsetIndex::G2, CharsetIndex::G3] {
            self.out.push((c.charsets[i] == StandardCharset::SpecialCharacterAndLineDrawing) as u8);
        }
        let ix = self.attr(&c.template);
        put_u(&mut self.out, ix as u64);
        put_u(&mut self.out, c.template.c as u64);
    }
}

struct Dec<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Dec<'a> {
    fn u(&mut self) -> io::Result<u64> {
        let (mut x, mut shift) = (0u64, 0);
        loop {
            let b = *self.b.get(self.i).ok_or_else(|| bad("snapshot truncated"))?;
            self.i += 1;
            x |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Ok(x);
            }
            shift += 7;
            if shift > 63 {
                return Err(bad("snapshot number"));
            }
        }
    }

    fn i(&mut self) -> io::Result<i64> {
        let x = self.u()?;
        Ok((x >> 1) as i64 ^ -((x & 1) as i64))
    }

    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let s = self.b.get(self.i..self.i + n).ok_or_else(|| bad("snapshot truncated"))?;
        self.i += n;
        Ok(s)
    }

    fn str(&mut self) -> io::Result<String> {
        let n = self.u()? as usize;
        String::from_utf8(self.bytes(n)?.to_vec()).map_err(|_| bad("snapshot text"))
    }

    fn opt_str(&mut self) -> io::Result<Option<String>> {
        match self.u()? as usize {
            0 => Ok(None),
            n => String::from_utf8(self.bytes(n - 1)?.to_vec()).map(Some).map_err(|_| bad("snapshot text")),
        }
    }

    fn ch(&mut self) -> io::Result<char> {
        let b = *self.b.get(self.i).ok_or_else(|| bad("snapshot truncated"))?;
        if b < 0x80 {
            self.i += 1;
            return Ok(b as char);
        }
        let n = if b >> 5 == 0b110 { 2 } else if b >> 4 == 0b1110 { 3 } else { 4 };
        let s = self.bytes(n)?;
        std::str::from_utf8(s).ok().and_then(|s| s.chars().next()).ok_or_else(|| bad("snapshot text"))
    }

    /// Write one stored row into a grid row.
    fn row(&mut self, row: &mut alacritty_terminal::grid::Row<Cell>, protos: &[Cell], cols: usize) -> io::Result<()> {
        // Not every row handed in is blank: entering the alternate screen
        // clears it to the cursor's background colour, whatever that is.
        row.reset(&Cell::default());
        let n = self.u()? as usize;
        if n > cols {
            return Err(bad("snapshot row"));
        }
        let mut col = 0;
        while col < n {
            let proto = protos.get(self.u()? as usize).ok_or_else(|| bad("snapshot attribute"))?;
            let len = self.u()? as usize;
            if len == 0 || col + len > n {
                return Err(bad("snapshot run"));
            }
            for _ in 0..len {
                let mut cell = proto.clone();
                cell.c = self.ch()?;
                row[Column(col)] = cell;
                col += 1;
            }
        }
        for _ in 0..self.u()? {
            let col = self.u()? as usize;
            if col >= n {
                return Err(bad("snapshot mark"));
            }
            for _ in 0..self.u()? {
                let c = char::from_u32(self.u()? as u32).ok_or_else(|| bad("snapshot char"))?;
                row[Column(col)].push_zerowidth(c);
            }
        }
        Ok(())
    }

    fn cursor(&mut self, protos: &[Cell], cols: usize, rows: usize) -> io::Result<Cursor<Cell>> {
        let mut c = Cursor::<Cell>::default();
        let (line, col) = (self.i()?, self.u()? as usize);
        if line < 0 || line >= rows as i64 || col >= cols {
            return Err(bad("snapshot cursor"));
        }
        c.point.line = Line(line as i32);
        c.point.column = Column(col);
        c.input_needs_wrap = self.u()? != 0;
        let sets = self.bytes(4)?;
        for (i, ix) in [CharsetIndex::G0, CharsetIndex::G1, CharsetIndex::G2, CharsetIndex::G3].into_iter().enumerate() {
            c.charsets[ix] = if sets[i] != 0 { StandardCharset::SpecialCharacterAndLineDrawing } else { StandardCharset::Ascii };
        }
        c.template = protos.get(self.u()? as usize).ok_or_else(|| bad("snapshot attribute"))?.clone();
        c.template.c = char::from_u32(self.u()? as u32).ok_or_else(|| bad("snapshot char"))?;
        Ok(c)
    }
}

// --------------------------------------------------------------- comparison

fn auto_id(id: &str) -> bool {
    id.ends_with("_alacritty")
}

/// Two cells that mean the same thing. Links the terminal numbered itself
/// are compared by address only, because that numbering is per process.
fn cell_eq(a: &Cell, b: &Cell) -> bool {
    if a == b {
        return true;
    }
    let marks = |c: &Cell| c.zerowidth().map(|z| z.to_vec()).unwrap_or_default();
    let link = |c: &Cell| c.hyperlink().map(|h| (if auto_id(h.id()) { String::new() } else { h.id().to_string() }, h.uri().to_string()));
    a.c == b.c
        && a.fg == b.fg
        && a.bg == b.bg
        && a.flags == b.flags
        && a.underline_color() == b.underline_color()
        && marks(a) == marks(b)
        && link(a) == link(b)
}

fn cursor_eq(a: &Cursor<Cell>, b: &Cursor<Cell>) -> bool {
    a.point == b.point && a.input_needs_wrap == b.input_needs_wrap && a.charsets == b.charsets && cell_eq(&a.template, &b.template)
}

/// The first way two terminals differ, or `None` if they are the same:
/// size, modes, cursor style, palette, cursor, saved cursor, every cell of
/// the screen, and `depth` rows of scrollback (`None`: all of it, and the
/// same amount of it).
pub fn diff(a: &Term<Listener>, b: &Term<Listener>, depth: Option<usize>) -> Option<String> {
    if (a.columns(), a.screen_lines()) != (b.columns(), b.screen_lines()) {
        return Some(format!("size {}x{} vs {}x{}", a.columns(), a.screen_lines(), b.columns(), b.screen_lines()));
    }
    if a.mode() != b.mode() {
        return Some(format!("modes {:?} vs {:?}", a.mode(), b.mode()));
    }
    if a.cursor_style() != b.cursor_style() {
        return Some("cursor style".into());
    }
    if let Some(i) = (0..COLOR_COUNT).find(|&i| a.colors()[i] != b.colors()[i]) {
        return Some(format!("palette entry {i}"));
    }
    let (ga, gb) = (a.grid(), b.grid());
    if !cursor_eq(&ga.cursor, &gb.cursor) {
        return Some(format!("cursor {:?} vs {:?}", ga.cursor.point, gb.cursor.point));
    }
    if !cursor_eq(&ga.saved_cursor, &gb.saved_cursor) {
        return Some(format!("saved cursor {:?} vs {:?}", ga.saved_cursor.point, gb.saved_cursor.point));
    }
    let hist = match depth {
        None if ga.history_size() != gb.history_size() => {
            return Some(format!("scrollback {} vs {} rows", ga.history_size(), gb.history_size()));
        }
        None => ga.history_size(),
        Some(d) if ga.history_size() < d || gb.history_size() < d => {
            return Some(format!("scrollback {} and {} rows, wanted {d}", ga.history_size(), gb.history_size()));
        }
        Some(d) => d,
    };
    for line in -(hist as i32)..a.screen_lines() as i32 {
        let (ra, rb) = (&ga[Line(line)][..], &gb[Line(line)][..]);
        if let Some(col) = (0..ra.len()).find(|&c| !cell_eq(&ra[c], &rb[c])) {
            return Some(format!("line {line} column {col}: {:?} vs {:?}", ra[col], rb[col]));
        }
    }
    None
}

// --------------------------------------------------------------------- text

fn push_row(s: &mut String, row: &[Cell]) {
    for c in row {
        if c.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        s.push(if c.c == '\t' || c.c == '\0' { ' ' } else { c.c });
        if let Some(z) = c.zerowidth() {
            s.extend(z);
        }
    }
}

/// The screen as text, one string per row, trailing blanks removed.
pub fn screen_text(term: &Term<Listener>) -> Vec<String> {
    (0..term.screen_lines() as i32)
        .map(|l| {
            let mut s = String::new();
            push_row(&mut s, &term.grid()[Line(l)][..]);
            s.truncate(s.trim_end_matches(' ').len());
            s
        })
        .collect()
}

/// A journal time as the command line prints it: seconds, to the microsecond.
/// (To the millisecond, a moment `find` reported and `at --until` was then
/// given could fall just before the record that made it so.)
pub fn seconds(us: u64) -> f64 {
    us as f64 / 1e6
}

/// Seconds given on the command line, as a journal time. Rounded, not cut
/// short: 0.651234 is not a number a machine holds exactly, and it must
/// still mean microsecond 651234.
pub fn micros(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1e6).round() as u64
}

/// `H:MM:SS.mmm` (hours only when there are any).
pub fn clock(us: u64) -> String {
    let (ms, s) = ((us / 1000) % 1000, us / 1_000_000);
    if s >= 3600 {
        format!("{}:{:02}:{:02}.{ms:03}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{:02}:{:02}.{ms:03}", s / 60, s % 60)
    }
}

// -------------------------------------------------------------------- index

pub fn sidecar(journal: &Path) -> PathBuf {
    journal.with_extension("prtk")
}

/// A keyframe's checksum covers where it claims to belong as well as what
/// it holds: one moved or relabelled is as wrong as one corrupted.
fn frame_sum(pos: u64, off: u64, t_us: u64, data: &[u8]) -> u64 {
    let mut h = FNV0;
    for x in [pos, off, t_us, data.len() as u64] {
        h = fnv64(h, &x.to_le_bytes());
    }
    fnv64(h, data)
}

/// One keyframe: the state after `pos` records, the next of which starts at
/// journal offset `off`.
pub struct Frame {
    pub pos: u64,
    pub off: u64,
    pub t_us: u64,
    /// Size of the snapshot in bytes.
    pub len: u32,
    sum: u64,
    /// Where the snapshot sits in the sidecar, once it is there.
    at: u64,
    /// The snapshot itself until it has been written to the sidecar.
    data: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug)]
pub enum Target {
    /// Everything up to and including this many microseconds into the session.
    Time(u64),
    /// Exactly this many records.
    Pos(u64),
    End,
}

impl Target {
    /// The moment whose screen is wanted, given the time of the last record
    /// applied: the time asked for; or the instant that record landed; or,
    /// for the end, any time later (whatever the journal holds is shown).
    fn shown_at(self, last: u64) -> u64 {
        match self {
            Target::Time(t) => t.max(last),
            Target::Pos(_) => last,
            Target::End => u64::MAX,
        }
    }
}

/// A terminal at a chosen moment, and how it was reached.
pub struct Seeked {
    pub replay: Replay,
    /// Records applied.
    pub pos: u64,
    /// Time of the last record applied.
    pub t_us: u64,
    /// No record follows.
    pub at_end: bool,
    /// Journal bytes replayed to get here.
    pub replayed: u64,
    /// "keyframe", "start" (no keyframe before this moment), "build" (the
    /// index had to be extended to here) or "full" (rebuilt from the start
    /// because a resize needed more scrollback than a keyframe carries).
    pub how: &'static str,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct IndexStats {
    /// Keyframes added by this process.
    pub added: usize,
    /// Keyframes that did not reproduce their terminal and were dropped.
    pub rejected: usize,
    /// Moments at which a keyframe was due but the parser was not at rest.
    pub deferred: usize,
}

pub struct Index {
    journal: PathBuf,
    pub head: Head,
    pub frames: Vec<Frame>,
    /// Frames already in the sidecar file.
    stored: usize,
    /// Sidecar length when last read or written by this process.
    side_len: u64,
    /// Sidecar bytes that are valid; anything after is a torn write.
    side_valid: u64,
    side_ok: bool,
    /// Keyframe spacing in journal bytes and scrollback rows per keyframe
    /// (`KF_BYTES` and `HIST_TAIL` unless a test wants many small ones).
    pub every: u64,
    pub ratio: u64,
    pub tail: usize,
    pub stats: IndexStats,
    /// Why the first dropped keyframe was dropped.
    pub first_reject: Option<String>,
}

impl Index {
    /// Open a journal's index, loading whatever part of the sidecar is valid.
    pub fn open(journal: &Path) -> io::Result<Index> {
        let (head, rd) = Reader::open(journal)?;
        let journal_len = rd.len;
        let mut ix = Index {
            journal: journal.to_path_buf(),
            head,
            frames: Vec::new(),
            stored: 0,
            side_len: 0,
            side_valid: 0,
            side_ok: false,
            every: KF_BYTES,
            ratio: KF_RATIO,
            tail: HIST_TAIL,
            stats: IndexStats::default(),
            first_reject: None,
        };
        ix.load(journal_len);
        Ok(ix)
    }

    /// Hash of the first `n` bytes of this journal's records.
    fn lead_hash(&self, n: u64) -> io::Result<u64> {
        let mut f = File::open(&self.journal)?;
        f.seek(SeekFrom::Start(self.head.data_start))?;
        let mut b = vec![0u8; n as usize];
        f.read_exact(&mut b)?;
        Ok(fnv64(FNV0, &b))
    }

    /// A new sidecar's header: what made the index, and which journal it is
    /// an index of (as the journal stands now; what it has so far never changes).
    fn side_header(&self) -> io::Result<Vec<u8>> {
        let lead = std::fs::metadata(&self.journal)?.len().saturating_sub(self.head.data_start).min(LEAD);
        let meta = serde_json::json!({ "engine": ENGINE, "journal": self.head.id, "lead": lead, "lead_hash": self.lead_hash(lead)? });
        let meta = serde_json::to_vec(&meta).unwrap_or_default();
        let mut h = SIDE_MAGIC.to_vec();
        h.extend_from_slice(&(meta.len() as u32).to_le_bytes());
        h.extend_from_slice(&meta);
        Ok(h)
    }

    /// Read a sidecar's header. Its length, if it is the header of an index
    /// of this journal made by this engine.
    fn side_is_ours(&self, f: &mut File, journal_len: u64) -> Option<u64> {
        let mut h = [0u8; 12];
        f.read_exact(&mut h).ok()?;
        let mlen = u32::from_le_bytes([h[8], h[9], h[10], h[11]]) as u64;
        if &h[..8] != SIDE_MAGIC || mlen > 4096 {
            return None;
        }
        let mut m = vec![0u8; mlen as usize];
        f.read_exact(&mut m).ok()?;
        let meta: serde_json::Value = serde_json::from_slice(&m).ok()?;
        let lead = meta["lead"].as_u64().filter(|&n| n <= LEAD && self.head.data_start + n <= journal_len)?;
        let same = meta["engine"].as_str() == Some(ENGINE)
            && meta["journal"].as_u64() == Some(self.head.id)
            && self.lead_hash(lead).ok()? == meta["lead_hash"].as_u64()?;
        same.then_some(12 + mlen)
    }

    /// Read the sidecar's table of keyframes, hopping over the snapshots
    /// themselves: one is only read (and its checksum tested) when used.
    fn load(&mut self, journal_len: u64) {
        let Ok(mut f) = File::open(sidecar(&self.journal)) else { return };
        self.side_len = f.metadata().map(|m| m.len()).unwrap_or(0);
        // Another journal's, other records under the same header, another
        // engine's, or not an index at all: left alone here and made again.
        let Some(mut at) = self.side_is_ours(&mut f, journal_len) else { return };
        self.side_ok = true;
        self.side_valid = at;
        // Entry: u32 len | u64 pos | u64 off | u64 t_us | u64 hash | snapshot.
        let mut h = [0u8; 36];
        while at + 36 <= self.side_len && f.read_exact(&mut h).is_ok() {
            let n = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
            let q = |k: usize| u64::from_le_bytes(h[k..k + 8].try_into().unwrap());
            let (pos, off, t_us, sum) = (q(4), q(12), q(20), q(28));
            let last = self.frames.last().map_or((0, self.head.data_start, 0), |f| (f.pos, f.off, f.t_us));
            let whole = at + 36 + n as u64 <= self.side_len;
            let in_order = pos > last.0 && off > last.1 && t_us >= last.2 && off <= journal_len;
            if !whole || !in_order || f.seek(SeekFrom::Current(n as i64)).is_err() {
                break;
            }
            self.frames.push(Frame { pos, off, t_us, len: n, sum, at: at + 36, data: None });
            at += 36 + n as u64;
            self.side_valid = at;
        }
        self.stored = self.frames.len();
    }

    /// Keyframe `k`'s snapshot. A damaged one takes itself and everything
    /// after it out of the index (they are made again on the next build).
    fn snapshot_of(&mut self, k: usize) -> Option<Vec<u8>> {
        let f = &self.frames[k];
        if let Some(d) = &f.data {
            return Some(d.clone());
        }
        let read = || -> io::Result<Vec<u8>> {
            let mut file = File::open(sidecar(&self.journal))?;
            file.seek(SeekFrom::Start(f.at))?;
            let mut d = vec![0u8; f.len as usize];
            file.read_exact(&mut d)?;
            Ok(d)
        };
        match read() {
            Ok(d) if frame_sum(f.pos, f.off, f.t_us, &d) == f.sum => Some(d),
            _ => {
                self.drop_from(k);
                None
            }
        }
    }

    fn drop_from(&mut self, k: usize) {
        if k < self.stored {
            self.side_valid = self.frames[k].at - 36;
            self.stored = k;
        }
        self.frames.truncate(k);
    }

    /// Append keyframes made since the last save. Never fatal: without a
    /// sidecar the index still works, it is just rebuilt next time.
    fn save(&mut self) {
        if self.stored == self.frames.len() {
            return;
        }
        let path = sidecar(&self.journal);
        let on_disk = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if on_disk != self.side_len {
            return; // another process wrote it meanwhile; its copy stands
        }
        let result = (|| -> io::Result<u64> {
            use std::os::windows::fs::OpenOptionsExt;
            // Readers may read while this writes; a second writer may not open it.
            let mut f = std::fs::OpenOptions::new().write(true).create(true).share_mode(1).open(&path)?;
            let mut at = self.side_valid;
            if !self.side_ok {
                let h = self.side_header()?;
                f.set_len(0)?;
                f.write_all(&h)?;
                at = h.len() as u64;
            } else {
                f.set_len(at)?;
                f.seek(SeekFrom::Start(at))?;
            }
            for fr in &mut self.frames[self.stored..] {
                let data = fr.data.as_deref().unwrap_or(&[]);
                let mut e = Vec::with_capacity(36 + data.len());
                e.extend_from_slice(&fr.len.to_le_bytes());
                for x in [fr.pos, fr.off, fr.t_us, fr.sum] {
                    e.extend_from_slice(&x.to_le_bytes());
                }
                e.extend_from_slice(data);
                f.write_all(&e)?;
                fr.at = at + 36;
                at += e.len() as u64;
            }
            f.flush()?;
            Ok(at)
        })();
        if let Ok(at) = result {
            self.side_ok = true;
            self.side_len = at;
            self.side_valid = at;
            self.stored = self.frames.len();
            // The sidecar holds them now; no need to hold them twice.
            self.frames.iter_mut().for_each(|f| f.data = None);
        }
    }

    /// Bytes the keyframes take in the sidecar.
    pub fn bytes(&self) -> u64 {
        self.frames.iter().map(|f| f.len as u64 + 36).sum()
    }

    fn fresh(&self, tracked: bool) -> (Replay, u64, u64) {
        let (cols, rows) = self.head.size();
        (Replay::new(cols, rows, tracked), 0, self.head.data_start)
    }

    /// Apply records from `off` until `target`. With `emit`, keyframes are
    /// taken on the way wherever one is due and possible.
    #[allow(clippy::too_many_arguments)]
    fn drive(
        &mut self,
        rp: &mut Replay,
        pos: &mut u64,
        off: &mut u64,
        t_us: &mut u64,
        target: Target,
        emit: bool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> io::Result<bool> {
        let mut rd = Reader::at(&self.journal, *off)?;
        let total = rd.len;
        let mut buf = Vec::new();
        let mut told = *off;
        loop {
            if matches!(target, Target::Pos(p) if *pos >= p) {
                return Ok(rd.next().is_none());
            }
            let Some(h) = rd.next() else { return Ok(true) };
            if matches!(target, Target::Time(lim) if h.t_us > lim) {
                return Ok(false);
            }
            if h.kind == journal::K_OUTPUT || h.kind == journal::K_RESIZE {
                rd.payload(&h, &mut buf)?;
                rp.apply_at(h.kind, &buf, h.t_us);
            } else {
                rd.skip(&h)?;
            }
            *pos += 1;
            *off = rd.pos;
            *t_us = h.t_us;
            if *off - told >= 4 << 20 {
                told = *off;
                progress(*off, total);
            }
            // Keyframes are spaced by journal bytes, and further apart when
            // they are large, so the sidecar stays a small part of the journal.
            let (last_pos, due) = self.frames.last().map_or((0, self.head.data_start + self.every), |f| (f.pos, f.off + self.every.max(f.len as u64 * self.ratio)));
            if emit && rp.tracked() && *pos > last_pos && *off >= due {
                match rp.snapshot(self.tail) {
                    Some(data) => self.keep(rp, data, *pos, *off, *t_us),
                    None => self.stats.deferred += 1,
                }
            }
        }
    }

    /// Store a keyframe, but only once it has been restored and found to be
    /// the terminal it was taken from.
    fn keep(&mut self, rp: &Replay, data: Vec<u8>, pos: u64, off: u64, t_us: u64) {
        let verdict = match Replay::restore(&data, false) {
            Ok(back) => diff(&rp.term, &back.term, Some(back.term.history_size())),
            Err(e) => Some(e.to_string()),
        };
        match verdict {
            None => {
                self.frames.push(Frame { pos, off, t_us, len: data.len() as u32, sum: frame_sum(pos, off, t_us, &data), at: 0, data: Some(data) });
                self.stats.added += 1;
            }
            Some(why) => {
                self.stats.rejected += 1;
                self.first_reject.get_or_insert(why);
            }
        }
    }

    /// The terminal as it stood at `target`, at the size the session had
    /// then; with `view`, refitted to a viewer's grid the way a window refits
    /// a replayed session.
    pub fn seek(&mut self, target: Target, view: Option<(usize, usize)>) -> io::Result<Seeked> {
        self.seek_with(target, view, &mut |_, _| {})
    }

    pub fn seek_with(&mut self, target: Target, view: Option<(usize, usize)>, progress: &mut dyn FnMut(u64, u64)) -> io::Result<Seeked> {
        let (mut rp, mut pos, mut off, mut t_us, mut how, building) = loop {
            let usable = match target {
                Target::Time(t) => self.frames.partition_point(|f| f.t_us <= t),
                Target::Pos(p) => self.frames.partition_point(|f| f.pos <= p),
                Target::End => self.frames.len(),
            };
            // Inside the indexed part a keyframe and a short tail is all it
            // takes. Past its end the index is extended on the way.
            let building = usable == self.frames.len();
            let Some(k) = usable.checked_sub(1) else {
                let (rp, pos, off) = self.fresh(building);
                break (rp, pos, off, 0, if building { "build" } else { "start" }, building);
            };
            // A keyframe that cannot be read back is dropped and the next one down tried.
            let Some(data) = self.snapshot_of(k) else { continue };
            match Replay::restore(&data, building) {
                Ok(rp) => {
                    let f = &self.frames[k];
                    break (rp, f.pos, f.off, f.t_us, if building { "build" } else { "keyframe" }, building);
                }
                Err(_) => self.drop_from(k),
            }
        };
        let from = off;
        let mut at_end = self.drive(&mut rp, &mut pos, &mut off, &mut t_us, target, building, progress)?;
        let mut replayed = off - from;
        // What was on screen then, fitted to the viewer's grid as a window fits a replay.
        let fitted = |rp: &mut Replay, last: u64| {
            rp.settle_at(target.shown_at(last));
            if let Some((c, r)) = view {
                rp.resize(c, r);
            }
            !rp.short && !rp.poisoned
        };
        if !fitted(&mut rp, t_us) {
            // Scrollback older than the keyframe's tail was needed. Start
            // over from the very beginning, where nothing is missing.
            let (r2, p2, o2) = self.fresh(true);
            (rp, pos, off, t_us, how) = (r2, p2, o2, 0, "full");
            at_end = self.drive(&mut rp, &mut pos, &mut off, &mut t_us, target, true, progress)?;
            replayed += off - self.head.data_start;
            fitted(&mut rp, t_us);
        }
        self.save();
        Ok(Seeked { replay: rp, pos, t_us, at_end, replayed, how })
    }

    /// Index the whole journal as it stands.
    pub fn build(&mut self, progress: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
        self.seek_with(Target::End, None, progress).map(|_| ())
    }
}

/// The terminal as it stood `t_us` microseconds into a journal.
pub fn seek(journal: &Path, t_us: u64, view: Option<(usize, usize)>) -> io::Result<Seeked> {
    Index::open(journal)?.seek(Target::Time(t_us), view)
}

/// The same answer with no index: every record from the start. This is the
/// definition keyframes are checked against, and the cost they remove.
pub fn replay_from_start(journal: &Path, target: Target, view: Option<(usize, usize)>) -> io::Result<Seeked> {
    let (head, mut rd) = Reader::open(journal)?;
    let (cols, rows) = head.size();
    let mut rp = Replay::new(cols, rows, false);
    let (mut pos, mut t_us, mut at_end, mut buf) = (0u64, 0u64, true, Vec::new());
    while let Some(h) = rd.next() {
        if matches!(target, Target::Pos(p) if pos >= p) || matches!(target, Target::Time(lim) if h.t_us > lim) {
            at_end = false;
            break;
        }
        if h.kind == journal::K_OUTPUT || h.kind == journal::K_RESIZE {
            rd.payload(&h, &mut buf)?;
            rp.apply_at(h.kind, &buf, h.t_us);
        } else {
            rd.skip(&h)?;
        }
        pos += 1;
        t_us = h.t_us;
    }
    rp.settle_at(target.shown_at(t_us));
    if let Some((c, r)) = view {
        rp.resize(c, r);
    }
    Ok(Seeked { replay: rp, pos, t_us, at_end, replayed: rd.pos - head.data_start, how: "start" })
}

// --------------------------------------------------------------------- find

/// One stretch of time during which the text was on screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub appeared_us: u64,
    /// `None`: still there when the journal ends.
    pub left_us: Option<u64>,
    /// The line it was first seen in, and that line's first screen row.
    pub line: String,
    pub row: usize,
}

pub struct Found {
    pub hits: Vec<Hit>,
    pub records: u64,
    pub bytes: u64,
    /// How many times the screen was examined.
    pub looks: u64,
}

/// Every moment `needle` was on screen: when it appeared and when it left.
///
/// "On screen" means inside one line as it was printed: rows that are one
/// soft-wrapped line are joined, so a path broken across two rows is found.
/// The screen is examined after every record and, inside a record, before
/// and after every line feed, so text that a flood scrolled straight past is
/// still seen; it is reported as appearing and leaving at that record's time.
pub fn find(journal: &Path, needle: &str, ignore_case: bool) -> io::Result<Found> {
    let mut out = Found { hits: Vec::new(), records: 0, bytes: 0, looks: 0 };
    if needle.is_empty() {
        return Ok(out);
    }
    let mut watch = Watch::new(needle, ignore_case);
    let mut open = false;
    (out.records, out.bytes) = walk(journal, |rp, t_us, lone| {
        out.looks += 1;
        if !watch.look(&mut rp.term, lone) {
            return;
        }
        match (watch.found(), open) {
            (Some((row, line)), false) => {
                out.hits.push(Hit { appeared_us: t_us, left_us: None, line, row });
                open = true;
            }
            (None, true) => {
                if let Some(h) = out.hits.last_mut() {
                    h.left_us = Some(t_us);
                }
                open = false;
            }
            _ => {}
        }
    })?;
    Ok(out)
}

/// Feed a journal to a fresh terminal piece by piece, calling `look` after
/// each piece with the record's time. Output is cut at every line feed and
/// each line feed is a piece of its own (`lone`): whatever a line feed
/// scrolls away was looked at just before it went.
fn walk(journal: &Path, mut look: impl FnMut(&mut Replay, u64, bool)) -> io::Result<(u64, u64)> {
    let (head, mut rd) = Reader::open(journal)?;
    let (cols, rows) = head.size();
    let mut rp = Replay::new(cols, rows, false);
    let (mut records, mut bytes, mut buf) = (0u64, 0u64, Vec::new());
    while let Some(h) = rd.next() {
        records += 1;
        // A synchronized update the window stopped waiting for was on screen
        // from that moment, not from whenever the program went on to close it.
        if let Some(shown) = rp.overdue(h.t_us) {
            rp.settle();
            look(&mut rp, shown, false);
        }
        match h.kind {
            journal::K_OUTPUT => {
                rd.payload(&h, &mut buf)?;
                bytes += buf.len() as u64;
                let begun = rp.sync_begun();
                let mut from = 0;
                while let Some(i) = buf[from..].iter().position(|&b| b == b'\n').map(|i| from + i) {
                    if i > from {
                        rp.feed(&buf[from..i]);
                        look(&mut rp, h.t_us, false);
                    }
                    rp.feed(b"\n");
                    look(&mut rp, h.t_us, true);
                    from = i + 1;
                }
                if from < buf.len() {
                    rp.feed(&buf[from..]);
                    look(&mut rp, h.t_us, false);
                }
                rp.note_sync(begun, h.t_us);
            }
            journal::K_RESIZE => {
                rd.payload(&h, &mut buf)?;
                rp.apply(h.kind, &buf);
                look(&mut rp, h.t_us, false);
            }
            _ => rd.skip(&h)?,
        }
    }
    // An update the journal leaves open: the window shows it once it has
    // waited long enough, and a seek to that moment or later shows it too.
    if let Some(shown) = rp.overdue(u64::MAX) {
        rp.settle();
        look(&mut rp, shown, false);
    }
    Ok((records, bytes))
}

/// What `find` remembers of the screen between looks, so that a look costs
/// what changed and not the whole screen: each row's text, whether the row
/// wraps into the next, whether the line it is part of holds the needle, and
/// where the row's cells live (rows move when the screen scrolls; their
/// cells do not).
struct Watch {
    needle: String,
    fold: bool,
    size: (usize, usize),
    text: Vec<String>,
    wrap: Vec<bool>,
    hit: Vec<bool>,
    at: Vec<usize>,
    todo: Vec<usize>,
    scratch: String,
}

impl Watch {
    fn new(needle: &str, fold: bool) -> Self {
        let needle = if fold { needle.to_lowercase() } else { needle.to_string() };
        Watch { needle, fold, size: (0, 0), text: Vec::new(), wrap: Vec::new(), hit: Vec::new(), at: Vec::new(), todo: Vec::new(), scratch: String::new() }
    }

    /// Read row `r` from the grid. True if its text or wrapping changed.
    fn read(&mut self, g: &Grid<Cell>, r: usize) -> bool {
        let row = &g[Line(r as i32)][..];
        let wrap = row.last().is_some_and(|c| c.flags.contains(Flags::WRAPLINE));
        // A row that wraps keeps its full width, so joined rows line up.
        let blank = |c: &Cell| matches!(c.c, ' ' | '\t' | '\0') && c.zerowidth().is_none_or(|z| z.is_empty());
        let end = if wrap { row.len() } else { row.iter().rposition(|c| !blank(c)).map_or(0, |i| i + 1) };
        self.scratch.clear();
        push_row(&mut self.scratch, &row[..end]);
        if self.fold {
            self.scratch = self.scratch.to_lowercase();
        }
        let changed = wrap != self.wrap[r] || self.scratch != self.text[r];
        if changed {
            std::mem::swap(&mut self.text[r], &mut self.scratch);
            self.wrap[r] = wrap;
        }
        changed
    }

    /// Decide again whether the line row `r` is part of holds the needle.
    fn judge(&mut self, r: usize) {
        let (mut first, mut last) = (r, r);
        while first > 0 && self.wrap[first - 1] {
            first -= 1;
        }
        while last + 1 < self.text.len() && self.wrap[last] {
            last += 1;
        }
        let hit = if first == last {
            self.text[r].contains(&self.needle)
        } else {
            self.scratch.clear();
            self.text[first..=last].iter().for_each(|t| self.scratch.push_str(t));
            self.scratch.contains(&self.needle)
        };
        self.hit[first..=last].iter_mut().for_each(|h| *h = hit);
    }

    /// Bring the memory up to date with the terminal. True if anything on
    /// screen changed. `lone` says the only thing fed since the last look
    /// was a single line feed.
    fn look(&mut self, term: &mut Term<Listener>, lone: bool) -> bool {
        use alacritty_terminal::term::TermDamage;
        let size = (term.columns(), term.screen_lines());
        let n = size.1;
        self.todo.clear();
        let mut all = match term.damage() {
            TermDamage::Full => true,
            TermDamage::Partial(lines) => {
                self.todo.extend(lines.map(|l| l.line).filter(|&l| l < n));
                false
            }
        };
        term.reset_damage();
        let g = term.grid();
        let home = |r: usize| g[Line(r as i32)][..].as_ptr() as usize;
        let mut changed = false;
        if size != self.size {
            self.size = size;
            self.text = vec![String::new(); n];
            (self.wrap, self.hit, self.at) = (vec![false; n], vec![false; n], vec![0; n]);
            (all, changed) = (true, true);
        } else if all && lone {
            // A line feed by itself writes nothing. At most it scrolled: some
            // rows moved up one, and the row the cursor is left on is new.
            // Rows are recognised by where their cells live.
            let cursor = g.cursor.point.line.0 as usize;
            let mut run = false;
            for r in 0..n {
                let h = home(r);
                let moved = r != cursor && h != self.at[r] && r + 1 < n && h == self.at[r + 1];
                if moved {
                    self.text[r] = std::mem::take(&mut self.text[r + 1]);
                    (self.wrap[r], self.hit[r]) = (self.wrap[r + 1], self.hit[r + 1]);
                    changed = true;
                }
                // Read again: the cursor's row, any row not recognised, and
                // the top row of what moved (the row above it is gone, so
                // the line it belonged to may have lost its beginning).
                if r == cursor || (moved && !run) || (!moved && h != self.at[r]) {
                    self.todo.push(r);
                }
                run = moved;
            }
            all = false;
            let todo = std::mem::take(&mut self.todo);
            for &r in &todo {
                changed |= self.read(g, r);
            }
            if changed {
                for &r in &todo {
                    // And the row below each: it may have just been cut loose.
                    self.judge(r);
                    if r + 1 < n {
                        self.judge(r + 1);
                    }
                }
            }
            self.todo = todo;
            self.todo.clear();
        }
        if all {
            for r in 0..n {
                changed |= self.read(g, r);
            }
            (0..n).for_each(|r| self.judge(r));
        } else if !self.todo.is_empty() {
            let todo = std::mem::take(&mut self.todo);
            for &r in &todo {
                // The row above is read too: overwriting a wide character at
                // the start of a row silently clears a mark in the last cell
                // of the row before, which the terminal does not report.
                for q in [r.wrapping_sub(1), r] {
                    if q < n && self.read(g, q) {
                        changed = true;
                        // The row below may have just been joined to this one or cut loose from it.
                        self.judge(q);
                        if q + 1 < n {
                            self.judge(q + 1);
                        }
                    }
                }
            }
            self.todo = todo;
        }
        for r in 0..n {
            self.at[r] = home(r);
        }
        changed
    }

    /// The first line holding the needle: its first row and its text.
    fn found(&self) -> Option<(usize, String)> {
        let r = self.hit.iter().position(|&h| h)?;
        let mut line = String::new();
        let mut last = r;
        while last + 1 < self.text.len() && self.wrap[last] {
            last += 1;
        }
        self.text[r..=last].iter().for_each(|t| line.push_str(t));
        line.truncate(line.trim_end_matches(' ').len());
        Some((r, line))
    }
}

#[cfg(test)]
mod tests;
