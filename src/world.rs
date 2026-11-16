//! The World view: a session's real work drawn as one scene. This first
//! version shows the processes the session started (`procs.rs`) and how much
//! it printed, on one time axis.
//!
//! Everything in this file is plain data in, plain data out. `build` turns a
//! process ledger, the output record and a view (zoom, hover) into rectangles
//! and text. It reads no clock and no GPU, so the same input always gives the
//! same picture, and nothing in the picture can move unless an input changed:
//! time on the axis ends at the latest event, not at "now".
//!
//! `Live` is the little state a window keeps between events (what is shown,
//! the zoom, what the mouse is over, which nodes have just arrived). The GPU
//! side is `renderer/scene.rs`; the headless render is `world_shot.rs`.
use std::collections::HashMap;
use std::io::{self, BufReader, Read};
use std::path::Path;

use serde_json::Value;

use crate::procs::{self, Ledger, Proc, ProcEvent};
use crate::theme::{mix, Theme, C3};

// ------------------------------------------------------------------ type

/// The three text sizes the view uses. The numbers index `fonts::SIZES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Body = 0,
    Small = 1,
    Large = 2,
}

/// What layout needs to know about type: the cell (advance, line height) of
/// each size in pixels, and how many pixels one logical pixel is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Type {
    pub cell: [[f32; 2]; 3],
    pub unit: f32,
}

impl Type {
    fn w(&self, s: Size) -> f32 {
        self.cell[s as usize][0]
    }
    fn h(&self, s: Size) -> f32 {
        self.cell[s as usize][1]
    }
}

// ----------------------------------------------------------------- scene

/// A rectangle. `ring` > 0 draws only an outline that thick.
#[derive(Clone, Debug, PartialEq)]
pub struct Quad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: C3,
    pub alpha: f32,
    pub radius: f32,
    pub ring: f32,
}

/// One run of text; `x`, `y` is the top-left of its first cell.
#[derive(Clone, Debug, PartialEq)]
pub struct Text {
    pub x: f32,
    pub y: f32,
    pub s: String,
    pub color: C3,
    pub size: Size,
    pub bold: bool,
    /// When this text arrived (the window's frame clock, seconds), if it has
    /// only just arrived: it is revealed and casts light like arriving output.
    pub born: Option<f64>,
    /// Drawn on the hover card, above everything else.
    pub top: bool,
}

/// Light cast by an event (a process starting, or failing), from a rectangle.
#[derive(Clone, Debug, PartialEq)]
pub struct Emit {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: C3,
    pub born: f64,
    pub energy: f32,
}

/// What the mouse can be over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A process drawn as a node of the tree (its ledger id).
    Node(u64),
    /// A counted group of siblings: (ledger id of its first member, class).
    Group(u64, u8),
    /// A process's span on the timeline.
    Span(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub rect: [f32; 4],
    pub target: Target,
}

/// What the scene shows, in numbers (printed by `shot`, checked by tests).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub procs: usize,
    pub running: usize,
    pub failed: usize,
    /// Processes drawn as their own node, and counted groups.
    pub nodes: usize,
    pub groups: usize,
    /// Processes that are inside a group or folded into an ancestor.
    pub folded: usize,
    pub rows: usize,
    /// Rows the window had no room for (stated on screen when not zero).
    pub rows_cut: usize,
    /// How far detail was reduced to fit (0 = everything drawn expanded).
    pub level: usize,
    pub spans: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    /// Drawn under the text, in order.
    pub quads: Vec<Quad>,
    /// Drawn over the text (outlines only).
    pub over: Vec<Quad>,
    pub texts: Vec<Text>,
    pub emits: Vec<Emit>,
    pub hits: Vec<Hit>,
    /// The hover card: text beneath it that is not `top` is not drawn.
    pub clip: Option<[f32; 4]>,
    pub axis: Option<Axis>,
    /// The latest arrival in the scene, if any is still shown as arriving.
    pub last_birth: Option<f64>,
    pub counts: Counts,
}

impl Scene {
    fn quad(&mut self, x: f32, y: f32, w: f32, h: f32, color: C3, alpha: f32, radius: f32) {
        if w > 0.0 && h > 0.0 {
            self.quads.push(Quad { x, y, w, h, color, alpha, radius, ring: 0.0 });
        }
    }

    fn ring(&mut self, x: f32, y: f32, w: f32, h: f32, color: C3, alpha: f32, radius: f32, ring: f32) {
        self.quads.push(Quad { x, y, w, h, color, alpha, radius, ring });
    }

    /// Returns the x just past the text.
    fn text(&mut self, ty: &Type, x: f32, y: f32, s: &str, color: C3, size: Size, bold: bool) -> f32 {
        let n = s.chars().count();
        if n > 0 {
            self.texts.push(Text { x: x.round(), y: y.round(), s: s.to_string(), color, size, bold, born: None, top: false });
        }
        x.round() + n as f32 * ty.w(size)
    }

    /// The target under a point: the last one drawn wins (failures are drawn last).
    pub fn target_at(&self, x: f32, y: f32) -> Option<Target> {
        self.hits.iter().rev().find(|h| x >= h.rect[0] && x < h.rect[0] + h.rect[2] && y >= h.rect[1] && y < h.rect[1] + h.rect[3]).map(|h| h.target)
    }
}

// -------------------------------------------------------------- activity

/// How much the session printed, and when: bytes per slice of time, taken
/// from the journal's own record times (or the moment a chunk arrived, live).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Activity {
    /// Width of a slice in microseconds (doubles if a session grows very long).
    bin_us: u64,
    /// (slice number, bytes), ascending; empty slices are not stored.
    bins: Vec<(u64, u64)>,
    pub total: u64,
    pub first_us: Option<u64>,
    pub last_us: Option<u64>,
}

const BIN_US: u64 = 20_000;
const MAX_BINS: usize = 1 << 20;

impl Activity {
    pub fn add(&mut self, t_us: u64, bytes: u64) {
        if bytes == 0 {
            return;
        }
        if self.bin_us == 0 {
            self.bin_us = BIN_US;
        }
        self.total += bytes;
        self.first_us = Some(self.first_us.map_or(t_us, |f| f.min(t_us)));
        self.last_us = Some(self.last_us.map_or(t_us, |l| l.max(t_us)));
        let i = t_us / self.bin_us;
        match self.bins.last_mut() {
            Some(l) if l.0 == i => l.1 += bytes,
            // Out of order: history read from a journal after live output began.
            Some(l) if l.0 > i => match self.bins.binary_search_by_key(&i, |b| b.0) {
                Ok(k) => self.bins[k].1 += bytes,
                Err(k) => self.bins.insert(k, (i, bytes)),
            },
            _ => self.bins.push((i, bytes)),
        }
        self.coarsen();
    }

    /// Slices twice as long, as often as it takes to hold no more than `MAX_BINS`.
    fn coarsen(&mut self) {
        while self.bins.len() > MAX_BINS {
            self.bin_us *= 2;
            let mut out: Vec<(u64, u64)> = Vec::with_capacity(self.bins.len() / 2 + 1);
            for &(i, b) in &self.bins {
                match out.last_mut() {
                    Some(l) if l.0 == i / 2 => l.1 += b,
                    _ => out.push((i / 2, b)),
                }
            }
            self.bins = out;
        }
    }

    /// Many pieces of output at once, each (when, bytes), oldest first:
    /// history read from a journal after live output began. One pass over
    /// what is already held, however much there is of either.
    pub fn add_sorted(&mut self, pieces: &[(u64, u64)]) {
        if self.bin_us == 0 {
            self.bin_us = BIN_US;
        }
        let mut new: Vec<(u64, u64)> = Vec::with_capacity(pieces.len());
        for &(t_us, bytes) in pieces.iter().filter(|p| p.1 > 0) {
            self.total += bytes;
            self.first_us = Some(self.first_us.map_or(t_us, |f| f.min(t_us)));
            self.last_us = Some(self.last_us.map_or(t_us, |l| l.max(t_us)));
            new.push((t_us / self.bin_us, bytes));
        }
        // Already in order when the caller kept its word; cheap to make sure.
        new.sort_by_key(|p| p.0);
        let old = std::mem::take(&mut self.bins);
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(old.len() + new.len());
        let (mut a, mut b) = (old.into_iter().peekable(), new.into_iter().peekable());
        loop {
            let next = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) if x.0 <= y.0 => a.next(),
                (Some(_), None) => a.next(),
                (_, Some(_)) => b.next(),
                (None, None) => break,
            };
            let Some((i, bytes)) = next else { break };
            match out.last_mut() {
                Some(l) if l.0 == i => l.1 += bytes,
                _ => out.push((i, bytes)),
            }
        }
        self.bins = out;
        self.coarsen();
    }

    /// Bytes printed in each slice of `lo..=hi`, in at most `max_n` slices.
    /// A slice is a whole number of the stored ones, so each bar stands for
    /// exactly the stretch of time it is drawn over. Returns the length of a
    /// slice, when the first begins, and the bytes in each.
    pub fn slices(&self, lo: u64, hi: u64, max_n: usize) -> (u64, u64, Vec<u64>) {
        let bin = self.bin_us.max(BIN_US);
        if hi <= lo || max_n == 0 {
            return (bin, lo, Vec::new());
        }
        let first = lo / bin;
        let bins = (hi / bin - first + 1) as usize;
        let per = bins.div_ceil(max_n).max(1) as u64;
        let mut out = vec![0u64; bins.div_ceil(per as usize)];
        let from = self.bins.partition_point(|b| b.0 < first);
        for &(i, bytes) in &self.bins[from..] {
            if i * bin > hi {
                break;
            }
            out[((i - first) / per) as usize] += bytes;
        }
        (per * bin, first * bin, out)
    }
}

// ------------------------------------------------------------------ axis

/// The shared time axis: `lo..hi` (microseconds, Unix clock) across `w` pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Axis {
    pub lo: u64,
    pub hi: u64,
    pub x0: f32,
    pub w: f32,
}

impl Axis {
    pub fn x(&self, t: u64) -> f32 {
        let span = (self.hi - self.lo).max(1) as f64;
        self.x0 + ((t.clamp(self.lo, self.hi) - self.lo) as f64 / span) as f32 * self.w
    }

    pub fn t(&self, x: f32) -> u64 {
        let f = ((x - self.x0) / self.w.max(1.0)).clamp(0.0, 1.0) as f64;
        self.lo + (f * (self.hi - self.lo) as f64).round() as u64
    }
}

const SEC: u64 = 1_000_000;
const TICK_STEPS: [u64; 30] = [
    1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000,
    SEC, 2 * SEC, 5 * SEC, 10 * SEC, 15 * SEC, 30 * SEC, 60 * SEC, 120 * SEC, 300 * SEC, 600 * SEC, 900 * SEC, 1800 * SEC,
    3600 * SEC, 2 * 3600 * SEC, 3 * 3600 * SEC, 6 * 3600 * SEC, 12 * 3600 * SEC, 86_400 * SEC, 2 * 86_400 * SEC, 7 * 86_400 * SEC, 30 * 86_400 * SEC,
];

/// The smallest round step that puts at most `max_ticks` ticks in `span_us`.
pub fn tick_step(span_us: u64, max_ticks: usize) -> u64 {
    let max = max_ticks.max(1) as u64;
    TICK_STEPS.iter().copied().find(|&s| span_us / s <= max).unwrap_or(TICK_STEPS[TICK_STEPS.len() - 1])
}

/// A time since the session began, written at the precision of the tick step.
pub fn tick_label(t_us: u64, step_us: u64) -> String {
    let s = t_us / SEC;
    if step_us < SEC {
        let digits = if step_us >= 100_000 { 1 } else if step_us >= 10_000 { 2 } else { 3 };
        let frac = (t_us % SEC) / 10u64.pow(6 - digits);
        return match s {
            0..=59 => format!("{s}.{frac:0w$}s", w = digits as usize),
            _ => format!("{}:{:02}.{frac:0w$}", s / 60, s % 60, w = digits as usize),
        };
    }
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}:{:02}", s / 60, s % 60),
        _ => format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
    }
}

