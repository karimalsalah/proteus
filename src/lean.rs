//! The lean reader: what a program printed, in the form an agent should read.
//!
//! Everything here is pure: bytes in, text out; no clock, no files, no way
//! to answer the program.
//!
//! Stage 1, `Ribbon`. The bytes go through the same terminal emulator the
//! window uses and the rows are read back as they finally stood. Colour and
//! title sequences vanish, a progress bar redrawn two hundred times is one
//! line, a prompt edited key by key is the command that was run, and a line
//! the terminal wrapped is one line again. This is exact where stripping
//! escape codes is not: the console host positions the cursor by row and
//! column (`ESC[4;68H`) even for a plain command line.
//!
//! Stage 2, `condense`. The rules, in order:
//!
//! 1. Trailing blanks are trimmed; a run of blank lines becomes one.
//! 2. Three or more identical lines in a row become the line and a note with
//!    the count.
//! 3. Four or more lines in a row that are identical once every run of digits
//!    is replaced by `#` become the first, a note with the count, and the
//!    last. Signal lines (rule 5) never take part.
//! 4. If more than `head + tail` entries remain, the first `head` and the
//!    last `tail` are kept, and of the rest only signal lines, each with the
//!    line after it when that line is indented further (a location or a
//!    stack frame). Each gap becomes one note giving the count and the line
//!    numbers left out.
//! 5. A signal line is an error or a warning. Error: the line contains one of
//!    `ERROR_WORDS`, ignoring case, not directly preceded by a letter; or
//!    `Error` / `Exception` inside a longer name (`ValueError`). Warning: the
//!    same test with `WARNING_WORDS` and `Warning`. A signal line is never
//!    left out and never merged with a merely similar one; identical ones are
//!    counted (rule 2). (What the program itself erased or wrote over before
//!    the read is not there to keep: stage 1 reads the screen as it ended.)
//! 6. A line longer than `max_line` characters keeps its first three quarters
//!    and last quarter of that budget around a note with the count. In a
//!    signal line the word that makes it one is always in what is kept: if
//!    it lies in between, the line keeps its first half, a quarter around
//!    the word, and its last quarter.
//!
//! `Strip` is the cheap streaming cousin, used to match a pattern while
//! output is still arriving.
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags, LineLength};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi;

/// What a program asks its terminal (colours, cursor position, identity) is
/// dropped here. A reader has no voice: only the window answers.
#[derive(Clone, Copy)]
pub struct Mute;

impl EventListener for Mute {
    fn send_event(&self, _: Event) {}
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
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

/// Bytes parsed between two harvests, and the rows the emulator keeps above
/// the screen until then. No more rows than `HISTORY` may scroll off between
/// two harvests, or the oldest are gone before they are read. A byte of
/// ordinary output scrolls one row at most. Two sequences scroll many: a
/// repeat (`ESC[65535b` writes 65,535 characters, a row each on the narrowest
/// screen with wide characters) and a scroll (`ESC[999S`, the height of the
/// screen); `feed` therefore also harvests after every byte that ends one of
/// them, and then 4,096 + 65,535 rows is the most there can be.
const STEP: usize = 4096;
const HISTORY: usize = 1 << 17;

struct Row {
    /// Position counted from the first row the session ever had.
    abs: u64,
    text: String,
    /// The terminal wrapped this row: the next one continues the same line.
    wrapped: bool,
}

fn fingerprint(text: &str, wrapped: bool) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    wrapped.hash(&mut h);
    h.finish()
}

/// One row of a grid as text, and whether the terminal wrapped it into the next.
pub fn row_text(row: &alacritty_terminal::grid::Row<Cell>) -> (String, bool) {
    let len = row.line_length().0;
    let mut s = String::with_capacity(len);
    for c in 0..len {
        let cell = &row[Column(c)];
        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        // A tab is stored as itself followed by blanks up to the tab stop.
        s.push(if cell.c == '\t' { ' ' } else { cell.c });
        if let Some(extra) = cell.zerowidth() {
            s.extend(extra.iter());
        }
    }
    let wrapped = len > 0 && len == row.len() && row[Column(len - 1)].flags.contains(Flags::WRAPLINE);
    if !wrapped {
        s.truncate(s.trim_end().len());
    }
    (s, wrapped)
}

/// How much has scrolled past since the last mark: enough to judge the size
/// of a read without rendering it.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Weight {
    pub rows: usize,
    pub bytes: usize,
    pub signal_bytes: usize,
}

/// A terminal read as a growing ribbon of rows instead of a window onto it.
pub struct Ribbon {
    term: Term<Mute>,
    parser: ansi::Processor<ansi::StdSyncHandler>,
    rows: usize,
    /// Rows that have scrolled off the top since the last mark, oldest first.
    done: VecDeque<Row>,
    /// Position of the row now at the top of the screen.
    top: u64,
    /// How each row on screen at the last mark looked then. Such a row is
    /// only reported again if it has changed.
    seen: HashMap<u64, u64>,
    weight: Weight,
    /// The last byte fed.
    last: u8,
}