/// The shortest stretch of time the timeline can be zoomed to.
const MIN_SPAN_US: u64 = 20_000;

/// Zoom the visible range by `factor` (< 1 zooms in) keeping `anchor` where
/// it is on screen. `None` is the whole session.
pub fn zoomed(full: (u64, u64), cur: Option<(u64, u64)>, anchor: u64, factor: f64) -> Option<(u64, u64)> {
    let (lo, hi) = cur.unwrap_or(full);
    let whole = full.1.saturating_sub(full.0);
    let old = hi.saturating_sub(lo).max(1) as f64;
    let new = (old * factor).max(MIN_SPAN_US.min(whole.max(1)) as f64);
    if new >= whole as f64 {
        return None;
    }
    let a = anchor.clamp(lo, hi);
    let left = (a - lo) as f64 / old;
    let mut nlo = a as f64 - left * new;
    nlo = nlo.clamp(full.0 as f64, full.1 as f64 - new);
    let nlo = nlo.round() as u64;
    Some((nlo, (nlo + new.round() as u64).min(full.1)))
}

/// Slide the visible range by `frac` of its own width.
pub fn panned(full: (u64, u64), cur: Option<(u64, u64)>, frac: f64) -> Option<(u64, u64)> {
    let (lo, hi) = cur?;
    let span = hi - lo;
    let d = (span as f64 * frac).round() as i64;
    let nlo = (lo as i64 + d).clamp(full.0 as i64, (full.1 - span.min(full.1 - full.0)) as i64) as u64;
    Some((nlo, nlo + span))
}

// ---------------------------------------------------------------- forest

/// How a process stands: still going, ended with zero, ended with something
/// else, or ended without anyone learning its code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Failed,
    Ok,
    Unknown,
}

pub fn state(p: &Proc) -> State {
    match &p.end {
        None => State::Running,
        Some(e) => match e.code {
            Some(0) if !e.abnormal => State::Ok,
            Some(_) => State::Failed,
            None if e.abnormal => State::Failed,
            None => State::Unknown,
        },
    }
}

/// Classes of sibling, by what their whole subtree holds: something running,
/// else something that failed, else nothing of note.
const RUN: usize = 0;
const FAIL: usize = 1;
const QUIET: usize = 2;

/// The file name of a path without its directory or `.exe`.
fn stem(path: &str) -> &str {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match file.len().checked_sub(4) {
        Some(n) if file.is_char_boundary(n) && file[n..].eq_ignore_ascii_case(".exe") && n > 0 => &file[..n],
        _ => file,
    }
}

/// Where the program name ends in a command line (after the closing quote, or
/// at the first blank).
fn argv0_end(cmd: &str) -> usize {
    match cmd.strip_prefix('"') {
        Some(rest) => rest.find('"').map_or(cmd.len(), |i| i + 2),
        None => cmd.find(char::is_whitespace).unwrap_or(cmd.len()),
    }
}

/// What to call a process: its image's file name, else the first word of its
/// command line. `None` when neither is known.
pub fn name_of(p: &Proc) -> Option<&str> {
    if let Some(i) = p.image.as_deref().filter(|s| !s.trim().is_empty()) {
        return Some(stem(i.trim()));
    }
    let c = p.cmd.as_deref()?.trim_start();
    let word = c[..argv0_end(c)].trim_matches('"');
    (!word.is_empty()).then(|| stem(word))
}

/// The command line without the program's own name, on one line.
pub fn args_of(p: &Proc) -> String {
    let Some(c) = p.cmd.as_deref() else { return String::new() };
    let c = c.trim_start();
    let rest = &c[argv0_end(c).min(c.len())..];
    one_line(rest)
}

/// Runs of blanks and control characters become one space.
fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// A long path inside one word of a command line, cut to its last two parts
/// (`--manifest-path=…\tiny\Cargo.toml`). Anything else is returned as it is.
fn short_path(word: &str) -> std::borrow::Cow<'_, str> {
    if word.len() < 26 || word.contains("://") {
        return word.into();
    }
    let seps: Vec<usize> = word.char_indices().filter(|(_, c)| *c == '\\' || *c == '/').map(|(i, _)| i).collect();
    if seps.len() < 3 {
        return word.into();
    }
    // What comes before the path stays: an option's name, an opening quote.
    let head = word[..seps[0]].rfind(['=', '"', ',']).map_or(0, |i| i + 1);
    let last = seps[seps.len() - 1];
    let two = seps[seps.len() - 2];
    let from = if word.len() - two <= 34 { two } else { last };
    if from <= head + 2 {
        return word.into();
    }
    format!("{}…{}", &word[..head], &word[from..]).into()
}

/// A command line's arguments as a node has room for them: long paths cut to
/// their ends. The whole line is on the hover card.
fn shorten(args: &str) -> String {
    let mut out = String::with_capacity(args.len());
    for word in args.split(' ') {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&short_path(word));
    }
    out
}

/// `s` cut to `max` characters, the last being an ellipsis if anything was cut.
fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    s.chars().take(max - 1).chain(Some('…')).collect()
}

const NONE: u32 = u32::MAX;

/// The ledger as arrays: who is whose child, and what each subtree holds.
/// Index `i` is the i-th process of the ledger; index `n` stands for "above
/// the roots".
struct Forest<'a> {
    procs: &'a [Proc],
    parent: Vec<u32>,
    depth: Vec<u32>,
    state: Vec<State>,
    class: Vec<u8>,
    /// Processes beneath each one.
    desc: Vec<u32>,
    /// Failed, and running, processes in each subtree (itself included).
    fails: Vec<u32>,
    runs: Vec<u32>,
    name: Vec<u32>,
    names: Vec<&'a str>,
    kid_at: Vec<u32>,
    kids: Vec<u32>,
}

impl<'a> Forest<'a> {
    fn of(ledger: &'a Ledger) -> Self {
        let procs = ledger.procs();
        let n = procs.len();
        let mut parent = vec![NONE; n];
        let mut depth = vec![0u32; n];
        let mut name = vec![0u32; n];
        let mut names: Vec<&str> = Vec::new();
        let mut ids: HashMap<&str, u32> = HashMap::new();
        let mut kid_at = vec![0u32; n + 2];
        for (i, p) in procs.iter().enumerate() {
            // A parent always has a smaller id than its child, so it is
            // already placed when the child is reached.
            if let Some(pi) = p.parent.and_then(|id| procs[..i].binary_search_by_key(&id, |q| q.id).ok()) {
                parent[i] = pi as u32;
                depth[i] = depth[pi] + 1;
            }
            let key = name_of(p).unwrap_or("");
            name[i] = *ids.entry(key).or_insert_with(|| {
                names.push(key);
                names.len() as u32 - 1
            });
            kid_at[if parent[i] == NONE { n } else { parent[i] as usize } + 1] += 1;
        }
        for i in 0..=n {
            kid_at[i + 1] += kid_at[i];
        }
        let mut fill = kid_at.clone();
        let mut kids = vec![0u32; n];
        for i in 0..n {
            let slot = if parent[i] == NONE { n } else { parent[i] as usize };
            kids[fill[slot] as usize] = i as u32;
            fill[slot] += 1;
        }
        let state: Vec<State> = procs.iter().map(state).collect();
        let mut desc = vec![0u32; n];
        let mut fails: Vec<u32> = state.iter().map(|s| (*s == State::Failed) as u32).collect();
        let mut runs: Vec<u32> = state.iter().map(|s| (*s == State::Running) as u32).collect();
        for i in (0..n).rev() {
            if parent[i] != NONE {
                let p = parent[i] as usize;
                desc[p] += desc[i] + 1;
                fails[p] += fails[i];
                runs[p] += runs[i];
            }
        }
        let class = (0..n).map(|i| if runs[i] > 0 { RUN } else if fails[i] > 0 { FAIL } else { QUIET } as u8).collect();
        Forest { procs, parent, depth, state, class, desc, fails, runs, name, names, kid_at, kids }
    }

    fn n(&self) -> usize {
        self.procs.len()
    }

    fn kids_of(&self, i: usize) -> &[u32] {
        &self.kids[self.kid_at[i] as usize..self.kid_at[i + 1] as usize]
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.procs.binary_search_by_key(&id, |p| p.id).ok()
    }

    fn label(&self, i: usize) -> String {
        match self.names[self.name[i] as usize] {
            "" => format!("pid {}", self.procs[i].pid),
            s => s.to_string(),
        }
    }
}

/// How much is drawn expanded: of each class of sibling, the most recent
/// `keep` stay as their own nodes and the earlier ones are counted in groups
/// by name; at most `kinds` groups per class, the rest merged into one.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Detail {
    keep: [usize; 3],
    kinds: [usize; 3],
}

/// Detail from most to least. Finished successes are folded first, failures
/// next, running processes last; of failures and of running processes the
/// most recent always stays expanded.
fn ladder() -> Vec<Detail> {
    const ALL: usize = usize::MAX;
    const KEEP: [usize; 11] = [24, 16, 12, 9, 7, 5, 4, 3, 2, 1, 0];
    const KINDS: [usize; 5] = [8, 5, 3, 2, 1];
    let mut d = Detail { keep: [ALL; 3], kinds: [ALL; 3] };
    let mut out = vec![d];
    for class in [QUIET, FAIL, RUN] {
        for &k in KEEP.iter().filter(|&&k| k > 0 || class == QUIET) {
            d.keep[class] = k;
            out.push(d);
        }
        for &k in &KINDS {
            d.kinds[class] = k;
            out.push(d);
        }
    }
    out
}

/// Rows the tree takes at a level of detail, without building it.
fn rows_at(f: &Forest, d: &Detail, max_depth: usize, rows: &mut [u32], stamp: &mut [u32]) -> usize {
    let n = f.n();
    stamp.iter_mut().for_each(|s| *s = 0);
    // Children have larger indices than their parents, so going backwards
    // meets every child before its parent; the roots' own parent comes last.
    for p in (0..n).rev().chain(Some(n)) {
        let kids = f.kids_of(p);
        if p < n && (kids.is_empty() || f.depth[p] as usize + 1 >= max_depth) {
            rows[p] = 1;
            continue;
        }
        let mut count = [0usize; 3];
        for &k in kids {
            count[f.class[k as usize] as usize] += 1;
        }
        let (mut seen, mut kinds, mut total) = ([0usize; 3], [0u32; 3], 0u32);
        for &k in kids {
            let k = k as usize;
            let c = f.class[k] as usize;
            let folded = seen[c] + d.keep[c].min(count[c]) < count[c];
            seen[c] += 1;
            if folded {
                let s = &mut stamp[f.name[k] as usize * 3 + c];
                if *s != p as u32 + 1 {
                    *s = p as u32 + 1;
                    kinds[c] += 1;
                }
            } else {
                total += rows[k];
            }
        }
        for c in 0..3 {
            total += kinds[c].min(d.kinds[c].clamp(1, u32::MAX as usize) as u32);
        }
        rows[p] = if p < n { total.max(1) } else { total };
    }
    rows[n] as usize
}