impl Ribbon {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.clamp(2, 1000), rows.clamp(1, 1000));
        let cfg = Config { scrolling_history: HISTORY, ..Config::default() };
        Self {
            term: Term::new(cfg, &Size { cols, rows }, Mute),
            parser: ansi::Processor::new(),
            rows,
            done: VecDeque::new(),
            top: 0,
            seen: HashMap::new(),
            weight: Weight::default(),
            last: 0,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(STEP) {
            let mut from = 0;
            for (i, &byte) in chunk.iter().enumerate() {
                // The final byte of a repeat or a scroll with a count: `b` or
                // `S` after a digit, a separator, or a control character (one
                // of those may stand inside a sequence without ending it).
                let before = if i == 0 { self.last } else { chunk[i - 1] };
                if matches!(byte, b'b' | b'S') && (before.is_ascii_digit() || before < 0x20 || matches!(before, b';' | b':' | 0x7f)) {
                    self.parser.advance(&mut self.term, &chunk[from..=i]);
                    self.harvest();
                    from = i + 1;
                }
            }
            self.parser.advance(&mut self.term, &chunk[from..]);
            self.harvest();
            self.last = chunk[chunk.len() - 1];
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.clamp(2, 1000), rows.clamp(1, 1000));
        self.term.resize(Size { cols, rows });
        self.rows = rows;
        self.harvest();
    }

    /// A full-screen program (editor, pager, agent UI) owns the display: its
    /// output is a picture, not a log, and is read with `screen`.
    pub fn full_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn weight(&self) -> Weight {
        self.weight
    }

    /// Move rows that left the screen into `done`; they can no longer change.
    fn harvest(&mut self) {
        if self.full_screen() {
            return;
        }
        let h = self.term.grid().history_size();
        if h == 0 {
            return;
        }
        for i in (1..=h).rev() {
            let (text, wrapped) = row_text(&self.term.grid()[Line(-(i as i32))]);
            let abs = self.top + (h - i) as u64;
            if self.seen.remove(&abs) == Some(fingerprint(&text, wrapped)) {
                continue;
            }
            self.weight.rows += 1;
            self.weight.bytes += text.len() + 1;
            if signal(&text) != Signal::None {
                self.weight.signal_bytes += text.len() + 1;
            }
            self.done.push_back(Row { abs, text, wrapped });
        }
        self.top += h as u64;
        self.term.grid_mut().clear_history();
    }

    /// A synchronized update left open would hide everything after it.
    fn settle(&mut self) {
        if self.parser.sync_bytes_count() > 0 {
            self.parser.stop_sync(&mut self.term);
            self.harvest();
        }
    }

    /// Last screen row in use: the cursor's, or a later one with text.
    fn used(&self) -> usize {
        let grid = self.term.grid();
        let cursor = (grid.cursor.point.line.0.max(0) as usize).min(self.rows - 1);
        (cursor + 1..self.rows).rev().find(|&r| grid[Line(r as i32)].line_length().0 > 0).unwrap_or(cursor)
    }

    /// First screen row of the line the cursor is on (it may have wrapped).
    fn cursor_line(&self) -> usize {
        let grid = self.term.grid();
        let mut r = (grid.cursor.point.line.0.max(0) as usize).min(self.rows - 1);
        while r > 0 && row_text(&grid[Line(r as i32 - 1)]).1 {
            r -= 1;
        }
        r
    }

    /// How many screen rows a reading covers: all in use, or, when more
    /// output is still to come, only those above the line being written.
    fn reach(&self, whole: bool) -> usize {
        if whole {
            self.used() + 1
        } else {
            self.cursor_line()
        }
    }

    /// Nothing is half told: the line being written is empty, and so is every
    /// row below it. A read that stops here holds no line back, so a reader
    /// that starts at this very byte later (another server, or this one with
    /// a cursor) reports exactly what this one would have reported next.
    pub fn at_rest(&self) -> bool {
        if self.full_screen() {
            return true;
        }
        if self.parser.sync_bytes_count() > 0 {
            return false; // text is waiting inside an open synchronized update
        }
        let grid = self.term.grid();
        (self.cursor_line()..=self.used()).all(|r| grid[Line(r as i32)].line_length().0 == 0)
    }

    /// What `pending(whole)` reports has been seen: later calls report only
    /// rows that are new or have changed since now.
    pub fn mark(&mut self, whole: bool) {
        self.settle();
        self.done.clear();
        self.weight = Weight::default();
        if self.full_screen() {
            return; // the rows underneath cannot be read; keep what was seen
        }
        self.seen.clear();
        let writing = self.cursor_line();
        let rows: Vec<(String, bool)> = (0..self.reach(whole)).map(|r| row_text(&self.term.grid()[Line(r as i32)])).collect();
        // Blank rows at the end, from the line being written on, are rows
        // nothing has been written to: `pending` did not report them, so
        // they are not seen. (If the program leaves one blank, that is a
        // blank line it printed, and it is reported then.) It also makes a
        // mark taken at rest the same whether or not more was expected.
        let mut told = rows.len();
        while told > writing && rows[told - 1].0.is_empty() && !rows[told - 1].1 {
            told -= 1;
        }
        for (r, (text, wrapped)) in rows.iter().enumerate().take(told) {
            self.seen.insert(self.top + r as u64, fingerprint(text, *wrapped));
        }
    }

    /// The lines that are new or changed since the last mark, as they stand.
    /// With `whole` false the line the cursor is on is left out: it is
    /// reported, complete, by a later call.
    pub fn pending(&mut self, whole: bool) -> Vec<String> {
        self.settle();
        let mut out: Vec<String> = Vec::new();
        // (position, wrapped) of the row most recently added to `out`.
        let mut prev: Option<(u64, bool)> = None;
        fn add(out: &mut Vec<String>, prev: &mut Option<(u64, bool)>, abs: u64, text: &str, wrapped: bool) {
            match *prev {
                Some((p, true)) if p + 1 == abs => {
                    if let Some(last) = out.last_mut() {
                        last.push_str(text);
                    }
                }
                _ => out.push(text.to_string()),
            }
            *prev = Some((abs, wrapped));
        }
        for row in &self.done {
            add(&mut out, &mut prev, row.abs, &row.text, row.wrapped);
        }
        // Lines above the one being written are final, blank ones included.
        // From that line on, blank rows at the end are rows nothing has been
        // written to, and are left out.
        let mut written = out.len();
        if !self.full_screen() {
            let writing = self.cursor_line();
            for r in 0..self.reach(whole) {
                if r <= writing {
                    written = out.len();
                }
                let (text, wrapped) = row_text(&self.term.grid()[Line(r as i32)]);
                let abs = self.top + r as u64;
                if self.seen.get(&abs) == Some(&fingerprint(&text, wrapped)) {
                    continue;
                }
                add(&mut out, &mut prev, abs, &text, wrapped);
            }
            if self.reach(whole) <= writing {
                written = out.len();
            }
        }
        while out.len() > written && out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        out
    }
}