/// What a node of the drawn tree stands for.
#[derive(Clone, Debug, PartialEq)]
pub enum What {
    /// One process (its index in the ledger). `inside` processes beneath it
    /// are not drawn (it sits in the last column, or was the only one of its
    /// kind to be folded).
    Proc { index: usize, inside: usize },
    /// Siblings counted together. `name` is `None` for a mix of kinds.
    Group { class: u8, name: Option<String>, kinds: usize, members: Vec<u32> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shown {
    pub what: What,
    pub depth: usize,
    pub row: usize,
    pub rows: usize,
    pub parent: Option<usize>,
}

/// The drawn tree at a level of detail: depth first, each node after its
/// parent, siblings in the order their first member was created.
fn show(f: &Forest, d: &Detail, max_depth: usize) -> Vec<Shown> {
    enum Item {
        Kept(u32),
        Group(u8, Option<u32>, usize, Vec<u32>),
    }
    fn place(f: &Forest, d: &Detail, max_depth: usize, p: usize, at: Option<usize>, depth: usize, row: &mut usize, out: &mut Vec<Shown>) {
        let kids = f.kids_of(p);
        let mut count = [0usize; 3];
        for &k in kids {
            count[f.class[k as usize] as usize] += 1;
        }
        let mut seen = [0usize; 3];
        let mut items: Vec<(u32, Item)> = Vec::new();
        let mut folded: Vec<(u8, u32, u32)> = Vec::new();
        for &k in kids {
            let c = f.class[k as usize] as usize;
            if seen[c] + d.keep[c].min(count[c]) < count[c] {
                folded.push((c as u8, f.name[k as usize], k));
            } else {
                items.push((k, Item::Kept(k)));
            }
            seen[c] += 1;
        }
        // Sorted by class, then name, then age: each run is one group.
        folded.sort_unstable();
        let mut groups: Vec<(u8, u32, Vec<u32>)> = Vec::new();
        for (c, name, k) in folded {
            match groups.last_mut() {
                Some(g) if g.0 == c && g.1 == name => g.2.push(k),
                _ => groups.push((c, name, vec![k])),
            }
        }
        for class in 0..3u8 {
            let split = groups.iter().position(|g| g.0 != class).unwrap_or(groups.len());
            let mut of_class: Vec<(u8, u32, Vec<u32>)> = groups.drain(..split).collect();
            let cap = d.kinds[class as usize].max(1);
            if of_class.len() > cap {
                // The largest kinds keep their names; the rest become one group.
                of_class.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then(a.2[0].cmp(&b.2[0])));
                let rest = of_class.split_off(cap - 1);
                let kinds = rest.len();
                let mut members: Vec<u32> = rest.into_iter().flat_map(|g| g.2).collect();
                members.sort_unstable();
                items.push((members[0], Item::Group(class, None, kinds, members)));
            }
            for (c, name, members) in of_class {
                items.push((members[0], Item::Group(c, Some(name), 1, members)));
            }
        }
        items.sort_by_key(|i| i.0);
        for (_, item) in items {
            let me = out.len();
            let top = *row;
            match item {
                Item::Kept(k) => {
                    let k = k as usize;
                    let leaf = f.kids_of(k).is_empty() || depth + 1 >= max_depth;
                    out.push(Shown { what: What::Proc { index: k, inside: if leaf { f.desc[k] as usize } else { 0 } }, depth, row: top, rows: 1, parent: at });
                    if leaf {
                        *row += 1;
                    } else {
                        place(f, d, max_depth, k, Some(me), depth + 1, row, out);
                        if *row == top {
                            *row += 1;
                        }
                    }
                }
                // A group of one is that process, with what is beneath it folded in.
                Item::Group(_, _, _, members) if members.len() == 1 => {
                    let k = members[0] as usize;
                    out.push(Shown { what: What::Proc { index: k, inside: f.desc[k] as usize }, depth, row: top, rows: 1, parent: at });
                    *row += 1;
                }
                Item::Group(class, name, kinds, members) => {
                    let name = name.map(|id| f.label(members.iter().copied().find(|&m| f.name[m as usize] == id).unwrap_or(members[0]) as usize));
                    out.push(Shown { what: What::Group { class, name, kinds, members }, depth, row: top, rows: 1, parent: at });
                    *row += 1;
                }
            }
            out[me].rows = *row - top;
        }
    }
    let mut out = Vec::new();
    let mut row = 0;
    place(f, d, max_depth, f.n(), None, 0, &mut row, &mut out);
    out
}

/// The tree as it will be drawn for a window with room for `max_rows` rows
/// and `max_depth` columns: the most detailed level that fits.
fn arrange(f: &Forest, max_rows: usize, max_depth: usize) -> (Vec<Shown>, usize) {
    let levels = ladder();
    let mut rows = vec![0u32; f.n() + 1];
    let mut stamp = vec![0u32; f.names.len() * 3];
    let fits = |i: usize, rows: &mut [u32], stamp: &mut [u32]| rows_at(f, &levels[i], max_depth, rows, stamp) <= max_rows;
    // Rows never grow as detail falls, so the first level that fits is found by halving.
    let (mut lo, mut hi) = (0, levels.len() - 1);
    if fits(0, &mut rows, &mut stamp) {
        hi = 0;
    }
    while lo < hi {
        let mid = (lo + hi) / 2;
        if fits(mid, &mut rows, &mut stamp) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    // Between two steps of the ladder there may be room for a few more:
    // keep as many expanded as still fit.
    let mut best = levels[hi];
    if hi > 0 {
        let prev = levels[hi - 1];
        for c in 0..3 {
            for which in 0..2 {
                let (fit_v, miss_v) = if which == 0 { (best.keep[c], prev.keep[c]) } else { (best.kinds[c], prev.kinds[c]) };
                // `fit_v` fits and `miss_v` does not (or they are the same
                // step, and there is nothing between them to try).
                let (mut lo, mut hi_v) = (fit_v, miss_v.min(f.n()));
                while hi_v > lo && hi_v - lo > 1 {
                    let mid = lo + (hi_v - lo) / 2;
                    let mut d = best;
                    if which == 0 { d.keep[c] = mid } else { d.kinds[c] = mid }
                    if rows_at(f, &d, max_depth, &mut rows, &mut stamp) <= max_rows {
                        lo = mid;
                    } else {
                        hi_v = mid;
                    }
                }
                if which == 0 { best.keep[c] = lo } else { best.kinds[c] = lo }
            }
        }
    }
    (show(f, &best, max_depth), hi)
}

// --------------------------------------------------------------- palette

struct Palette {
    bg: C3,
    ink: C3,
    soft: C3,
    muted: C3,
    faint: C3,
    hair: C3,
    accent: C3,
    ok: C3,
    fail: C3,
}

impl Palette {
    fn of(th: &Theme) -> Self {
        Palette {
            bg: th.bg,
            ink: th.fg,
            soft: mix(th.fg, th.bg, 0.22),
            muted: mix(th.fg, th.bg, 0.48),
            faint: mix(th.fg, th.bg, 0.66),
            hair: mix(th.fg, th.bg, 0.86),
            accent: th.accent,
            // Exit state is the only place red and green appear.
            ok: mix(th.ansi[2], th.bg, 0.42),
            fail: th.ansi[1],
        }
    }

    fn of_state(&self, s: State) -> C3 {
        match s {
            State::Running => self.accent,
            State::Failed => self.fail,
            State::Ok => self.ok,
            State::Unknown => self.faint,
        }
    }

    fn of_class(&self, c: u8) -> C3 {
        match c as usize {
            RUN => self.accent,
            FAIL => self.fail,
            _ => self.ok,
        }
    }
}

// ----------------------------------------------------------------- build

/// Everything a picture is made from.
pub struct Frame<'a> {
    pub ledger: &'a Ledger,
    pub activity: &'a Activity,
    /// The session's first and latest moments (microseconds, Unix clock).
    /// Time on screen ends at `t1_us`; nothing reads the wall clock.
    pub t0_us: u64,
    pub t1_us: u64,
    pub session: &'a str,
    /// The session's own end, once it has ended: its exit code if known.
    pub ended: Option<Option<u32>>,
    /// Bytes printed before this window attached whose times are not known.
    pub unplaced: u64,
    pub zoom: Option<(u64, u64)>,
    pub hover: Option<Target>,
    /// Processes that have just started, and just failed: (ledger id, when,
    /// on the frame clock), ascending by id.
    pub born: &'a [(u64, f64)],
    pub failed: &'a [(u64, f64)],
    pub size: [f32; 2],
    pub pad: f32,
    pub ty: Type,
    pub theme: &'a Theme,
}

/// Light from a process starting, and from one failing.
const EMIT_BORN: f32 = 0.20;
const EMIT_FAILED: f32 = 0.30;

fn when(list: &[(u64, f64)], id: u64) -> Option<f64> {
    list.binary_search_by(|e| e.0.cmp(&id)).ok().map(|i| list[i].1)
}

fn bytes(n: u64) -> String {
    match n {
        0..=999 => format!("{n} B"),
        1_000..=9_999 => format!("{:.2} kB", n as f64 / 1e3),
        10_000..=999_999 => format!("{:.1} kB", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1} MB", n as f64 / 1e6),
        _ => format!("{:.2} GB", n as f64 / 1e9),
    }
}

fn exit_text(code: u32) -> String {
    if code >= 0x8000_0000 {
        format!("exit 0x{code:08X}")
    } else {
        format!("exit {code}")
    }
}

/// Build the picture.
pub fn build(f: &Frame) -> Scene {
    let mut sc = Scene::default();
    let (ty, pal) = (f.ty, Palette::of(f.theme));
    let u = ty.unit.max(0.5);
    let (w, h) = (f.size[0], f.size[1]);
    let (x0, x1) = (f.pad.round(), (w - f.pad).round());
    let forest = Forest::of(f.ledger);
    let n = forest.n();
    sc.counts.procs = n;
    sc.counts.running = forest.state.iter().filter(|s| **s == State::Running).count();
    sc.counts.failed = forest.state.iter().filter(|s| **s == State::Failed).count();
    if x1 - x0 < 8.0 * ty.w(Size::Small) || h < 6.0 * ty.h(Size::Small) {
        return sc; // no room to say anything true
    }
    let small = h < 22.0 * ty.h(Size::Body);
    let (sw, sh) = (ty.w(Size::Small), ty.h(Size::Small));

    // ---- header: what is true of the session right now
    let head_bottom = header(&mut sc, f, &forest, &pal, small, x0, x1);

    // ---- timeline along the bottom
    let lanes = Lanes::of(&forest);
    let lane_gap = (2.0 * u).round();
    let lane_h_max = (sh * 0.34).round().max(3.0 * u);
    let lanes_want = lanes.rows as f32 * (lane_h_max + lane_gap);
    let lanes_h = lanes_want.clamp(sh * 1.2, if small { sh * 2.0 } else { sh * 4.6 }).round();
    let strip_h = (if small { sh * 1.2 } else { sh * 1.9 }).round();
    let band_h = sh + lanes_h + (sh * 0.5).round() + sh + strip_h + sh;
    let band_top = (h - f.pad - band_h).round();
    let full = (f.t0_us.min(f.t1_us), f.t1_us.max(f.t0_us + 1));
    let (lo, hi) = f.zoom.map_or(full, |(a, b)| {
        let lo = a.clamp(full.0, full.1 - 1);
        (lo, b.clamp(lo + 1, full.1))
    });
    let axis = Axis { lo, hi, x0, w: x1 - x0 };
    sc.axis = Some(axis);
    sc.quad(x0, band_top - (sh * 0.45).round(), x1 - x0, u.round().max(1.0), pal.hair, 1.0, 0.0);

    // Section line: what the marks mean.
    let mut y = band_top;
    let mut lx = sc.text(&ty, x0, y, "processes", pal.muted, Size::Small, false);
    lx = sc.text(&ty, lx, y, "  one span each, stacked by depth", pal.faint, Size::Small, false);
    if f.ledger.dropped > 0 {
        // A long-lived window keeps only its most recent history; what it
        // let go is counted above but can no longer be drawn.
        lx = sc.text(&ty, lx, y, &format!("  ({} earlier ones are no longer held)", f.ledger.dropped), pal.faint, Size::Small, false);
    }
    let legend_from = if f.zoom.is_some() {
        // A zoomed axis says so: what stretch is shown, of how much.
        let part = format!("  showing {} to {} of {}", procs::span(lo - full.0), procs::span(hi - full.0), procs::span(full.1 - full.0));
        sc.text(&ty, lx, y, &part, pal.soft, Size::Small, false)
    } else {
        lx
    };
    {
        // A legend: state is never told by colour alone.
        let items = [("running", State::Running), ("exit 0", State::Ok), ("failed", State::Failed)];
        let total: f32 = items.iter().map(|(s, _)| (s.chars().count() as f32 + 3.5) * sw).sum();
        let mut x = x1 - total + 1.5 * sw;
        if x > legend_from + 2.0 * sw {
            for (label, st) in items {
                let d = (sh * 0.26).round();
                sc.quad(x, y + ((sh - d) * 0.5).round(), d * 1.6, d, pal.of_state(st), 1.0, 2.0 * u);
                x = sc.text(&ty, x + d * 1.6 + sw * 0.6, y, label, pal.muted, Size::Small, false) + 1.4 * sw;
            }
        }
    }
    y += sh;

    // Ticks, drawn first so the marks sit on them (and not through the labels).
    let span = hi - lo;
    let step = tick_step(span, ((x1 - x0) / (11.0 * sw)).floor().max(2.0) as usize);
    let strip_y = y + lanes_h + (sh * 0.5).round() + sh;
    let axis_y = strip_y + strip_h;
    {
        let rel = lo - full.0;
        let mut t = rel.div_ceil(step) * step;
        let mut last_x = f32::MIN;
        while t <= rel + span {
            let x = axis.x(full.0 + t).round();
            let hair = u.round().max(1.0);
            sc.quad(x, y, hair, lanes_h, pal.hair, 0.55, 0.0);
            sc.quad(x, strip_y, hair, strip_h + (sh * 0.22).round(), pal.hair, 0.55, 0.0);
            let label = tick_label(t, step);
            let lw = label.chars().count() as f32 * sw;
            let tx = (x + 0.5 * sw).min(x1 - lw);
            if tx > last_x + sw {
                sc.text(&ty, tx, axis_y, &label, pal.faint, Size::Small, false);
                last_x = tx + lw;
            }
            t += step;
        }
    }

    // Process spans.
    {
        let rows = lanes.rows.max(1) as f32;
        let pitch = (lanes_h / rows).min(lane_h_max + lane_gap).max(2.0);
        let bar_h = (pitch - lane_gap).max(1.5).min(lane_h_max);
        let top = y + ((lanes_h - pitch * rows) * 0.5).max(0.0).round();
        let min_w = (2.0 * u).round().max(2.0);
        let mut last: Vec<(i32, i32, u8)> = vec![(i32::MIN, 0, 9); lanes.rows.max(1)];
        // Successes first, then what is running, failures on top: red stays visible.
        for pass in [State::Unknown, State::Ok, State::Running, State::Failed] {
            for i in 0..n {
                if forest.state[i] != pass {
                    continue;
                }
                let p = &forest.procs[i];
                let end = p.end.as_ref().map_or(f.t1_us, |e| e.end_us).max(p.start_us);
                if end < lo || p.start_us > hi {
                    continue;
                }
                let xa = axis.x(p.start_us).round();
                let xb = axis.x(end).round().max(xa + min_w).min(x1.max(xa + min_w));
                let row = lanes.row[i] as usize;
                let key = (xa as i32, xb as i32, pass as u8);
                if last[row] == key {
                    continue; // the same pixels in the same colour: one mark says it
                }
                last[row] = key;
                let yy = (top + row as f32 * pitch).round();
                let hot = f.hover == Some(Target::Span(p.id)) || f.hover == Some(Target::Node(p.id));
                let alpha = if pass == State::Ok || pass == State::Unknown { 0.85 } else { 1.0 };
                sc.quad(xa, yy, xb - xa, bar_h.round().max(1.0), pal.of_state(pass), alpha, (bar_h * 0.5).min(2.0 * u));
                if hot {
                    sc.over.push(Quad { x: xa - 2.0 * u, y: yy - 2.0 * u, w: xb - xa + 4.0 * u, h: bar_h.round().max(1.0) + 4.0 * u, color: pal.ink, alpha: 0.9, radius: 3.0 * u, ring: (1.0 * u).max(1.0) });
                }
                let grow = (4.0 * u - (xb - xa)).max(0.0) * 0.5;
                sc.hits.push(Hit { rect: [xa - grow, yy - lane_gap * 0.5, xb - xa + 2.0 * grow, pitch], target: Target::Span(p.id) });
                sc.counts.spans += 1;
            }
        }
    }
    y += lanes_h + (sh * 0.5).round();

    // Output: bytes printed in each slice of time.
    {
        let bar_min = (3.0 * u).round().max(2.0);
        let gap = u.round().max(1.0);
        let (slice_us, first_us, b) = f.activity.slices(lo, hi, (((x1 - x0) + gap) / (bar_min + gap)).floor().max(1.0) as usize);
        let peak = b.iter().copied().max().unwrap_or(0);
        let mut lx = sc.text(&ty, x0, y, "output", pal.muted, Size::Small, false);
        lx = sc.text(&ty, lx, y, &format!("  {} printed", bytes(f.activity.total)), pal.faint, Size::Small, false);
        if f.unplaced > 0 {
            lx = sc.text(&ty, lx, y, &format!(", and {} before this window attached (when is not known)", bytes(f.unplaced)), pal.faint, Size::Small, false);
        }
        if peak > 0 {
            let label = format!("each bar {}, tallest {}", procs::span(slice_us), bytes(peak));
            let lw = label.chars().count() as f32 * sw;
            if x1 - lw > lx + 2.0 * sw {
                sc.text(&ty, x1 - lw, y, &label, pal.faint, Size::Small, false);
            }
        }
        let base = y + sh + strip_h;
        sc.quad(x0, base, x1 - x0, u.round().max(1.0), pal.hair, 1.0, 0.0);
        for (k, &v) in b.iter().enumerate() {
            if v == 0 {
                continue;
            }
            // Linear in bytes; anything printed at all is at least a visible
            // tick. A bar covers the stretch of time its bytes arrived in.
            let hh = ((v as f64 / peak as f64) as f32 * (strip_h - 2.0 * u)).max(2.0 * u).round();
            let t = first_us + k as u64 * slice_us;
            let (xa, xb) = (axis.x(t.max(lo)).round(), axis.x((t + slice_us).min(hi)).round());
            let wd = (xb - xa - gap).max(bar_min.min(xb - xa)).max(2.0);
            sc.quad(xa, base - hh, wd, hh, pal.muted, 0.85, 0.0);
        }
    }

    // ---- the tree, in what is left between the two
    let tree_top = head_bottom + (sh * 0.5).round();
    let tree_bottom = band_top - (sh * 0.9).round();
    let shown = tree(&mut sc, f, &forest, &pal, [x0, tree_top, x1, tree_bottom]);
    if n == 0 && tree_bottom - tree_top >= sh {
        // Every sensed session has at least its shell, so none at all means
        // the session was recorded without process sensing. Said, not left blank.
        let msg = "this session has no process records: it was recorded without process sensing, so there is no tree to draw";
        sc.text(&ty, x0, tree_top, &fit(msg, ((x1 - x0) / sw) as usize), pal.muted, Size::Small, false);
    }

    // ---- hover card, last: it sits on top
    if let Some(t) = f.hover {
        card(&mut sc, f, &forest, &pal, &shown, t, [x0, f.pad.round(), x1, (h - f.pad).round()]);
    }
    sc
}

/// The counters. Returns the y where the header ends.
fn header(sc: &mut Scene, f: &Frame, forest: &Forest, pal: &Palette, small: bool, x0: f32, x1: f32) -> f32 {
    let ty = f.ty;
    let c = sc.counts;
    let elapsed = procs::span(f.t1_us.saturating_sub(f.t0_us));
    let failed_ink = if c.failed > 0 { pal.fail } else { pal.muted };
    let running_ink = if c.running > 0 { pal.accent } else { pal.muted };
    let total = f.ledger.total.max(forest.n() as u64);
    // With no process records at all the counts are not zero, they are not
    // known: a dash, not a number.
    let count = |v: u64| if total == 0 { "\u{2013}".to_string() } else { v.to_string() };
    let tiles: [(String, &str, C3); 4] = [
        (count(c.running as u64), "running", running_ink),
        (count(total), "spawned", if total == 0 { pal.muted } else { pal.ink }),
        (count(c.failed as u64), "failed", failed_ink),
        (elapsed, "elapsed", pal.ink),
    ];
    // Under the session's name: how it ended, or, while it has not, how to
    // leave the view (the one line here that is not data).
    let state = match f.ended {
        None => "esc returns, the wheel zooms the timeline".to_string(),
        Some(Some(code)) => format!("ended, {}", exit_text(code)),
        Some(None) => "ended".to_string(),
    };
    let (sw, sh) = (ty.w(Size::Small), ty.h(Size::Small));
    let top = f.pad.round();
    if small {
        // One line: the same counters, said smaller.
        let (bw, bh) = (ty.w(Size::Body), ty.h(Size::Body));
        let mut x = x0;
        for (value, label, ink) in &tiles {
            // A counter is drawn whole or not at all: in a window too narrow
            // for all four, the later ones (elapsed first) are left out.
            let wide = value.chars().count() as f32 * bw + 0.5 * bw + label.chars().count() as f32 * sw;
            if x + wide > x1 {
                break;
            }
            x = sc.text(&ty, x, top, value, *ink, Size::Body, true);
            x = sc.text(&ty, x + 0.5 * bw, top + ((bh - sh) * 0.5).round(), label, pal.muted, Size::Small, false) + 1.6 * bw;
        }
        // Which session, and how it ended. Where both do not fit, how it
        // ended is said and the name gives way; in the narrowest windows
        // that is the exit code alone.
        let mut wanted = if f.ended.is_none() { vec![f.session.to_string()] } else { vec![format!("{}  {state}", f.session), state.clone()] };
        if let Some(Some(code)) = f.ended {
            wanted.push(exit_text(code));
        }
        if let Some(right) = wanted.iter().find(|r| x1 - r.chars().count() as f32 * sw > x + bw) {
            sc.text(&ty, x1 - right.chars().count() as f32 * sw, top + ((bh - sh) * 0.5).round(), right, pal.faint, Size::Small, false);
        }
        return top + bh;
    }
    let (lw, lh) = (ty.w(Size::Large), ty.h(Size::Large));
    let label_y = top + (lh * 0.84).round();
    let mut x = x0;
    for (value, label, ink) in &tiles {
        let wide = (value.chars().count() as f32 * lw).max(label.chars().count() as f32 * sw);
        if x + wide > x1 {
            break; // whole or not at all, as above
        }
        sc.text(&ty, x, top, value, *ink, Size::Large, false);
        sc.text(&ty, x, label_y, label, pal.muted, Size::Small, false);
        x += wide + 1.6 * lw;
    }
    // The session, on the right: which one this is, and whether it is over.
    let lines = [format!("session {}", f.session), state];
    for (i, line) in lines.iter().enumerate() {
        let rw = line.chars().count() as f32 * sw;
        if !line.is_empty() && x1 - rw > x {
            let ink = if i == 0 { pal.muted } else { pal.faint };
            sc.text(&ty, x1 - rw, top + (lh * 0.84).round() - (1 - i as i32) as f32 * sh, line, ink, Size::Small, false);
        }
    }
    label_y + sh
}

/// Which row of the timeline each process's span sits in: one band per
/// depth, and within a band as many rows as ran side by side (up to a cap).
struct Lanes {
    row: Vec<u32>,
    rows: usize,
}

impl Lanes {
    const DEPTHS: usize = 8;
    const SIDE_BY_SIDE: usize = 6;

    fn of(f: &Forest) -> Self {
        let n = f.n();
        // Sub-rows per depth: when each is next free.
        let mut free: Vec<Vec<u64>> = vec![Vec::new(); Self::DEPTHS];
        let mut sub = vec![(0u8, 0u8); n];
        for i in 0..n {
            let p = &f.procs[i];
            let d = (f.depth[i] as usize).min(Self::DEPTHS - 1);
            let end = p.end.as_ref().map_or(u64::MAX, |e| e.end_us.max(p.start_us + 1)).max(p.start_us + 1);
            let rows = &mut free[d];
            let k = match rows.iter().position(|&e| e <= p.start_us) {
                Some(k) => k,
                None if rows.len() < Self::SIDE_BY_SIDE => {
                    rows.push(0);
                    rows.len() - 1
                }
                // More at once than there are rows: share the one free soonest.
                None => (0..rows.len()).min_by_key(|&k| rows[k]).unwrap_or(0),
            };
            rows[k] = rows[k].max(end);
            sub[i] = (d as u8, k as u8);
        }
        let mut base = [0u32; Self::DEPTHS];
        let mut total = 0u32;
        for d in 0..Self::DEPTHS {
            base[d] = total;
            total += free[d].len() as u32;
        }
        Lanes { row: sub.iter().map(|&(d, k)| base[d as usize] + k as u32).collect(), rows: total as usize }
    }
}

/// Draw the process tree inside `area` (left, top, right, bottom). Returns
/// the arrangement that was drawn.
fn tree(sc: &mut Scene, f: &Frame, forest: &Forest, pal: &Palette, area: [f32; 4]) -> Vec<Shown> {
    let ty = f.ty;
    let u = ty.unit.max(0.5);
    let (sw, sh) = (ty.w(Size::Small), ty.h(Size::Small));
    let [ax0, ay0, ax1, ay1] = area;
    let n = forest.n();
    if n == 0 || ay1 - ay0 < sh {
        return Vec::new();
    }
    let node_h = (sh * 0.94).round();
    let pitch_min = node_h + (5.0 * u).round();
    let pitch_max = node_h + (sh * 0.42).round();
    let max_rows = (((ay1 - ay0) / pitch_min).floor() as usize).max(1);
    let gap = (3.0 * ty.w(Size::Body)).round();
    let min_w = 15.0 * sw;
    let max_depth = (((ax1 - ax0 + gap) / (min_w + gap)).floor() as usize).max(1);

    let (shown, level) = arrange(forest, max_rows, max_depth);
    let total_rows = shown.iter().filter(|s| s.parent.is_none()).map(|s| s.rows).sum::<usize>();
    let cols = shown.iter().map(|s| s.depth + 1).max().unwrap_or(1);
    let rows_fit = total_rows.min(max_rows);
    let pitch = ((ay1 - ay0) / rows_fit.max(1) as f32).clamp(pitch_min, pitch_max).floor();
    sc.counts.level = level;
    sc.counts.rows = total_rows;
    // A cut is said in the last row, so that row is given up too.
    let cut = total_rows > max_rows;
    let last_row = if cut { max_rows.saturating_sub(1) } else { total_rows };
    sc.counts.rows_cut = total_rows - last_row.min(total_rows);

    // Where each drawn node is, and which drawn node each process ended up in.
    let mut home = vec![NONE; n];
    for (si, s) in shown.iter().enumerate() {
        match &s.what {
            What::Proc { index, .. } => home[*index] = si as u32,
            What::Group { members, .. } => members.iter().for_each(|&m| home[m as usize] = si as u32),
        }
    }
    for i in 0..n {
        if home[i] == NONE && forest.parent[i] != NONE {
            home[i] = home[forest.parent[i] as usize];
        }
    }
    let node_of = |id: u64| forest.index_of(id).map(|i| (i, home[i])).filter(|&(_, s)| s != NONE);

    let row_y = |row: usize| (ay0 + row as f32 * pitch + ((pitch - node_h) * 0.5).floor()).round();
    let pad_in = (0.75 * sw).round();
    let dot = (sh * 0.24).round().max(3.0);
    let chrome = 2.0 * pad_in + dot + (0.7 * sw).round();

    // What each node says: its name, then tags (state, counts, a long
    // life), then as much of its arguments as there is room for. Names are
    // in text ink; the state colour is carried by the mark and the tag.
    /// A tag a node says even if its name has to be cut for it: an exit
    /// code, what is folded inside it, a group's state.
    struct Must {
        /// The same fact from most words to fewest ("+10 inside, 1 failed",
        /// "+10, 1 failed", "1 failed"); a narrow column gets a shorter one.
        forms: [String; 3],
        ink: C3,
        /// It says that something failed or is still running. A column too
        /// narrow for everything loses its other tags first, and the name
        /// is cut to nothing before this is left unsaid.
        vital: bool,
    }
    impl Must {
        fn one(text: String, ink: C3, vital: bool) -> Self {
            Must { forms: [text.clone(), text.clone(), text], ink, vital }
        }
    }
    struct Label {
        name: Vec<(String, C3)>,
        must: Vec<Must>,
        /// Said when there is room.
        nice: Vec<(String, C3)>,
        args: String,
    }
    impl Label {
        const ARGS_MAX: usize = 64;
        fn want(&self) -> usize {
            let tags = self.must.iter().map(|t| &t.forms[0]).chain(self.nice.iter().map(|t| &t.0)).map(|t| t.chars().count() + 2).sum::<usize>();
            self.name.iter().map(|s| s.0.chars().count()).sum::<usize>() + tags + if self.args.is_empty() { 0 } else { 1 + self.args.chars().count().min(Self::ARGS_MAX) }
        }
        /// The pieces that fit `budget` characters: (text, ink, bold).
        fn fit(&self, budget: usize, args_ink: C3) -> Vec<(String, C3, bool)> {
            let mut segs: Vec<(String, C3, bool)> = self.name.iter().map(|(s, c)| (s.clone(), *c, true)).collect();
            let rest: usize = segs.iter().skip(1).map(|s| s.0.chars().count()).sum();
            let first = segs[0].0.chars().count();
            let name_min = first.min(3) + rest;
            let full = first + rest;
            // (text, ink, vital)
            let said = |form: usize| -> Vec<(String, C3, bool)> { self.must.iter().map(|m| (m.forms[form].clone(), m.ink, m.vital)).collect() };
            let wide = |tags: &[(String, C3, bool)]| tags.iter().map(|t| t.0.chars().count() + 2).sum::<usize>();
            // The name whole and every tag in full; failing that the tags in
            // fewer words ("+10 inside" as "+10"); failing that, with the
            // name cut as well, in the fewest.
            let mut vital = said(0);
            if full + wide(&vital) > budget {
                vital = said(1);
            }
            if name_min + wide(&vital) > budget {
                vital = said(2);
            }
            // Still too much: what is not vital goes first, latest first.
            while name_min + wide(&vital) > budget {
                match vital.iter().rposition(|t| !t.2) {
                    Some(i) => drop(vital.remove(i)),
                    None => break,
                }
            }
            // Only failures and running counts are left. The name gives way
            // to them, down to one character; and where even that is too
            // wide, the first of them is all the node says.
            let mut name_floor = 3;
            if !vital.is_empty() && name_min + wide(&vital) > budget {
                name_floor = 1;
                while vital.len() > 1 && first.min(1) + rest + wide(&vital) > budget {
                    vital.pop();
                }
                if first.min(1) + rest + wide(&vital) > budget {
                    return vital.first().map(|(t, ink, _)| (fit(t, budget), *ink, false)).into_iter().collect();
                }
            }
            let must: Vec<(String, C3)> = vital.into_iter().map(|(t, ink, _)| (t, ink)).collect();
            let len = |tags: &[(String, C3)]| tags.iter().map(|t| t.0.chars().count() + 2).sum::<usize>();
            let room = budget.saturating_sub(len(&must));
            if first + rest > room {
                segs[0].0 = fit(&segs[0].0, room.saturating_sub(rest).max(name_floor));
            }
            let mut used: usize = segs.iter().map(|s| s.0.chars().count()).sum::<usize>() + len(&must);
            let mut tail: Vec<(String, C3, bool)> = must.iter().map(|(t, ink)| (format!("  {t}"), *ink, false)).collect();
            for (t, ink) in &self.nice {
                let need = t.chars().count() + 2;
                if used + need <= budget {
                    used += need;
                    tail.push((format!("  {t}"), *ink, false));
                }
            }
            if !self.args.is_empty() && budget > used + 4 {
                segs.push((format!(" {}", fit(&self.args, (budget - used - 1).min(Self::ARGS_MAX))), args_ink, false));
            }
            segs.extend(tail);
            segs
        }
    }
    let labels: Vec<Label> = shown
        .iter()
        .map(|s| {
            let mut l = Label { name: Vec::new(), must: Vec::new(), nice: Vec::new(), args: String::new() };
            match &s.what {
                What::Proc { index, inside } => {
                    let p = &forest.procs[*index];
                    let st = forest.state[*index];
                    let ink = match st {
                        State::Running | State::Failed => pal.ink,
                        _ => pal.soft,
                    };
                    l.name.push((forest.label(*index), ink));
                    l.args = shorten(&args_of(p));
                    match (&p.end, st) {
                        (Some(e), State::Failed) => {
                            // "exit 0xC0000005" is, at its shortest, the code alone.
                            let (text, brief) = match e.code {
                                Some(c) if c >= 0x8000_0000 => (exit_text(c), format!("0x{c:08X}")),
                                Some(c) => (exit_text(c), exit_text(c)),
                                None => ("ended abnormally".to_string(), "abnormal".to_string()),
                            };
                            l.must.push(Must { forms: [text.clone(), text, brief], ink: pal.fail, vital: true });
                        }
                        (Some(e), State::Unknown) if e.lost => l.nice.push(("exit not seen".into(), pal.muted)),
                        (Some(_), State::Unknown) => l.nice.push(("exit code unknown".into(), pal.muted)),
                        _ => {}
                    }
                    if *inside > 0 {
                        let (fl, rn) = (forest.fails[*index] - (st == State::Failed) as u32, forest.runs[*index] - (st == State::Running) as u32);
                        // What is folded inside: how many, and of those what is
                        // running and what failed. At its shortest only the
                        // most notable of the three is left: a failure hidden
                        // in this node is never left unsaid for want of room.
                        let mut more = String::new();
                        if rn > 0 {
                            more.push_str(&format!(", {rn} running"));
                        }
                        if fl > 0 {
                            more.push_str(&format!(", {fl} failed"));
                        }
                        let brief = if fl > 0 { format!("{fl} failed") } else if rn > 0 { format!("{rn} running") } else { format!("+{inside}") };
                        l.must.push(Must {
                            forms: [format!("+{inside} inside{more}"), format!("+{inside}{more}"), brief],
                            ink: if fl > 0 { pal.fail } else if rn > 0 { pal.accent } else { pal.muted },
                            vital: fl > 0 || rn > 0,
                        });
                    }
                    // A life worth a second is worth saying; shorter ones are in the card.
                    if p.end.is_some() && p.duration_us(f.t1_us) >= 1_000_000 {
                        l.nice.push((procs::span(p.duration_us(f.t1_us)), pal.faint));
                    }
                }
                What::Group { class, name, members, .. } => {
                    let ink = if *class as usize == QUIET { pal.soft } else { pal.ink };
                    // A mix of kinds is "others"; which kinds is on its card.
                    l.name.push((name.clone().unwrap_or_else(|| "others".to_string()), ink));
                    l.name.push((format!(" \u{d7}{}", members.len()), ink));
                    let inside: u32 = members.iter().map(|&m| forest.desc[m as usize]).sum();
                    let fails: u32 = members.iter().map(|&m| forest.fails[m as usize]).sum();
                    let runs: u32 = members.iter().map(|&m| forest.runs[m as usize]).sum();
                    let all = members.len() as u32;
                    match *class as usize {
                        RUN if runs == all && inside == 0 => l.must.push(Must::one("running".into(), pal.accent, true)),
                        RUN => l.must.push(Must::one(format!("{runs} running"), pal.accent, true)),
                        FAIL if fails == all && inside == 0 => l.must.push(Must::one("failed".into(), pal.fail, true)),
                        FAIL => l.must.push(Must::one(format!("{fails} failed"), pal.fail, true)),
                        _ => l.nice.push(("exit 0".into(), pal.muted)),
                    }
                    if inside > 0 {
                        l.must.push(Must { forms: [format!("+{inside} inside"), format!("+{inside}"), format!("+{inside}")], ink: pal.muted, vital: false });
                    }
                }
            }
            l
        })
        .collect();

    // Columns: each as wide as its widest node wants, and when that is more
    // than the window has, the widest give way first.
    let room = (((ax1 - ax0 - gap * (cols - 1) as f32 - chrome * cols as f32) / sw).floor() as usize).max(cols * 4);
    let mut want = vec![0usize; cols];
    for (s, l) in shown.iter().zip(&labels) {
        if s.row < last_row {
            want[s.depth] = want[s.depth].max(l.want());
        }
    }
    let mut budget = want.clone();
    let mut order: Vec<usize> = (0..cols).collect();
    order.sort_by_key(|&d| want[d]);
    let mut left = room;
    for (k, &d) in order.iter().enumerate() {
        let share = left / (cols - k);
        budget[d] = want[d].min(share).max(4);
        left = left.saturating_sub(budget[d]);
    }
    let mut col_x = vec![ax0.round(); cols + 1];
    for d in 0..cols {
        col_x[d + 1] = col_x[d] + (chrome + budget[d] as f32 * sw).round() + gap;
    }
    struct Drawn {
        rect: [f32; 4],
        visible: bool,
    }
    let fitted: Vec<Vec<(String, C3, bool)>> = shown.iter().zip(&labels).map(|(s, l)| l.fit(budget[s.depth], pal.muted)).collect();
    let drawn: Vec<Drawn> = shown
        .iter()
        .zip(&fitted)
        .map(|(s, segs)| {
            let chars: usize = segs.iter().map(|s| s.0.chars().count()).sum();
            Drawn { rect: [col_x[s.depth], row_y(s.row), (chrome + chars as f32 * sw).round(), node_h], visible: s.row < last_row }
        })
        .collect();

    // Connectors first, so nodes sit on them: out of the parent, down a
    // trunk in the gap, and into each child.
    let line = u.round().max(1.0);
    for (pi, s) in shown.iter().enumerate() {
        let kids: Vec<usize> = (pi + 1..shown.len()).take_while(|&k| shown[k].depth > s.depth).filter(|&k| shown[k].parent == Some(pi)).collect();
        if kids.is_empty() || !drawn[pi].visible {
            continue;
        }
        let pr = drawn[pi].rect;
        let py = (pr[1] + (node_h - line) * 0.5).round();
        let tx = (col_x[s.depth + 1] - gap * 0.5).round();
        let class_of = |k: usize| match &shown[k].what {
            What::Proc { index, .. } => forest.class[*index],
            What::Group { class, .. } => *class,
        };
        let tint = |c: u8| match c as usize {
            RUN => (mix(pal.accent, pal.bg, 0.35), 1.0),
            FAIL => (mix(pal.fail, pal.bg, 0.45), 1.0),
            _ => (pal.hair, 1.0),
        };
        let vis: Vec<usize> = kids.iter().copied().filter(|&k| drawn[k].visible).collect();
        let Some(&last) = vis.last() else { continue };
        let ly = (drawn[last].rect[1] + (node_h - line) * 0.5).round();
        // The run out of the parent takes the colour of the most notable child.
        let lead = vis.iter().map(|&k| class_of(k)).min().unwrap_or(QUIET as u8);
        let (c, a) = tint(lead);
        sc.quad(pr[0] + pr[2], py, tx - (pr[0] + pr[2]) + line, line, c, a, 0.0);
        sc.quad(tx, py, line, ly - py + line, pal.hair, 1.0, 0.0);
        for class in [QUIET as u8, FAIL as u8, RUN as u8] {
            let deepest = vis.iter().copied().filter(|&k| class_of(k) == class).last();
            if let (Some(k), true) = (deepest, class as usize != QUIET) {
                let ky = (drawn[k].rect[1] + (node_h - line) * 0.5).round();
                let (c, a) = tint(class);
                sc.quad(tx, py, line, ky - py + line, c, a, 0.0);
            }
            for &k in vis.iter().filter(|&&k| class_of(k) == class) {
                let ky = (drawn[k].rect[1] + (node_h - line) * 0.5).round();
                let (c, a) = tint(class);
                sc.quad(tx, ky, drawn[k].rect[0] - tx, line, c, a, 0.0);
            }
        }
    }

    // Nodes.
    let radius = (node_h * 0.30).round();
    for (si, s) in shown.iter().enumerate() {
        if !drawn[si].visible {
            continue;
        }
        let [x, y, wd, hh] = drawn[si].rect;
        let (colour, strong, target, id) = match &s.what {
            What::Proc { index, .. } => {
                let st = forest.state[*index];
                (pal.of_state(st), st == State::Running || st == State::Failed, Target::Node(forest.procs[*index].id), Some(forest.procs[*index].id))
            }
            What::Group { class, members, .. } => {
                (pal.of_class(*class), (*class as usize) != QUIET, Target::Group(forest.procs[members[0] as usize].id, *class), None)
            }
        };
        let group = matches!(s.what, What::Group { .. });
        if group {
            // A stack: two more outlines behind, so a group reads as many.
            // It stays inside its own row however tightly rows are packed.
            let down = ((pitch - node_h - u) * 0.5).clamp(u, 3.0 * u);
            for k in [2.0f32, 1.0] {
                let (ox, oy) = ((k * 3.0 * u).round(), (k * down).round());
                sc.quad(x + ox, y + oy, wd, hh, pal.bg, 1.0, radius);
                sc.ring(x + ox, y + oy, wd, hh, colour, if strong { 0.75 } else { 0.50 } / k, radius, line);
            }
        }
        sc.quad(x, y, wd, hh, pal.bg, 1.0, radius);
        sc.quad(x, y, wd, hh, colour, if strong { 0.13 } else { 0.05 }, radius);
        sc.ring(x, y, wd, hh, colour, if strong { 0.95 } else { 0.60 }, radius, if strong { (1.5 * u).round().max(1.0) } else { line });
        // The state mark: filled while running or failed, hollow once it ended well.
        let (dx, dy) = (x + pad_in, y + ((hh - dot) * 0.5).round());
        if strong {
            sc.quad(dx, dy, dot, dot, colour, 1.0, dot * 0.5);
        } else {
            sc.ring(dx, dy, dot, dot, colour, 0.9, dot * 0.5, line);
        }
        // Label.
        let born = id.and_then(|id| when(f.born, id));
        let mut tx = dx + dot + (0.7 * sw).round();
        let ty_ = y + ((hh - sh) * 0.5).round();
        for (text, ink, bold) in &fitted[si] {
            let before = sc.texts.len();
            tx = sc.text(&ty, tx, ty_, text, *ink, Size::Small, *bold);
            if let (Some(t), Some(last)) = (born, sc.texts.get_mut(before)) {
                // The label unfolds left to right across its pieces.
                last.born = Some(t + (((last.x - x) / sw).max(0.0) as f64 * 0.0032).min(0.16));
            }
        }
        if let Some(t) = born {
            sc.last_birth = Some(sc.last_birth.map_or(t, |b: f64| b.max(t)));
        }
        if f.hover == Some(target) || matches!((f.hover, id), (Some(Target::Span(a)), Some(b)) if a == b) {
            sc.over.push(Quad { x: x - 2.0 * u, y: y - 2.0 * u, w: wd + 4.0 * u, h: hh + 4.0 * u, color: pal.ink, alpha: 0.9, radius: radius + 2.0 * u, ring: line });
        }
        sc.hits.push(Hit { rect: [x, y - ((pitch - node_h) * 0.5).floor(), wd, pitch], target });
        if group {
            sc.counts.groups += 1;
        } else {
            sc.counts.nodes += 1;
        }
    }
    sc.counts.folded = n - shown.iter().filter(|s| matches!(s.what, What::Proc { .. })).count();

    // Light: a process that has just started lights the node it is in (its
    // own, or the group it was counted into); one that has just failed, in red.
    for (list, colour, energy) in [(f.born, pal.accent, EMIT_BORN), (f.failed, pal.fail, EMIT_FAILED)] {
        for &(id, t) in list {
            let Some((_, si)) = node_of(id) else { continue };
            if !drawn[si as usize].visible {
                continue;
            }
            let [x, y, wd, hh] = drawn[si as usize].rect;
            sc.emits.push(Emit { x, y, w: wd, h: hh, color: colour, born: t, energy });
            sc.last_birth = Some(sc.last_birth.map_or(t, |b: f64| b.max(t)));
        }
    }

    if cut {
        // Said on screen, with real counts: nothing is dropped silently.
        let hidden: usize = shown.iter().filter(|s| s.row >= last_row).map(|s| match &s.what {
            What::Proc { inside, .. } => 1 + inside,
            What::Group { members, .. } => members.iter().map(|&m| 1 + forest.desc[m as usize] as usize).sum(),
        }).sum();
        let msg = format!("{} more rows ({} processes) do not fit this window; they are on the timeline below", sc.counts.rows_cut, hidden);
        sc.text(&ty, ax0, row_y(last_row), &fit(&msg, ((ax1 - ax0) / sw) as usize), pal.muted, Size::Small, false);
    }
    shown
}

/// The hover card: everything known about what the mouse is over.
fn card(sc: &mut Scene, f: &Frame, forest: &Forest, pal: &Palette, shown: &[Shown], target: Target, bounds: [f32; 4]) {
    let ty = f.ty;
    let u = ty.unit.max(0.5);
    let (sw, sh) = (ty.w(Size::Small), ty.h(Size::Small));
    let Some(anchor) = sc.hits.iter().rev().find(|h| h.target == target).map(|h| h.rect) else { return };
    // No line of the card is wider than this, so the card is never wider
    // than the window: about three fifths of it, and in a window too narrow
    // for 24 characters, all of it.
    let pad = (0.9 * sw).round();
    let widest = (((bounds[2] - bounds[0] - 2.0 * pad) / sw).floor() as usize).max(8);
    let max_chars = ((((bounds[2] - bounds[0]) * 0.62) / sw).floor() as usize).clamp(24, 110).min(widest);
    // And no more lines than the window is tall.
    let max_rows = (((bounds[3] - bounds[1] - 2.0 * (0.35 * sh).round()) / sh).floor() as usize).max(4);
    let since = |t: u64| format!("+{}", procs::span(t.saturating_sub(f.t0_us)));
    let mut lines: Vec<(String, C3, bool)> = Vec::new();
    let wrap = |s: &str, ink: C3, limit: usize, lines: &mut Vec<(String, C3, bool)>| {
        let width = max_chars.max(8);
        let mut rows: Vec<String> = vec![String::new()];
        for word in s.split(' ') {
            let mut word: Vec<char> = word.chars().collect();
            loop {
                let row = rows.last_mut().unwrap();
                let have = row.chars().count();
                let need = word.len() + (have > 0) as usize;
                if have + need <= width {
                    if have > 0 {
                        row.push(' ');
                    }
                    row.extend(word.iter());
                    break;
                }
                if word.len() > width {
                    // Longer than a whole row: fill this row and carry on.
                    let take = width.saturating_sub(have + (have > 0) as usize);
                    if take >= 8 {
                        if have > 0 {
                            row.push(' ');
                        }
                        row.extend(word.drain(..take));
                    }
                }
                rows.push(String::new());
            }
        }
        let total = rows.len();
        for (k, row) in rows.into_iter().enumerate() {
            if k + 1 == limit && total > limit {
                let cut: String = row.chars().take(width - 1).collect();
                lines.push((format!("{cut}…"), ink, false));
                lines.push((format!("({} characters in all)", s.chars().count()), pal.faint, false));
                break;
            }
            lines.push((row, ink, false));
        }
    };
    let describe = |i: usize| -> (String, C3) {
        let p = &forest.procs[i];
        match (&p.end, forest.state[i]) {
            (None, _) => (format!("running, {} so far", procs::span(p.duration_us(f.t1_us))), pal.accent),
            (Some(e), State::Failed) => (format!("{}, lived {}", e.code.map_or("ended abnormally".into(), exit_text), procs::span(p.duration_us(f.t1_us))), pal.fail),
            (Some(_), State::Ok) => (format!("exit 0, lived {}", procs::span(p.duration_us(f.t1_us))), pal.ok),
            (Some(e), _) => (format!("{}, lived {}", if e.lost { "exit not seen" } else { "exit code unknown" }, procs::span(p.duration_us(f.t1_us))), pal.muted),
        }
    };
    match target {
        Target::Node(id) | Target::Span(id) => {
            let Some(i) = forest.index_of(id) else { return };
            let p = &forest.procs[i];
            lines.push((fit(&forest.label(i), max_chars), pal.ink, true));
            // What follows the command line, first: it says how many rows
            // are left for the command line itself.
            let mut tail: Vec<(String, C3, bool)> = Vec::new();
            let (text, ink) = describe(i);
            wrap(&text, ink, usize::MAX, &mut tail);
            let mut facts = format!("pid {}, started {}", p.pid, since(p.start_us));
            if let Some(cpu) = p.cpu_us() {
                facts.push_str(&format!(", cpu {}", procs::span(cpu)));
            }
            if forest.desc[i] > 0 {
                facts.push_str(&format!(", {} beneath it", forest.desc[i]));
                let own = (forest.state[i] == State::Failed) as u32;
                if forest.fails[i] > own {
                    facts.push_str(&format!(" ({} failed)", forest.fails[i] - own));
                }
            }
            wrap(&facts, pal.muted, usize::MAX, &mut tail);
            // The whole command line, as far as the window can hold a card:
            // every row but the ones the other lines need (and one more for
            // saying how long it is, if it has to be cut).
            let cmd_rows = max_rows.saturating_sub(lines.len() + tail.len() + 1).max(1);
            match p.cmd.as_deref().map(one_line) {
                Some(c) if !c.is_empty() => wrap(&c, pal.soft, cmd_rows, &mut lines),
                Some(_) => wrap("(empty command line)", pal.faint, usize::MAX, &mut lines),
                None => wrap("(command line could not be read: it ended first)", pal.faint, usize::MAX, &mut lines),
            }
            lines.extend(tail);
        }
        Target::Group(first, class) => {
            let (members, kinds): (Vec<usize>, usize) = shown
                .iter()
                .find_map(|s| match &s.what {
                    What::Group { class: c, members, kinds, .. } if *c == class && forest.procs[members[0] as usize].id == first => Some((members.iter().map(|&m| m as usize).collect(), *kinds)),
                    _ => None,
                })
                .unwrap_or_default();
            if members.is_empty() {
                return;
            }
            let fails: u32 = members.iter().map(|&m| forest.fails[m]).sum();
            let runs: u32 = members.iter().map(|&m| forest.runs[m]).sum();
            let inside: u32 = members.iter().map(|&m| forest.desc[m]).sum();
            wrap(&format!("{} siblings counted together", members.len()), pal.ink, usize::MAX, &mut lines);
            if let Some(l) = lines.first_mut() {
                l.2 = true;
            }
            if kinds > 1 {
                // A mixed group says what it is a mix of, most numerous first.
                let mut of: Vec<(u32, usize)> = Vec::new();
                for &m in &members {
                    match of.iter_mut().find(|k| k.0 == forest.name[m]) {
                        Some(k) => k.1 += 1,
                        None => of.push((forest.name[m], 1)),
                    }
                }
                of.sort_by(|a, b| b.1.cmp(&a.1));
                let list: Vec<String> = of.iter().map(|&(name, count)| format!("{} ×{count}", match forest.names[name as usize] { "" => "unnamed", s => s })).collect();
                lines.push((fit(&list.join(", "), max_chars), pal.soft, false));
            }
            let mut facts = match class as usize {
                RUN => format!("{runs} running"),
                FAIL => format!("{fails} failed"),
                _ => "all exit 0".to_string(),
            };
            if inside > 0 {
                facts.push_str(&format!(", {inside} more beneath them"));
            }
            wrap(&facts, pal.of_class(class), usize::MAX, &mut lines);
            let first_t = members.iter().map(|&m| forest.procs[m].start_us).min().unwrap_or(f.t0_us);
            let last_t = members.iter().map(|&m| forest.procs[m].end.as_ref().map_or(f.t1_us, |e| e.end_us)).max().unwrap_or(f.t1_us);
            let cpu: u64 = members.iter().filter_map(|&m| forest.procs[m].cpu_us()).sum();
            wrap(&format!("first started {}, last ended {}, cpu {}", since(first_t), since(last_t), procs::span(cpu)), pal.muted, usize::MAX, &mut lines);
            // The most recent few, so a group is not a closed box: four, or
            // as many as the window has rows left for (and one row to say
            // how many came before them).
            let recent = max_rows.saturating_sub(lines.len() + 1).min(4);
            for &m in members.iter().rev().take(recent).rev() {
                let p = &forest.procs[m];
                let c = p.cmd.as_deref().map(one_line).unwrap_or_else(|| forest.label(m));
                let tag = match forest.state[m] {
                    State::Failed => p.exit_code().map_or("failed".into(), exit_text),
                    State::Running => "running".into(),
                    State::Ok => "exit 0".into(),
                    State::Unknown => "exit unknown".into(),
                };
                lines.push((fit(&format!("{tag}  {c}"), max_chars), pal.soft, false));
            }
            if members.len() > recent {
                lines.push((fit(&format!("and {} earlier", members.len() - recent), max_chars), pal.faint, false));
            }
        }
    }
    let chars = lines.iter().map(|l| l.0.chars().count()).max().unwrap_or(0);
    let (cw, ch) = (chars as f32 * sw + 2.0 * pad, lines.len() as f32 * sh + 2.0 * (0.35 * sh).round());
    // Under what it describes; above it when there is no room under; and
    // when it fits neither way, wherever it is whole.
    let (below, above) = (anchor[1] + anchor[3] + (3.0 * u).round(), anchor[1] - ch - (3.0 * u).round());
    let x = anchor[0].round().min(bounds[2] - cw).max(bounds[0]);
    let y = if below + ch <= bounds[3] {
        below
    } else if above >= bounds[1] {
        above
    } else {
        (bounds[3] - ch).max(bounds[1])
    }
    .round();
    let panel = mix(pal.bg, pal.ink, 0.075);
    sc.quad(x, y, cw, ch, panel, 1.0, 5.0 * u);
    sc.ring(x, y, cw, ch, pal.faint, 0.8, 5.0 * u, u.round().max(1.0));
    sc.clip = Some([x, y, cw, ch]);
    // Outlines are drawn over text; none may cross the card.
    sc.over.retain(|q| q.x + q.w <= x || q.x >= x + cw || q.y + q.h <= y || q.y >= y + ch);
    for (k, (text, ink, bold)) in lines.iter().enumerate() {
        sc.text(&ty, x + pad, y + (0.35 * sh).round() + k as f32 * sh, text, *ink, Size::Small, *bold);
        if let Some(t) = sc.texts.last_mut() {
            t.top = true;
        }
    }
}

// --------------------------------------------------------------- journal

/// What a journal says happened, on one clock.
pub struct Scan {
    pub meta: Value,
    pub ledger: Ledger,
    pub activity: Activity,
    /// The session's own end, if the journal records it.
    pub ended: Option<Option<u32>>,
    /// Unix time (microseconds) of the journal's time zero.
    pub epoch_us: u64,
    /// True when the epoch comes from process records (to well under a
    /// millisecond); false when only the header's whole second was known.
    pub epoch_exact: bool,
    /// Time of the last record read, on the Unix clock.
    pub last_us: u64,
    /// Processes that started / failed, with the journal time (seconds) at
    /// which the record was written.
    pub born: Vec<(u64, f64)>,
    pub failed: Vec<(u64, f64)>,
    pub records: usize,
}

/// Read a journal's process records and the size and time of each piece of
/// output. Output itself is skipped over, not loaded. With `until_us`,
/// records written later than that many microseconds in are left out: the
/// session as it stood at that moment.
pub fn scan_journal(path: &Path, until_us: Option<u64>) -> io::Result<Scan> {
    use crate::journal::{K_EXIT, K_OUTPUT, K_PROC, MAGIC};
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut r = BufReader::with_capacity(1 << 16, std::fs::File::open(path)?);
    let mut head = [0u8; 12];
    r.read_exact(&mut head).map_err(|_| bad("not a PROTEUS journal"))?;
    if &head[..8] != MAGIC {
        return Err(bad("not a PROTEUS journal"));
    }
    let mut meta = vec![0u8; u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize];
    r.read_exact(&mut meta).map_err(|_| bad("journal header truncated"))?;
    let meta: Value = serde_json::from_slice(&meta).unwrap_or(Value::Null);

    enum Rec {
        Out(u64),
        Proc(Box<ProcEvent>),
        Exit(u32),
    }
    let mut recs: Vec<(u64, Rec)> = Vec::new();
    let mut epoch: Option<u64> = None;
    let mut rec = [0u8; 13];
    // A record cut short by a crash ends the read; what came before stands.
    while r.read_exact(&mut rec).is_ok() {
        let t_us = u64::from_le_bytes(rec[1..9].try_into().unwrap());
        let len = u32::from_le_bytes(rec[9..13].try_into().unwrap()) as usize;
        match rec[0] {
            K_PROC | K_EXIT => {
                let mut data = vec![0u8; len];
                if r.read_exact(&mut data).is_err() {
                    break;
                }
                if rec[0] == K_EXIT {
                    recs.push((t_us, Rec::Exit(data.get(..4).map_or(u32::MAX, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])))));
                } else if let Some(ev) = ProcEvent::from_bytes(&data) {
                    // Written just after the sensor saw the event: the largest
                    // difference is the closest estimate of time zero.
                    epoch = epoch.max(Some(ev.seen_us().saturating_sub(t_us)));
                    recs.push((t_us, Rec::Proc(Box::new(ev))));
                }
            }
            kind => {
                if r.seek_relative(len as i64).is_err() {
                    break;
                }
                if kind == K_OUTPUT {
                    recs.push((t_us, Rec::Out(len as u64)));
                }
            }
        }
    }
    let epoch_exact = epoch.is_some();
    let epoch_us = epoch.unwrap_or_else(|| meta["started_unix"].as_u64().unwrap_or(0) * SEC);
    let limit = until_us.unwrap_or(u64::MAX);
    let mut out = Scan { meta, ledger: Ledger::new(), activity: Activity::default(), ended: None, epoch_us, epoch_exact, last_us: epoch_us, born: Vec::new(), failed: Vec::new(), records: 0 };
    for (t_us, rec) in recs {
        if t_us > limit {
            break;
        }
        out.records += 1;
        out.last_us = out.last_us.max(epoch_us + t_us);
        match rec {
            Rec::Out(len) => out.activity.add(epoch_us + t_us, len),
            Rec::Exit(code) => out.ended = Some((code != u32::MAX).then_some(code)),
            Rec::Proc(ev) => {
                let id = out.ledger.apply(&ev);
                let at = t_us as f64 / 1e6;
                match &*ev {
                    ProcEvent::Start(_) => out.born.push((id, at)),
                    ProcEvent::Exit(_) => {
                        if out.ledger.get(id).is_some_and(|p| state(p) == State::Failed) {
                            out.failed.push((id, at));
                        }
                    }
                }
            }
        }
    }
    out.born.sort_by(|a, b| a.0.cmp(&b.0));
    out.born.dedup_by_key(|e| e.0);
    out.failed.sort_by(|a, b| a.0.cmp(&b.0));
    out.failed.dedup_by_key(|e| e.0);
    Ok(out)
}