/// The final lines of a whole capture, read at a given terminal size.
#[cfg(test)]
pub fn lines(raw: &[u8], cols: usize, rows: usize) -> Vec<String> {
    let mut r = Ribbon::new(cols, rows);
    r.feed(raw);
    r.pending(true)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    None,
    Warning,
    Error,
}

pub const ERROR_WORDS: &[&str] = &[
    "error", "err!", "fatal", "panic", "fail", "exception", "traceback", "denied", "abort", "rejected", "conflict",
    "invalid", "unexpected", "missing", "not found", "not recognized", "no such", "cannot", "could not", "unable to",
    "segmentation fault", "timed out", "refused", "unresolved", "undefined",
];
pub const WARNING_WORDS: &[&str] = &["warn", "deprecat"];

/// Where `word` occurs in `line` at a word start, or (when `compound`)
/// capitalised inside a longer name. `lower` is `line` in ASCII lower case.
fn find_word(line: &str, lower: &str, word: &str, compound: bool) -> Option<usize> {
    let (lb, ob) = (lower.as_bytes(), line.as_bytes());
    let mut from = 0;
    while let Some(i) = lower[from..].find(word) {
        let at = from + i;
        if at == 0 || !lb[at - 1].is_ascii_alphabetic() || (compound && ob[at].is_ascii_uppercase()) {
            return Some(at);
        }
        from = at + 1;
        while !lower.is_char_boundary(from) {
            from += 1;
        }
    }
    None
}

fn has_word(line: &str, lower: &str, word: &str, compound: bool) -> bool {
    find_word(line, lower, word, compound).is_some()
}

/// Where the first word that makes `line` an error or a warning starts.
fn signal_at(line: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    let errors = ERROR_WORDS.iter().filter_map(|w| find_word(line, &lower, w, matches!(*w, "error" | "exception")));
    let warnings = WARNING_WORDS.iter().filter_map(|w| find_word(line, &lower, w, *w == "warn"));
    errors.chain(warnings).min()
}

/// Does this line look like an error or a warning? (Rule 5.)
pub fn signal(line: &str) -> Signal {
    let lower = line.to_ascii_lowercase();
    if ERROR_WORDS.iter().any(|w| has_word(line, &lower, w, matches!(*w, "error" | "exception"))) {
        return Signal::Error;
    }
    if WARNING_WORDS.iter().any(|w| has_word(line, &lower, w, *w == "warn")) {
        return Signal::Warning;
    }
    Signal::None
}

#[derive(Clone, Copy, Debug)]
pub struct Rules {
    pub head: usize,
    pub tail: usize,
    pub max_line: usize,
}

impl Default for Rules {
    fn default() -> Self {
        Self { head: 20, tail: 40, max_line: 1000 }
    }
}

#[derive(Default, Debug)]
pub struct Lean {
    pub text: String,
    pub lines_in: usize,
    pub lines_out: usize,
    /// Lines of the input that look like errors / warnings (all are in `text`).
    pub errors: usize,
    pub warnings: usize,
    /// Input lines not shown one by one: left out, or folded into a count.
    pub folded: usize,
}

struct Entry<'a> {
    /// Input line numbers this entry stands for, from 1.
    first: usize,
    last: usize,
    text: std::borrow::Cow<'a, str>,
    sig: Signal,
    note: bool,
}