// ------------------------------------------------------------------ live

/// Keys the view answers to (the window maps real keys to these).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Close,
    Left,
    Right,
    In,
    Out,
    Fit,
    Latest,
}

/// How long an arrival is remembered as one. After this it is drawn settled,
/// which looks the same: the reveal and its light are long over.
const ARRIVAL_S: f64 = 3.0;

/// The view's state between events: what a window keeps so that the next
/// picture can be built. It holds no ledger (the window already has one).
#[derive(Default)]
pub struct Live {
    pub shown: bool,
    /// Something changed that the picture depends on.
    pub dirty: bool,
    pub activity: Activity,
    t0_us: Option<u64>,
    t1_us: u64,
    pub session: String,
    pub ended: Option<Option<u32>>,
    zoom: Option<(u64, u64)>,
    /// The zoomed range is following the latest event.
    follow: bool,
    /// The last key press showed or hid the view (see `repeat_of_toggle`).
    just_toggled: bool,
    mouse: Option<(f32, f32)>,
    hover: Option<Target>,
    born: Vec<(u64, f64)>,
    failed: Vec<(u64, f64)>,
    /// Output bytes accounted for: placed on the timeline, or known to be
    /// history that is waiting to be.
    placed: u64,
    /// Bytes that arrived as replayed history, and when the replay ended.
    history: Option<(u64, u64)>,
    /// Replayed bytes whose times are not known (yet).
    pub unplaced: u64,
    /// From the last picture: where things are, for the mouse.
    hits: Vec<Hit>,
    axis: Option<Axis>,
    pub builds: u64,
    pub build_us_total: u64,
    pub build_us_max: u64,
}

impl Live {
    pub fn new(session: &str) -> Self {
        Live { session: session.to_string(), ..Default::default() }
    }

    /// A view of a finished recording.
    pub fn from_scan(scan: &Scan, session: &str) -> Self {
        let mut l = Live::new(session);
        l.activity = scan.activity.clone();
        l.placed = scan.activity.total;
        l.t0_us = [Some(scan.epoch_us), scan.ledger.procs().iter().map(|p| p.start_us).min()].into_iter().flatten().min();
        l.t1_us = scan.last_us;
        l.ended = scan.ended;
        l.born = scan.born.clone();
        l.failed = scan.failed.clone();
        l.dirty = true;
        l
    }

    fn saw(&mut self, t_us: u64) {
        self.t0_us = Some(self.t0_us.map_or(t_us, |t| t.min(t_us)));
        if t_us > self.t1_us {
            // A zoomed range that was showing the latest moment keeps showing it.
            if let (Some((lo, hi)), true) = (self.zoom, self.follow) {
                let d = t_us - hi.min(t_us);
                self.zoom = Some((lo + d, hi + d));
            }
            self.t1_us = t_us;
        }
        self.dirty = true;
    }

    /// A process event was folded into the ledger as process `id`. `live` is
    /// false for history, which is placed but does not arrive. `now` is the
    /// frame clock in seconds.
    pub fn on_proc(&mut self, ledger: &Ledger, id: u64, ev: &ProcEvent, live: bool, now: f64) {
        let Some(p) = ledger.get(id) else { return };
        match ev {
            ProcEvent::Start(s) => {
                self.saw(p.start_us);
                self.saw(s.seen_us.max(p.start_us));
                if live {
                    arrive(&mut self.born, id, now);
                }
            }
            ProcEvent::Exit(e) => {
                self.saw(p.start_us);
                self.saw(e.seen_us.max(p.end.as_ref().map_or(0, |x| x.end_us)));
                if live && state(p) == State::Failed {
                    arrive(&mut self.failed, id, now);
                }
            }
        }
    }