/// Every run of digits becomes one `#`.
fn mask(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_digits = false;
    for c in line.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

fn indent(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

fn clip(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    let n = text.chars().count();
    if max < 8 || n <= max {
        return text.into();
    }
    let (a, b) = (max * 3 / 4, max / 4);
    let head: String = text.chars().take(a).collect();
    let tail: String = text.chars().skip(n - b).collect();
    format!("{head} [... {} chars ...] {tail}", n - a - b).into()
}

/// Rule 6 for a signal line: the word that makes it one is never in the part
/// left out. If it lies between the head and the tail kept, a stretch around
/// it is kept as well.
fn clip_signal(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    let n = text.chars().count();
    if max < 8 || n <= max {
        return text.into();
    }
    let Some(at) = signal_at(text).map(|b| text[..b].chars().count()) else { return clip(text, max) };
    let (head, around, tail) = (max / 2, max / 4, max / 4);
    if at + around <= max * 3 / 4 || at >= n - tail {
        return clip(text, max); // already in what the plain cut keeps
    }
    let from = at.saturating_sub(around / 4).max(head);
    let to = (from + around).min(n - tail);
    let part = |a: usize, b: usize| text.chars().skip(a).take(b - a).collect::<String>();
    let gap = |a: usize, b: usize| if b > a { format!(" [... {} chars ...] ", b - a) } else { String::new() };
    format!("{}{}{}{}{}", part(0, head), gap(head, from), part(from, to), gap(to, n - tail), part(n - tail, n)).into()
}

/// Apply rules 1 to 6 to lines already in their final state.
pub fn condense(lines: &[String], rules: &Rules) -> Lean {
    let n = lines.len();
    let sigs: Vec<Signal> = lines.iter().map(|l| signal(l)).collect();
    let mut entries: Vec<Entry> = Vec::new();
    let line = |i: usize| Entry { first: i + 1, last: i + 1, text: lines[i].trim_end().into(), sig: sigs[i], note: false };
    let note = |first: usize, last: usize, text: String| Entry { first, last, text: text.into(), sig: Signal::None, note: true };
    let blank = |i: usize| lines[i].trim().is_empty();

    let mut i = 0;
    while i < n {
        if blank(i) {
            let mut j = i;
            while j < n && blank(j) {
                j += 1;
            }
            entries.push(Entry { first: i + 1, last: j, text: "".into(), sig: Signal::None, note: false });
            i = j;
            continue;
        }
        let mut j = i + 1;
        while j < n && lines[j] == lines[i] {
            j += 1;
        }
        if j - i >= 3 {
            entries.push(line(i));
            entries.push(note(i + 2, j, format!("[... {} ...]", plural(j - i - 1, "more identical line"))));
            i = j;
            continue;
        }
        if sigs[i] == Signal::None {
            let m = mask(&lines[i]);
            if m.contains('#') {
                let mut k = i + 1;
                while k < n && sigs[k] == Signal::None && mask(&lines[k]) == m {
                    k += 1;
                }
                if k - i >= 4 {
                    entries.push(line(i));
                    entries.push(note(i + 2, k - 1, format!("[... {} ...]", plural(k - i - 2, "similar line"))));
                    entries.push(line(k - 1));
                    i = k;
                    continue;
                }
            }
        }
        entries.push(line(i));
        i += 1;
    }

    let count = entries.len();
    let mut keep = vec![true; count];
    if count > rules.head + rules.tail {
        for (idx, e) in entries.iter().enumerate().take(count - rules.tail).skip(rules.head) {
            keep[idx] = e.sig != Signal::None;
        }
        for idx in rules.head..count - rules.tail {
            let follows = idx > 0 && keep[idx - 1] && entries[idx - 1].sig != Signal::None && !entries[idx - 1].note;
            let e = &entries[idx];
            if follows && !keep[idx] && (e.note || (!e.text.is_empty() && indent(&e.text) > indent(&entries[idx - 1].text))) {
                keep[idx] = true;
            }
        }
    }

    let mut out = Lean {
        lines_in: n,
        errors: sigs.iter().filter(|s| **s == Signal::Error).count(),
        warnings: sigs.iter().filter(|s| **s == Signal::Warning).count(),
        ..Lean::default()
    };
    let mut shown = 0usize;
    let push = |out: &mut Lean, text: &str| {
        if out.lines_out > 0 {
            out.text.push('\n');
        }
        out.text.push_str(text);
        out.lines_out += 1;
    };
    let mut idx = 0;
    while idx < count {
        if keep[idx] {
            let e = &entries[idx];
            if !e.note {
                shown += 1;
            }
            push(&mut out, &if e.sig == Signal::None { clip(&e.text, rules.max_line) } else { clip_signal(&e.text, rules.max_line) });
            idx += 1;
            continue;
        }
        let start = idx;
        while idx < count && !keep[idx] {
            idx += 1;
        }
        let (a, b) = (entries[start].first, entries[idx - 1].last);
        push(&mut out, &format!("[... {} omitted ({a}-{b}) ...]", plural(b - a + 1, "line")));
    }
    out.folded = n.saturating_sub(shown);
    out
}

/// Escape sequences removed from a stream as it arrives. Unlike `Ribbon` it
/// keeps no screen: a carriage return or a cursor jump simply starts a new
/// line, so every intermediate state of a progress bar is still there to be
/// matched. Split anywhere: an unfinished sequence or character is held
/// until the rest arrives.
#[derive(Default)]
pub struct Strip {
    state: State,
    /// Text bytes not yet known to be whole characters.
    text: Vec<u8>,
    param: u32,
    cr: bool,
    ends_in_newline: bool,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Text,
    Esc,
    EscInter,
    Csi,
    Osc,
    OscEsc,
    Str,
    StrEsc,
}

impl Strip {
    fn flush(&mut self, out: &mut String, all: bool) {
        let mut rest = &self.text[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(good).unwrap_or_default());
                    match e.error_len() {
                        Some(len) => {
                            out.push('\u{fffd}');
                            rest = &bad[len..];
                        }
                        None if all => {
                            out.push('\u{fffd}');
                            rest = &[];
                            break;
                        }
                        None => {
                            rest = bad; // wait for the rest of this character
                            break;
                        }
                    }
                }
            }
        }
        self.text = rest.to_vec();
    }

    fn newline(&mut self, out: &mut String) {
        self.flush(out, true);
        out.push('\n');
        self.ends_in_newline = true;
    }

    pub fn push(&mut self, bytes: &[u8], out: &mut String) {
        for &b in bytes {
            if self.state == State::Text {
                if self.cr {
                    self.cr = false;
                    self.newline(out);
                    if b == b'\n' {
                        continue;
                    }
                }
                match b {
                    0x1b => self.state = State::Esc,
                    b'\r' => self.cr = true,
                    b'\n' => self.newline(out),
                    b'\t' | 0x20..=0x7e | 0x80..=0xff => {
                        self.text.push(b);
                        self.ends_in_newline = false;
                    }
                    _ => {}
                }
                continue;
            }
            self.state = match (self.state, b) {
                (State::Esc, b'[') => {
                    self.param = 0;
                    State::Csi
                }
                (State::Esc, b']') => State::Osc,
                (State::Esc, b'P' | b'X' | b'^' | b'_') => State::Str,
                (State::Esc | State::EscInter, 0x20..=0x2f) => State::EscInter,
                (State::Esc | State::EscInter, _) => State::Text,
                (State::Csi, 0x1b) => State::Esc,
                (State::Csi, b'0'..=b'9') => {
                    self.param = self.param.saturating_mul(10).saturating_add((b - b'0') as u32);
                    State::Csi
                }
                (State::Csi, 0x40..=0x7e) => {
                    match b {
                        // Cursor forward: the console host writes gaps this way.
                        b'C' => {
                            self.flush(out, true);
                            for _ in 0..self.param.clamp(1, 200) {
                                out.push(' ');
                            }
                            self.ends_in_newline = false;
                        }
                        // A jump to another row.
                        b'H' | b'f' | b'd' | b'E' | b'F' | b'A' | b'B' if !self.ends_in_newline => self.newline(out),
                        _ => {}
                    }
                    State::Text
                }
                (State::Csi, b';' | b':') => {
                    self.param = 0;
                    State::Csi
                }
                (State::Csi, _) => State::Csi,
                (State::Osc, 0x07) => State::Text,
                (State::Osc, 0x1b) => State::OscEsc,
                (State::Osc, _) => State::Osc,
                (State::OscEsc, b'\\') => State::Text,
                (State::OscEsc, b'[') => {
                    self.param = 0;
                    State::Csi
                }
                (State::OscEsc, _) => State::Text,
                (State::Str, 0x1b) => State::StrEsc,
                (State::Str, _) => State::Str,
                (State::StrEsc, b'\\') => State::Text,
                (State::StrEsc, _) => State::Str,
                (State::Text, _) => State::Text,
            };
        }
        self.flush(out, false);
    }
}