    /// The session has printed `total` bytes in all by `now_us`.
    pub fn on_output(&mut self, total: u64, now_us: u64) {
        if total > self.placed {
            self.activity.add(now_us, total - self.placed);
            self.placed = total;
            self.saw(now_us);
        }
    }

    /// A reattached session's history has been replayed: `total` bytes have
    /// arrived, none of them printed just now. They are not drawn as a burst
    /// at the moment of attaching; `load_history` places them where the
    /// journal says they belong.
    pub fn on_replayed(&mut self, total: u64, now_us: u64) {
        if total > self.placed {
            let bytes = total - self.placed;
            self.history = Some((self.history.map_or(0, |h| h.0) + bytes, now_us));
            self.unplaced += bytes;
            self.placed = total;
            self.dirty = true;
        }
    }

    /// Place replayed history on the timeline, from the journal's own record
    /// times. The journal and the window count the same stream, so the first
    /// bytes of the journal are exactly the bytes that were replayed. Without
    /// a journal this window can read (or with one whose clock is only known
    /// to the second) they stay off the timeline, and the view says so.
    pub fn load_history(&mut self, journal: Option<&Path>) {
        let (Some(_), Some(path)) = (self.history, journal) else { return };
        let Ok(scan) = scan_journal(path, None) else { return };
        if !scan.epoch_exact {
            return;
        }
        self.place_history(&scan.activity, scan.epoch_us);
    }

    /// The placing itself: the first bytes of `recorded` (a journal's output,
    /// slice by slice) are the bytes that were replayed to this window.
    fn place_history(&mut self, recorded: &Activity, epoch_us: u64) {
        let Some((bytes, at_us)) = self.history.take() else { return };
        let mut left = bytes;
        let mut old: Vec<(u64, u64)> = Vec::new();
        for &(i, b) in &recorded.bins {
            if left == 0 {
                break;
            }
            let take = b.min(left);
            old.push((i * recorded.bin_us, take));
            left -= take;
        }
        // What the file did not hold yet had arrived by the end of the replay.
        if left > 0 {
            old.push((at_us, left));
        }
        // All of it in one pass. Added slice by slice, each would be put in
        // ahead of everything this window has seen arrive since it attached,
        // and a long history under a long-lived window would cost the
        // product of the two: seconds, on the window's own thread.
        self.activity.add_sorted(&old);
        if let (Some(first), Some(last)) = (old.first(), old.last()) {
            self.saw(first.0.max(epoch_us));
            self.saw(last.0.max(epoch_us));
        }
        self.unplaced -= bytes.min(self.unplaced);
        self.dirty = true;
    }

    pub fn on_exit(&mut self, code: Option<u32>, now_us: u64) {
        self.ended = Some(code);
        self.saw(now_us);
    }

    /// A key went down (`repeat`: it is being held, and this is the keyboard
    /// repeating it). True when the press must be dropped altogether: it is
    /// the repeat of the press that has just shown or hidden the view. Held
    /// a moment too long, that key would otherwise flip the view back and
    /// forth; and a held Esc, having closed the view, would be typed into
    /// the program underneath, where it can interrupt what is running.
    pub fn repeat_of_toggle(&mut self, repeat: bool) -> bool {
        if repeat && self.just_toggled {
            return true;
        }
        self.just_toggled = false;
        false
    }

    /// Show or hide the view. Returns whether it is now shown.
    pub fn toggle(&mut self) -> bool {
        self.just_toggled = true;
        self.shown = !self.shown;
        self.dirty = true;
        self.hover = None;
        self.mouse = None;
        self.shown
    }

    fn full(&self) -> (u64, u64) {
        let t0 = self.t0_us.unwrap_or(self.t1_us);
        (t0, self.t1_us.max(t0 + 1))
    }

    fn set_zoom(&mut self, z: Option<(u64, u64)>) -> bool {
        let z = z.filter(|&(lo, hi)| hi > lo && (lo, hi) != self.full());
        self.follow = z.is_some_and(|(_, hi)| hi >= self.t1_us);
        if z == self.zoom {
            return false;
        }
        self.zoom = z;
        self.dirty = true;
        true
    }

    /// Show `lo_s..hi_s` seconds of the session (for `shot --zoom`).
    pub fn zoom_to(&mut self, lo_s: f64, hi_s: f64) {
        let (t0, t1) = self.full();
        let at = |s: f64| (t0 + (s.max(0.0) * 1e6) as u64).min(t1);
        self.set_zoom(Some((at(lo_s), at(hi_s))));
    }

    /// The wheel turned by `lines` (positive = away from the person = closer)
    /// with the mouse at `x`. Returns whether the picture changed.
    pub fn wheel(&mut self, lines: f32, x: f32) -> bool {
        let Some(axis) = self.axis else { return false };
        if lines == 0.0 {
            return false;
        }
        let z = zoomed(self.full(), self.zoom, axis.t(x), 0.8f64.powf(lines as f64));
        self.set_zoom(z)
    }

    /// Returns whether the picture changed. `Close` hides the view.
    pub fn key(&mut self, key: Key) -> bool {
        let full = self.full();
        let mid = self.zoom.map_or((full.0 + full.1) / 2, |(lo, hi)| (lo + hi) / 2);
        match key {
            Key::Close => {
                self.toggle();
                true
            }
            Key::Left => self.set_zoom(panned(full, self.zoom, -0.2)),
            Key::Right => self.set_zoom(panned(full, self.zoom, 0.2)),
            Key::In => self.set_zoom(zoomed(full, self.zoom, mid, 0.8)),
            Key::Out => self.set_zoom(zoomed(full, self.zoom, mid, 1.25)),
            Key::Fit => self.set_zoom(None),
            Key::Latest => {
                let z = self.zoom.map(|(lo, hi)| (full.1 - (hi - lo), full.1));
                self.set_zoom(z)
            }
        }
    }

    /// The mouse moved. Returns whether the picture changed: only when it is
    /// now over something else, never for movement within one thing.
    pub fn mouse(&mut self, x: f32, y: f32) -> bool {
        self.mouse = Some((x, y));
        let over = self.hits.iter().rev().find(|h| x >= h.rect[0] && x < h.rect[0] + h.rect[2] && y >= h.rect[1] && y < h.rect[1] + h.rect[3]).map(|h| h.target);
        if over == self.hover {
            return false;
        }
        self.hover = over;
        self.dirty = true;
        true
    }

    pub fn hover(&self) -> Option<Target> {
        self.hover
    }

    /// Build the picture for the current state, and remember where things
    /// are. `now` (frame clock, seconds) only decides which arrivals are old
    /// enough to forget; nothing drawn is computed from it.
    pub fn scene(&mut self, ledger: &Ledger, ty: Type, size: [f32; 2], pad: f32, theme: &Theme, now: f64) -> Scene {
        let t = std::time::Instant::now();
        self.born.retain(|e| now - e.1 < ARRIVAL_S);
        self.failed.retain(|e| now - e.1 < ARRIVAL_S);
        let mut sc = self.build(ledger, ty, size, pad, theme);
        // The picture may have moved under a still mouse.
        if let Some((x, y)) = self.mouse {
            let over = sc.target_at(x, y);
            if over != self.hover {
                self.hover = over;
                sc = self.build(ledger, ty, size, pad, theme);
            }
        }
        self.hits = sc.hits.clone();
        self.axis = sc.axis;
        self.dirty = false;
        let us = t.elapsed().as_micros() as u64;
        self.builds += 1;
        self.build_us_total += us;
        self.build_us_max = self.build_us_max.max(us);
        sc
    }

    fn build(&self, ledger: &Ledger, ty: Type, size: [f32; 2], pad: f32, theme: &Theme) -> Scene {
        let (t0, t1) = self.full();
        build(&Frame {
            ledger,
            activity: &self.activity,
            t0_us: t0,
            t1_us: t1.max(self.t1_us),
            session: &self.session,
            ended: self.ended,
            unplaced: self.unplaced,
            zoom: self.zoom,
            hover: self.hover,
            born: &self.born,
            failed: &self.failed,
            size,
            pad,
            ty,
            theme,
        })
    }
}

fn arrive(list: &mut Vec<(u64, f64)>, id: u64, now: f64) {
    // Old arrivals are forgotten here as well as when a picture is built: a
    // window that never opens the view must not keep them for ever.
    list.retain(|e| now - e.1 < ARRIVAL_S);
    match list.binary_search_by(|e| e.0.cmp(&id)) {
        Ok(i) => list[i].1 = now,
        Err(i) => list.insert(i, (id, now)),
    }
}

#[cfg(test)]
#[path = "world_tests.rs"]
mod tests;