/// A whole capture with its escape sequences removed.
#[cfg(test)]
pub fn strip(raw: &[u8]) -> String {
    let mut out = String::new();
    Strip::default().push(raw, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real output, captured on this machine through the bundled console host
    // (see forge/lanes/organs.json for the commands).
    const CARGO: &[u8] = include_bytes!("../forge/samples/organs/cargo-build.vt");
    const GIT: &[u8] = include_bytes!("../forge/samples/organs/git-log-small.vt");
    const PROGRESS: &[u8] = include_bytes!("../forge/samples/organs/progress.vt");
    const WRITE_PROGRESS: &[u8] = include_bytes!("../forge/samples/organs/write-progress.vt");
    const CMD: &[u8] = include_bytes!("../forge/samples/organs/cmd-session.vt");
    const PWSH: &[u8] = include_bytes!("../forge/samples/organs/pwsh-session.vt");

    fn all(raw: &[u8]) -> Vec<String> {
        lines(raw, 120, 30)
    }

    #[test]
    fn a_progress_bar_is_its_last_state() {
        let l = all(PROGRESS);
        assert_eq!(l.len(), 2, "{l:#?}");
        assert_eq!(l[0], format!("Downloading crate 200 of 200 [{}] 100%", "#".repeat(40)));
        assert_eq!(l[1], "done: 200 crates");
        // 200 redraws in the raw stream, one line out.
        assert_eq!(strip(PROGRESS).matches("Downloading crate").count(), 200);
        let lean = condense(&l, &Rules::default());
        assert!(lean.text.len() * 100 < PROGRESS.len(), "{} of {}", lean.text.len(), PROGRESS.len());
    }

    /// PowerShell draws its progress bar at the top of the screen by row and
    /// column and wipes it with blanks: only an emulator reads that right.
    #[test]
    fn a_bar_drawn_by_cursor_position_leaves_nothing_behind() {
        assert_eq!(all(WRITE_PROGRESS), vec!["copied 50 files"]);
        assert!(strip(WRITE_PROGRESS).contains("Copying ["));
    }

    #[test]
    fn a_shell_session_reads_as_commands_and_output() {
        let l = all(CMD);
        let at = |needle: &str| l.iter().position(|x| x.ends_with(needle)).unwrap_or_else(|| panic!("no line ends with {needle:?} in {l:#?}"));
        let echo = at(">echo hello & dir /b");
        assert_eq!(l[echo + 1], "hello");
        assert_eq!(l[echo + 2], "Cargo.lock");
        let ty = at(">type nosuchfile.txt");
        assert_eq!(l[ty + 1], "The system cannot find the file specified.");
        assert_eq!(signal(&l[ty + 1]), Signal::Error);
        assert!(l.last().unwrap().ends_with("leanfix>"), "{l:#?}");
        assert!(l.iter().all(|x| !x.contains('\u{1b}') && !x.contains("cmd.exe - ")), "titles and escapes are gone");
    }

    /// PSReadLine redraws the command line on every key and shows a guess
    /// after the cursor; the final row is what was run.
    #[test]
    fn a_line_edited_key_by_key_is_the_command_that_ran() {
        let l = all(PWSH);
        let ran: Vec<&String> = l.iter().filter(|x| x.contains("Get-ChildItem")).collect();
        assert_eq!(ran.len(), 1, "{l:#?}");
        assert!(ran[0].ends_with("> Get-ChildItem | Select-Object -First 3"), "{}", ran[0]);
        assert!(l.iter().any(|x| x.ends_with("> Get-Item nosuchfile")));
        assert!(l.iter().any(|x| x == "row 5"));
        // The raw stream holds every intermediate redraw.
        assert!(strip(PWSH).matches("Get-ChildItem").count() > 5);
    }

    fn assert_no_signal_lost(raw: &[u8], rules: &Rules) -> Lean {
        let l = all(raw);
        let lean = condense(&l, rules);
        let kept: std::collections::HashSet<&str> = lean.text.lines().map(str::trim).collect();
        // Judged from the emulator's lines...
        for line in l.iter().filter(|x| signal(x) != Signal::None) {
            assert!(kept.contains(line.trim()), "lost: {line}");
        }
        // ...and independently from the stripped raw stream.
        let stripped = strip(raw);
        let mut n = 0;
        for line in stripped.lines().map(str::trim).filter(|x| signal(x) != Signal::None) {
            assert!(kept.contains(line), "lost (raw): {line}");
            n += 1;
        }
        assert_eq!(lean.errors + lean.warnings, n, "both readings agree on how many signal lines there are");
        lean
    }

    #[test]
    fn a_long_build_keeps_head_tail_and_every_error_and_warning() {
        let l = all(CARGO);
        assert!(l.len() > 150, "{} lines", l.len());
        assert!(l.iter().all(|x| !x.contains("Building [")), "the build's progress line is gone");
        let lean = assert_no_signal_lost(CARGO, &Rules::default());
        assert_eq!(lean.errors, 3, "error[E0308], the --explain hint and cargo's summary");
        assert!(lean.warnings >= 26, "{}", lean.warnings);
        assert!(lean.text.contains("error[E0308]: mismatched types"));
        // The location under each kept diagnostic survives with it.
        let out: Vec<&str> = lean.text.lines().collect();
        let e = out.iter().position(|x| x.starts_with("error[E0308]")).unwrap();
        assert!(out[e + 1].trim_start().starts_with("--> src\\main.rs:"), "{}", out[e + 1]);
        assert!(lean.text.contains(" omitted ("));
        assert!(lean.lines_out < lean.lines_in);
        println!("cargo: {} raw bytes, {} lines -> {} bytes, {} lines; {} errors, {} warnings", CARGO.len(), lean.lines_in, lean.text.len(), lean.lines_out, lean.errors, lean.warnings);
        assert!(lean.text.len() < CARGO.len(), "{} of {}", lean.text.len(), CARGO.len());
        // With room for everything, nothing is left out.
        let whole = condense(&l, &Rules { head: 10_000, tail: 0, max_line: 1000 });
        assert!(!whole.text.contains("omitted"));
    }

    #[test]
    fn a_diff_loses_only_its_colour() {
        let l = all(GIT);
        let lean = assert_no_signal_lost(GIT, &Rules { head: 1000, tail: 0, max_line: 1000 });
        // Only a doubled blank line is folded; every line with text is there, in order.
        let text: Vec<&str> = l.iter().map(String::as_str).filter(|x| !x.trim().is_empty()).collect();
        assert_eq!(lean.text.lines().filter(|x| !x.trim().is_empty()).collect::<Vec<_>>(), text);
        assert_eq!(lean.lines_in - lean.lines_out, 1);
        assert!(l.iter().any(|x| x == "+        _ => crate::paths::data_dir().join(\"proteusd-2.sock\"),"));
        assert!(lean.text.len() < GIT.len());
    }

    #[test]
    fn reading_in_two_parts_shows_each_line_once_unless_it_changed() {
        // Cut the cmd session just after its first prompt has been printed.
        let cut = CMD.windows(8).position(|w| w == b"leanfix>").unwrap() + 8;
        let mut r = Ribbon::new(120, 30);
        r.feed(&CMD[..cut]);
        let first = r.pending(true);
        assert!(first.last().unwrap().ends_with("leanfix>"));
        r.mark(true);
        assert!(r.pending(true).is_empty(), "nothing new: nothing reported");
        r.feed(&CMD[cut..]);
        let second = r.pending(true);
        // The prompt row changed (the command was typed on it), so it is shown again, whole.
        assert!(second[0].ends_with("leanfix>echo hello & dir /b"), "{second:#?}");
        assert!(!second.iter().any(|x| x.starts_with("(c) Microsoft")), "the banner is not shown twice");
        assert_eq!(first.len() + second.len(), all(CMD).len() + 1);
    }

    #[test]
    fn rows_that_scroll_away_are_not_lost_and_wrapped_rows_rejoin() {
        let mut raw = Vec::new();
        for i in 0..5000 {
            raw.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        raw.extend_from_slice("x".repeat(250).as_bytes());
        let mut r = Ribbon::new(100, 10);
        // Fed in ragged pieces, as a socket would deliver it.
        for chunk in raw.chunks(777) {
            r.feed(chunk);
        }
        // While more is expected, the line still being written is held back...
        let above = r.pending(false);
        assert_eq!((above.len(), above.last().map(String::as_str)), (5000, Some("line 4999")));
        // ...and nothing is lost or repeated by reading in two goes.
        let mut two = Ribbon::new(100, 10);
        two.feed(&raw[..30_000]);
        let mut both = two.pending(false);
        two.mark(false);
        two.feed(&raw[30_000..]);
        both.extend(two.pending(true));
        let l = r.pending(true);
        assert_eq!(both, l);
        assert_eq!(l.len(), 5001);
        assert_eq!(l[0], "line 0");
        assert_eq!(l[4999], "line 4999");
        assert_eq!(l[5000], "x".repeat(250), "a line wrapped over three rows is one line");
        assert_eq!(r.weight().rows, 5000 + 3 - 10, "everything above the screen has been weighed");
        let lean = condense(&l, &Rules::default());
        assert_eq!(lean.lines_out, 4, "first, a count, last, and the long line: {}", lean.text);
        assert!(lean.text.contains("[... 4998 similar lines ...]"));
    }

    /// A few bytes can scroll more rows away than the emulator keeps: a
    /// character repeated 65,535 times, five hundred times over. What was
    /// printed before is still reported.
    #[test]
    fn rows_scrolled_away_by_a_repeat_or_a_scroll_are_not_lost() {
        let mut raw = b"start of the run\r\nerror: printed before the flood\r\n".to_vec();
        for i in 0..500 {
            // 32.8 million characters from 4,700 bytes, in the three ways the count can be followed.
            raw.extend_from_slice([&b"a\x1b[65535b"[..], b"a\x1b[65535;b", b"a\x1b[65535\x00b"][i % 3]);
        }
        raw.extend_from_slice(b"\r\nend of the run\r\n");
        let mut r = Ribbon::new(120, 30);
        r.feed(&raw);
        let l = r.pending(true);
        let starts: Vec<String> = l.iter().map(|x| x.chars().take(40).collect()).collect();
        assert_eq!((l.len(), l[0].as_str(), l[1].as_str(), l[3].as_str()), (4, "start of the run", "error: printed before the flood", "end of the run"), "{starts:?}");
        assert_eq!(l[2].len(), 500 * 65_536, "the repeated character is one line, every one of it");
        let lean = condense(&l, &Rules::default());
        assert!(lean.errors == 1 && lean.text.len() < 1200 && lean.text.contains("error: printed before the flood"), "{}", lean.text.len());

        // The same with the screen scrolled by its whole height, over and over.
        let mut raw = b"warning: printed before the scrolling\r\n".to_vec();
        for i in 0..600 {
            raw.extend_from_slice(format!("line {i}\x1b[200S").as_bytes());
        }
        let mut r = Ribbon::new(80, 200);
        r.feed(&raw);
        let l = r.pending(true);
        assert_eq!(l[0], "warning: printed before the scrolling");
        assert_eq!(l.iter().filter(|x| x.contains("line ")).count(), 600, "{} lines", l.len());
    }

    #[test]
    fn rules_by_example() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let r = Rules::default();
        // 1. blank runs.
        assert_eq!(condense(&v(&["a", "", "", "", "b"]), &r).text, "a\n\nb");
        // 2. identical lines are counted, even errors.
        assert_eq!(condense(&v(&["x", "x", "x", "x"]), &r).text, "x\n[... 3 more identical lines ...]");
        assert_eq!(condense(&v(&["x", "x"]), &r).text, "x\nx");
        let e = condense(&v(&["error: boom", "error: boom", "error: boom"]), &r);
        assert_eq!((e.text.as_str(), e.errors), ("error: boom\n[... 2 more identical lines ...]", 3));
        // 3. lines that differ only in numbers; never signal lines.
        assert_eq!(condense(&v(&["tick 1", "tick 2", "tick 3", "tick 4"]), &r).text, "tick 1\n[... 2 similar lines ...]\ntick 4");
        assert_eq!(condense(&v(&["tick 1", "tick 2", "tick 3"]), &r).text, "tick 1\ntick 2\ntick 3");
        let f = v(&["test 1 failed", "test 2 failed", "test 3 failed", "test 4 failed"]);
        assert_eq!(condense(&f, &r).text, f.join("\n"));
        assert_eq!(condense(&v(&["a.rs", "b.rs", "c.rs", "d.rs"]), &r).text, "a.rs\nb.rs\nc.rs\nd.rs", "names are not numbers");
        // 4. head, tail and signal lines with their indented follower.
        let mut long: Vec<String> = (0..40).map(|i| format!("step {}", ["alpha", "beta", "gamma"][i % 3].repeat(1 + i % 5))).collect();
        long[10] = "error[E1]: bad".into();
        long[11] = "  --> src/x.rs:1:1".into();
        long[20] = "warning: hmm".into();
        let c = condense(&long, &Rules { head: 3, tail: 2, max_line: 1000 });
        let want = [&long[0][..], &long[1], &long[2], "[... 7 lines omitted (4-10) ...]", "error[E1]: bad", "  --> src/x.rs:1:1",
            "[... 8 lines omitted (13-20) ...]", "warning: hmm", "[... 17 lines omitted (22-38) ...]", &long[38], &long[39]];
        assert_eq!(c.text.lines().collect::<Vec<_>>(), want);
        assert_eq!((c.lines_in, c.lines_out, c.errors, c.warnings, c.folded), (40, 11, 1, 1, 32));
        // 6. very long lines.
        let clipped = condense(&v(&[&"z".repeat(5000)]), &Rules { head: 20, tail: 40, max_line: 100 });
        assert_eq!(clipped.text, format!("{} [... 4900 chars ...] {}", "z".repeat(75), "z".repeat(25)));
        // ...and in a long error line, the error itself is never in the part left out.
        let long = format!("{{\"log\":\"{}\",\"msg\":\"error: disk full on /dev/sda1\",\"more\":\"{}\"}}", "y".repeat(2000), "z".repeat(3000));
        let kept = condense(&v(&[&long]), &r);
        assert_eq!((kept.errors, kept.text.chars().count() < 1100), (1, true), "{}", kept.text.len());
        assert!(kept.text.contains("\"msg\":\"error: disk full on /dev/sda1\""), "the word that made it an error line was cut out: {}", kept.text);
        assert!(kept.text.starts_with("{\"log\":\"yyy") && kept.text.ends_with("zzz\"}") && kept.text.matches(" chars ...] ").count() == 2);
        // Near the start or the end it is in what the plain cut keeps anyway.
        for line in [format!("error: {}", "q".repeat(5000)), format!("{} failed", "q".repeat(5000)), format!("{} warning: w {}", "q".repeat(100), "q".repeat(4000))] {
            let c = condense(&v(&[&line]), &r).text;
            assert!(c.matches(" chars ...] ").count() == 1 && signal(&c) != Signal::None, "{c}");
        }
        let middle = format!("{} DeprecationWarning: old {}", "q".repeat(800), "q".repeat(4000));
        assert!(condense(&v(&[&middle]), &r).text.contains("DeprecationWarning: old"));
    }

    #[test]
    fn what_counts_as_a_signal() {
        for e in ["error: x", "error[E0308]: mismatched types", "src/a.c:3:1: error: expected ';'", "ValueError: bad", "java.io.IOException: closed",
            "FAILED tests/test_a.py::test_x", "test result: FAILED. 1 passed; 1 failed", "fatal: not a git repository", "thread 'main' panicked at src/main.rs",
            "npm ERR! code ELIFECYCLE", "bash: foo: command not found", "'foo' is not recognized as an internal or external command",
            "Access is denied.", "ls: cannot access 'x': No such file or directory", "Traceback (most recent call last):", "Segmentation fault (core dumped)"] {
            assert_eq!(signal(e), Signal::Error, "{e}");
        }
        for w in ["warning: unused variable", "WARN  slow query", "DeprecationWarning: use y", "(node:1) [DEP0040] deprecated"] {
            assert_eq!(signal(w), Signal::Warning, "{w}");
        }
        for n in ["   Compiling leanfix v0.1.0", "terrorist", "stderr is a tty", "forewarned", "reward: 5", "test a::b ... ok", "prefail"] {
            assert_eq!(signal(n), Signal::None, "{n}");
        }
    }

    #[test]
    fn strip_survives_any_split() {
        for raw in [CARGO, GIT, PROGRESS, WRITE_PROGRESS, CMD, PWSH, "h\u{e9}llo \u{4e16}\u{754c} \u{1f600}\r\n".as_bytes()] {
            let whole = strip(raw);
            assert!(!whole.contains('\u{1b}') && !whole.contains('\r') && !whole.contains('\u{fffd}'));
            for size in [1, 2, 3, 7, 64] {
                let (mut s, mut out) = (Strip::default(), String::new());
                for chunk in raw.chunks(size) {
                    s.push(chunk, &mut out);
                }
                assert_eq!(out, whole, "split every {size} bytes");
            }
        }
        assert!(strip(CARGO).contains("error[E0308]: mismatched types\n"));
        assert!(strip(CMD).contains("\necho hello & dir /b\n"), "a cursor jump starts a new line: {:?}", strip(CMD));
        assert_eq!(strip(b"a\x1b[3Cb\x1b]0;title\x07c\x1b[1;31md\x1b[0m\r\ne\rf"), "a   bcd\ne\nf");
    }
}
