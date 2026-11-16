//! Scrubbing: the session's past, shown in place.
//!
//! The past screen takes the grid minus three rows; the bar under it is drawn
//! by the engine from the journal alone. Every mark on it is something that
//! happened: bar heights are bytes the session printed in that slice of its
//! life (log scale), the playhead is the moment on screen, the tick at the
//! right is the exit code if it has ended.
//!
//! Seeks run on a worker thread so a long journal being indexed for the
//! first time never stalls the window; the worker sleeps on a channel, so
//! scrubbing costs nothing while nothing is asked of it.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::term::Term;
use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::fonts::CellMetrics;
use crate::recall::{clock, Index, Target, Timeline};
use crate::renderer::{Overlay, OverlayRect, OverlayText};
use crate::term::{Listener, Notify, UiEvent};
use crate::theme::{mix, Theme, C3};

/// Grid rows the bar takes from the bottom of the window.
pub const BAR_ROWS: usize = 3;

/// "850 ms", "12.3 s", "3m 12s", "1h 02m".
pub fn human(us: u64) -> String {
    let s = us / 1_000_000;
    match us {
        0..=999_999 => format!("{} ms", us / 1000),
        1_000_000..=59_999_999 => format!("{:.1} s", us as f64 / 1e6),
        _ if s < 3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s / 60) % 60),
    }
}

/// Local date and time of a Unix timestamp, from the system's own rules.
pub fn local_time(unix_secs: u64) -> Option<String> {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        ms: u16,
    }
    extern "system" {
        fn FileTimeToSystemTime(ft: *const u64, st: *mut SystemTime) -> i32;
        fn SystemTimeToTzSpecificLocalTime(tz: *const core::ffi::c_void, utc: *const SystemTime, local: *mut SystemTime) -> i32;
    }
    // Windows counts 100 ns steps from 1601.
    let ft = unix_secs.checked_add(11_644_473_600)?.checked_mul(10_000_000)?;
    let (mut utc, mut local) = (SystemTime::default(), SystemTime::default());
    let ok = unsafe { FileTimeToSystemTime(&ft, &mut utc) != 0 && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0 };
    ok.then(|| format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", local.year, local.month, local.day, local.hour, local.minute, local.second))
}

/// What the bar shows. All of it comes from the journal.
pub struct BarData<'a> {
    /// Output bytes in each equal slice of the session's life.
    pub hist: &'a [u64],
    pub duration_us: u64,
    /// The moment on screen.
    pub t_us: u64,
    /// When the session started (from the journal's header).
    pub started_unix: Option<u64>,
    /// Which output record is on screen, of how many.
    pub output: usize,
    pub outputs: usize,
    pub exit: Option<u32>,
    /// A line about work in progress ("indexing 43%"), if any.
    pub note: Option<&'a str>,
    /// The past screen asked for has not arrived yet: what is above the bar
    /// is still the present, and the bar must not call it the past.
    pub waiting: bool,
}

/// Where the histogram sits and how many bars fit: (left, width, bars, pitch).
pub fn track(screen_w: f32, pad_x: f32, cell_w: f32) -> (f32, f32, usize, f32) {
    let pitch = (cell_w * 0.3).round().max(3.0);
    let w = (screen_w - 2.0 * pad_x).max(pitch);
    (pad_x, w, (w / pitch).floor().max(1.0) as usize, pitch)
}

/// Y of the top of the bar's band.
pub fn band_top(pad_y: f32, cell_h: f32, grid_rows: usize) -> f32 {
    pad_y + grid_rows.saturating_sub(BAR_ROWS) as f32 * cell_h
}

/// The moment a horizontal pixel position on the bar stands for.
pub fn time_at_x(x: f32, screen_w: f32, pad_x: f32, cell_w: f32, duration_us: u64) -> u64 {
    let (x0, w, _, _) = track(screen_w, pad_x, cell_w);
    (((x - x0) / w).clamp(0.0, 1.0) as f64 * duration_us as f64) as u64
}

fn rect(pos: [f32; 2], size: [f32; 2], color: C3, alpha: f32) -> OverlayRect {
    OverlayRect { pos, size, color, alpha, radius: 0.0, glow: 0.0 }
}

/// The scrub bar as shapes and text.
pub fn bar(screen: [f32; 2], pad: [f32; 2], m: &CellMetrics, th: &Theme, grid_rows: usize, d: &BarData) -> Overlay {
    let mut o = Overlay::default();
    let (cw, ch) = (m.cell_w, m.cell_h);
    let px = (ch / 22.0).round().max(1.0); // one "line" at this scale
    let y0 = band_top(pad[1], ch, grid_rows);
    let (x0, w, n, pitch) = track(screen[0], pad[0], cw);
    let frac = if d.duration_us == 0 { 1.0 } else { (d.t_us as f64 / d.duration_us as f64).clamp(0.0, 1.0) as f32 };

    // The band: the ground lifted a little toward the accent, and a hairline.
    let top = y0 + (ch * 0.06).round();
    o.rects.push(rect([0.0, top], [screen[0], screen[1] - top], mix(th.bg, th.accent, 0.045), 1.0));
    o.rects.push(rect([0.0, top], [screen[0], px], th.accent, 0.30));

    // Output over the session's life. Log scale, counted in short lines (64
    // bytes): a keystroke's echo and a megabyte of build log are both worth
    // seeing, and a flood still stands taller than a prompt.
    let base = (y0 + ch * 1.86).round();
    let tall = ch * 1.42;
    let peak = d.hist.iter().copied().max().unwrap_or(0).max(1) as f32 / 64.0;
    let at = ((frac * n as f32) as usize).min(n - 1);
    let gap = (pitch * 0.25).round().max(1.0);
    for i in 0..n.min(d.hist.len()) {
        let v = d.hist[i];
        if v == 0 {
            continue;
        }
        let h = ((v as f32 / 64.0).ln_1p() / peak.ln_1p() * tall).max(px * 2.0).round();
        let (color, alpha) = match i.cmp(&at) {
            std::cmp::Ordering::Less => (th.accent, 0.80),
            std::cmp::Ordering::Equal => (th.fg, 1.0),
            std::cmp::Ordering::Greater => (th.fg, 0.24),
        };
        o.rects.push(rect([x0 + i as f32 * pitch, base - h], [pitch - gap, h], color, alpha));
    }
    o.rects.push(rect([x0, base], [w, px], th.fg, 0.16));
    if let Some(code) = d.exit {
        // The session ended: a tick in the colour of how.
        let c = if code == 0 { th.ansi[2] } else { th.ansi[1] };
        o.rects.push(rect([x0 + w - px * 3.0, base - ch * 0.55], [px * 3.0, ch * 0.55 + px], c, 1.0));
    }

    // The playhead.
    let xp = (x0 + frac * w).round().clamp(x0, x0 + w - px * 2.0);
    let (head_top, head_h) = (y0 + ch * 0.22, base - (y0 + ch * 0.22) + px * 3.0);
    o.rects.push(OverlayRect { pos: [xp, head_top], size: [px * 2.0, head_h], color: th.accent, alpha: 0.55, radius: px, glow: ch * 0.45 });
    o.rects.push(rect([xp, head_top], [px * 2.0, head_h], th.cursor, 1.0));
    o.rects.push(OverlayRect { pos: [xp - px * 2.0, head_top - px * 2.0], size: [px * 6.0, px * 6.0], color: th.cursor, alpha: 1.0, radius: px * 3.0, glow: 0.0 });

    // Words. Left: where you are. Right: how far from now. Dropped from the
    // least needed up when the window is narrow.
    let ty = y0 + ch * 2.0;
    let cols = (w / cw).floor() as usize;
    let text = |o: &mut Overlay, col: usize, s: &str, color: C3, alpha: f32, bold: bool| {
        o.texts.push(OverlayText { pos: [x0 + col as f32 * cw, ty], text: s.to_string(), color, alpha, bold });
        col + s.chars().count()
    };
    let (label, pill) = if d.waiting { ("SEEK", th.ansi[3]) } else { ("PAST", th.accent) };
    o.rects.push(OverlayRect { pos: [x0, ty + ch * 0.08], size: [cw * 6.0, ch * 0.84], color: pill, alpha: 0.92, radius: ch * 0.18, glow: 0.0 });
    text(&mut o, 1, label, th.bg, 1.0, true);
    let mut col = text(&mut o, 7, &clock(d.t_us), th.fg, 0.96, true);
    let total = format!(" / {}", clock(d.duration_us));
    let back = format!("{} back", human(d.duration_us.saturating_sub(d.t_us)));
    let count = format!("output {} of {}", d.output, d.outputs);
    let hint = "Esc: live";
    let when = d.started_unix.and_then(|s| local_time(s + d.t_us / 1_000_000));
    // Right side, widest first; each part is dropped if it would not fit.
    let mut right: Vec<(&str, C3, f32)> = vec![(back.as_str(), th.accent, 0.95)];
    let room = |right: &Vec<(&str, C3, f32)>, extra: usize, col: usize| col + 3 + right.iter().map(|p| p.0.chars().count() + 3).sum::<usize>() + extra <= cols;
    // A note (work in progress, or why the past cannot be read) is news:
    // its place is kept before anything that is only nice to have gets one.
    let noted = d.note.map_or(0, |n| n.chars().count() + 3);
    if room(&right, count.len() + 3 + noted, col + total.len()) {
        right.push((count.as_str(), th.fg, 0.55));
    }
    if room(&right, hint.len() + 3 + noted, col + total.len()) {
        right.push((hint, th.fg, 0.38));
    }
    if room(&right, noted, col + total.len()) {
        col = text(&mut o, col, &total, th.fg, 0.42, false);
    }
    if let Some(w) = when.as_deref().filter(|w| room(&right, noted, col + w.len() + 3)) {
        col = text(&mut o, col + 3, w, th.fg, 0.55, false);
    }
    if let Some(note) = d.note {
        // Too long even so (an error can be): as much of it as there is room for.
        let free = cols.saturating_sub(col + 3 + 3 + right.iter().map(|p| p.0.chars().count() + 3).sum::<usize>());
        let shown: String = if note.chars().count() <= free { note.to_string() } else { note.chars().take(free.saturating_sub(1)).chain(['\u{2026}']).collect() };
        if free >= 8 {
            text(&mut o, col + 3, &shown, th.ansi[3], 0.95, false);
        }
    }
    let mut end = cols;
    for (s, color, alpha) in right.iter().rev() {
        let len = s.chars().count();
        if end < len {
            break;
        }
        text(&mut o, end - len, s, *color, *alpha, false);
        end = end.saturating_sub(len + 3);
    }
    o
}

/// Words broken into lines of at most `cols` characters, at spaces where
/// there are any.
fn wrapped(words: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut lines: Vec<String> = vec![String::new()];
    for word in words.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let last = lines.last_mut().expect("at least one line");
            let used = last.chars().count();
            let gap = (used > 0) as usize;
            if used + gap + word.len() <= cols {
                if gap == 1 {
                    last.push(' ');
                }
                last.extend(word.iter());
                break;
            }
            if used > 0 {
                lines.push(String::new());
            } else {
                // A word longer than a whole line: cut it where the line ends.
                last.extend(word.drain(..cols));
                lines.push(String::new());
            }
        }
    }
    lines
}

/// A few words in the bar's place, for when there is no past to show: one
/// line where the window is wide enough for them, more lines where it is not.
pub fn notice(screen: [f32; 2], pad: [f32; 2], m: &CellMetrics, th: &Theme, grid_rows: usize, words: &str) -> Overlay {
    let mut o = Overlay::default();
    let ch = m.cell_h;
    let px = (ch / 22.0).round().max(1.0);
    let cols = ((screen[0] - 2.0 * pad[0]) / m.cell_w).floor().max(1.0) as usize;
    let lines = wrapped(words, cols);
    let y0 = pad[1] + grid_rows.saturating_sub(lines.len()) as f32 * ch;
    let top = y0 - (ch * 0.08).round();
    o.rects.push(rect([0.0, top], [screen[0], screen[1] - top], mix(th.bg, th.accent, 0.045), 1.0));
    o.rects.push(rect([0.0, top], [screen[0], px], th.ansi[3], 0.45));
    for (i, line) in lines.into_iter().enumerate() {
        o.texts.push(OverlayText { pos: [pad[0], y0 + i as f32 * ch], text: line, color: th.ansi[3], alpha: 0.95, bold: false });
    }
    o
}

/// Should a wake-up that reaches the event loop draw a frame? Out of the
/// past, always: it is the live session's output. In the past, only when the
/// seek worker has something to show; what the live session prints meanwhile
/// waits, so a window parked in the past draws nothing.
pub fn wake_draws(scrub: Option<&Scrub>) -> bool {
    scrub.is_none_or(|s| s.has_news())
}

/// A past screen, ready to draw.
pub struct View {
    pub term: Term<Listener>,
    pub pos: usize,
    /// How the seek was served and how long it took.
    pub how: &'static str,
    pub ms: f32,
}

/// One window's trip into its session's past.
pub struct Scrub {
    pub journal: PathBuf,
    pub tl: Timeline,
    /// Where the person has asked to be.
    pub pos: usize,
    /// The newest past screen that has arrived.
    pub view: Option<View>,
    pub error: Option<String>,
    tx: Sender<(u64, usize, usize)>,
    rx: Receiver<Result<View, String>>,
    asked: u64,
    answered: u64,
    /// Seeks served, and the slowest, for the log line written on the way out.
    served: (u32, f32, &'static str),
    news: Arc<AtomicBool>,
    /// Thousandths of the journal indexed so far while a seek is building; else `u32::MAX`.
    progress: Arc<AtomicU32>,
    hist: Vec<u64>,
    hist_for: (usize, usize),
}

impl Scrub {
    /// Read the journal's timeline and start the seek worker. `notify` is
    /// called (with `Wake`) whenever there is something new to draw.
    pub fn open(journal: &Path, notify: Notify) -> std::io::Result<Scrub> {
        let tl = Timeline::scan(journal)?;
        let (tx, jobs) = unbounded::<(u64, usize, usize)>();
        let (done, rx) = unbounded::<Result<View, String>>();
        let news = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU32::new(u32::MAX));
        let (path, flag, prog) = (journal.to_path_buf(), news.clone(), progress.clone());
        std::thread::Builder::new().name("recall".into()).spawn(move || {
            let mut index: Option<Index> = None;
            while let Ok(mut job) = jobs.recv() {
                // Holding a key queues requests faster than they can be
                // served; only the newest still matters.
                let mut skipped = 0;
                while let Ok(newer) = jobs.try_recv() {
                    job = newer;
                    skipped += 1;
                }
                let t0 = Instant::now();
                let mut told = Instant::now();
                let result = match index.take().map(Ok).unwrap_or_else(|| Index::open(&path)) {
                    Ok(mut ix) => {
                        let r = ix.seek_with(Target::Pos(job.0), Some((job.1, job.2)), &mut |at, total| {
                            prog.store((at.saturating_mul(1000) / total.max(1)) as u32, Ordering::Release);
                            if told.elapsed() > Duration::from_millis(80) {
                                told = Instant::now();
                                flag.store(true, Ordering::Release);
                                notify(UiEvent::Wake);
                            }
                        });
                        index = Some(ix);
                        r.map_err(|e| e.to_string())
                    }
                    Err(e) => Err(e.to_string()),
                };
                prog.store(u32::MAX, Ordering::Release);
                let view = result.map(|s| View { term: s.replay.term, pos: s.pos as usize, how: s.how, ms: t0.elapsed().as_secs_f32() * 1000.0 });
                for _ in 0..skipped {
                    let _ = done.send(Err(String::new()));
                }
                if done.send(view).is_err() {
                    break;
                }
                flag.store(true, Ordering::Release);
                notify(UiEvent::Wake);
            }
        })?;
        let pos = tl.evs.len();
        Ok(Scrub { journal: journal.to_path_buf(), tl, pos, view: None, error: None, tx, rx, asked: 0, answered: 0, served: (0, 0.0, ""), news, progress, hist: Vec::new(), hist_for: (0, 0) })
    }

    /// What this trip cost, in measured numbers.
    pub fn summary(&self) -> String {
        let (n, worst, how) = self.served;
        format!("time travel: {n} past screens shown, slowest {worst:.1} ms (served by {how}), {} records in the journal", self.tl.evs.len())
    }

    /// Ask for the screen at `pos`, fitted to a grid of this size.
    pub fn go(&mut self, pos: usize, (cols, rows): (usize, usize)) {
        self.pos = pos;
        self.asked += 1;
        let _ = self.tx.send((pos as u64, cols.max(2), rows.max(1)));
    }

    /// Is there something new to draw? (Asked from the event loop: cheap.)
    pub fn has_news(&self) -> bool {
        self.news.load(Ordering::Acquire)
    }

    /// Take what the worker has finished. True if the picture changed.
    pub fn poll(&mut self) -> bool {
        let mut fresh = self.news.swap(false, Ordering::AcqRel);
        while let Ok(r) = self.rx.try_recv() {
            self.answered += 1;
            match r {
                Ok(v) => {
                    self.served.0 += 1;
                    if v.ms >= self.served.1 {
                        (self.served.1, self.served.2) = (v.ms, v.how);
                    }
                    self.view = Some(v);
                    self.error = None;
                    fresh = true;
                }
                // An empty error stands for a request that was overtaken.
                Err(e) if !e.is_empty() => {
                    self.error = Some(e);
                    fresh = true;
                }
                Err(_) => {}
            }
        }
        fresh
    }

    /// A request is still out.
    pub fn busy(&self) -> bool {
        self.answered < self.asked
    }

    /// The bar for the moment on screen (or asked for, while it is on its way).
    pub fn overlay(&mut self, screen: [f32; 2], pad: [f32; 2], m: &CellMetrics, th: &Theme, grid_rows: usize) -> Overlay {
        let (_, _, n, _) = track(screen[0], pad[0], m.cell_w);
        if self.hist_for != (n, self.tl.evs.len()) {
            self.hist = self.tl.histogram(n);
            self.hist_for = (n, self.tl.evs.len());
        }
        let pos = self.view.as_ref().map_or(self.pos, |v| v.pos);
        let indexing = self.progress.load(Ordering::Acquire);
        let note = match (&self.error, self.busy(), indexing) {
            (Some(e), _, _) => Some(format!("cannot read the past: {e}")),
            (None, true, p) if p != u32::MAX => Some(format!("indexing {}%", p / 10)),
            _ => None,
        };
        bar(
            screen,
            pad,
            m,
            th,
            grid_rows,
            &BarData {
                hist: &self.hist,
                duration_us: self.tl.duration_us(),
                t_us: self.tl.time_at(pos),
                started_unix: self.tl.head.meta["started_unix"].as_u64(),
                output: self.tl.outputs_at(pos),
                outputs: self.tl.outs.len(),
                exit: self.tl.exit,
                note: note.as_deref(),
                waiting: self.view.is_none(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> CellMetrics {
        CellMetrics { cell_w: 9.0, cell_h: 20.0, baseline: 15.0, ul_y: 17.0, ul_h: 1.0, strike_y: 10.0 }
    }

    #[test]
    fn durations_read_like_a_person_wrote_them() {
        assert_eq!(human(850_000), "850 ms");
        assert_eq!(human(12_300_000), "12.3 s");
        assert_eq!(human(192_000_000), "3m 12s");
        assert_eq!(human(3_725_000_000), "1h 02m");
    }

    #[test]
    fn local_time_is_a_real_date() {
        // Whatever the zone, 2026-11-16 12:00 UTC is the 16th or a neighbour.
        let s = local_time(1_794_830_400).unwrap();
        assert!(s.starts_with("2026-11-1"), "{s}");
        assert_eq!(s.len(), 19);
    }

    /// The bar is a picture of its data and nothing else.
    #[test]
    fn the_bar_draws_what_happened_and_only_that() {
        let th = Theme::void_light();
        let (screen, pad, m) = ([1200.0, 700.0], [12.0, 12.0], metrics());
        let (x0, w, n, pitch) = track(screen[0], pad[0], m.cell_w);
        assert!(n > 100 && (n as f32 * pitch) <= w);
        let mut hist = vec![0u64; n];
        (hist[10], hist[200], hist[n - 1]) = (40, 4_000_000, 9);
        let data = |t_us| BarData { hist: &hist, duration_us: 1_000_000, t_us, started_unix: None, output: 5, outputs: 9, exit: Some(1), note: None, waiting: false };
        let o = bar(screen, pad, &m, &th, 33, &data(500_000));
        let top = band_top(pad[1], m.cell_h, 33);
        // Three slices had output, so exactly three histogram bars stand on
        // the baseline, and the tallest belongs to the slice that printed the most.
        let base = (top + m.cell_h * 1.86).round();
        let bars: Vec<&OverlayRect> = o.rects.iter().filter(|r| r.size[0] == pitch - 1.0 && r.pos[1] + r.size[1] == base).collect();
        assert_eq!(bars.len(), 3, "{bars:?}");
        let tallest = bars.iter().max_by(|a, b| a.size[1].total_cmp(&b.size[1])).unwrap();
        assert_eq!(tallest.pos[0], x0 + 200.0 * pitch);
        // Before the playhead in the accent, after it dimmed.
        assert_eq!((bars[0].color, bars[2].color), (th.accent, th.fg));
        assert!(bars[2].alpha < bars[0].alpha);
        // An exit code other than zero is marked in red.
        assert!(o.rects.iter().any(|r| r.color == th.ansi[1]));
        // Everything is inside the window and inside the band.
        for r in &o.rects {
            assert!(r.pos[0] >= 0.0 && r.pos[0] + r.size[0] <= screen[0] + 0.5, "{r:?}");
            assert!(r.pos[1] >= top && r.pos[1] + r.size[1] <= screen[1] + 0.5, "{r:?}");
        }
        let words: Vec<&str> = o.texts.iter().map(|t| t.text.as_str()).collect();
        assert!(words.contains(&"PAST") && words.contains(&"00:00.500") && words.contains(&"500 ms back"), "{words:?}");
        assert!(words.contains(&"output 5 of 9"));
        // The playhead follows the moment: a click where it stands gives that moment back.
        let early = bar(screen, pad, &m, &th, 33, &data(100_000));
        let head = |o: &Overlay| o.rects.iter().find(|r| r.glow > 0.0).map(|r| r.pos[0]).unwrap();
        assert!(head(&early) < head(&o));
        let t = time_at_x(head(&o), screen[0], pad[0], m.cell_w, 1_000_000);
        assert!((t as i64 - 500_000).abs() < 2_000, "{t}");
        // Until the past screen arrives the bar says so, and says what it is doing.
        let waiting = bar(screen, pad, &m, &th, 33, &BarData { waiting: true, note: Some("indexing 43%"), ..data(500_000) });
        let words: Vec<&str> = waiting.texts.iter().map(|t| t.text.as_str()).collect();
        assert!(words.contains(&"SEEK") && !words.contains(&"PAST") && words.contains(&"indexing 43%"), "{words:?}");
        // A narrow window keeps the essentials and drops the rest.
        let narrow = bar([420.0, 300.0], pad, &m, &th, 12, &data(500_000));
        let words: Vec<&str> = narrow.texts.iter().map(|t| t.text.as_str()).collect();
        assert!(words.contains(&"00:00.500") && words.contains(&"500 ms back") && !words.contains(&"Esc: live"), "{words:?}");
        for t in &narrow.texts {
            assert!(t.pos[0] + t.text.chars().count() as f32 * m.cell_w <= 420.0, "{t:?}");
        }
    }

    /// Every text of an overlay lies inside the window and none runs into the next.
    fn texts_fit(o: &Overlay, screen_w: f32, cell_w: f32) {
        let mut spans: Vec<(f32, f32, f32, &str)> = o.texts.iter().map(|t| (t.pos[1], t.pos[0], t.pos[0] + t.text.chars().count() as f32 * cell_w, t.text.as_str())).collect();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
        for s in &spans {
            assert!(s.1 >= 0.0 && s.2 <= screen_w, "`{}` runs from {} to {} in a window {screen_w} wide", s.3, s.1, s.2);
        }
        for pair in spans.windows(2) {
            assert!(pair[0].0 != pair[1].0 || pair[0].2 <= pair[1].1, "`{}` runs into `{}`", pair[0].3, pair[1].3);
        }
    }

    /// In a narrow window what is news keeps its place. The progress of a
    /// first seek, and why the past cannot be read, used to be dropped while
    /// the hint stayed: the bar then said SEEK and nothing else, for ever if
    /// the journal could not be read. (64 columns is the grid of a window
    /// 1200 pixels wide at twice the scale, where it was seen in a picture.)
    #[test]
    fn a_narrow_bar_still_says_what_it_is_doing() {
        let th = Theme::void_light();
        let (pad, m) = ([12.0, 12.0], metrics());
        let screen = [2.0 * pad[0] + 64.0 * m.cell_w, 300.0];
        let hist = vec![640u64; 300];
        let data = |note| BarData { hist: &hist, duration_us: 10_579_061, t_us: 5_813_000, started_unix: Some(1_794_794_455), output: 9663, outputs: 19321, exit: Some(0), note: Some(note), waiting: true };
        let o = bar(screen, pad, &m, &th, 12, &data("indexing 43%"));
        let words: Vec<&str> = o.texts.iter().map(|t| t.text.as_str()).collect();
        assert!(words.contains(&"SEEK") && words.contains(&"00:05.813") && words.contains(&"4.8 s back"), "{words:?}");
        assert!(words.contains(&"indexing 43%"), "the bar does not say what it is doing: {words:?}");
        texts_fit(&o, screen[0], m.cell_w);
        // An error too long for the room there is: as much of it as fits, never nothing.
        let long = "cannot read the past: The process cannot access the file because it is being used by another process. (os error 32)";
        let o = bar(screen, pad, &m, &th, 12, &data(long));
        let shown = o.texts.iter().find(|t| t.text.starts_with("cannot read the past")).expect("the error is not shown at all");
        assert!(shown.text.ends_with('\u{2026}') && shown.text.chars().count() >= 20, "{shown:?}");
        texts_fit(&o, screen[0], m.cell_w);
        // With room to spare nothing is cut and nothing is dropped.
        let wide = bar([2360.0, 1520.0], pad, &m, &th, 60, &data("indexing 43%"));
        let words: Vec<&str> = wide.texts.iter().map(|t| t.text.as_str()).collect();
        assert!(words.contains(&"indexing 43%") && words.contains(&"Esc: live") && words.contains(&"output 9663 of 19321"), "{words:?}");
        texts_fit(&wide, 2360.0, m.cell_w);
    }

    /// The line shown when there is no past to show ran off the edge of a
    /// window narrower than its words. It takes as many lines as it needs.
    #[test]
    fn a_notice_wider_than_the_window_takes_more_lines() {
        let th = Theme::void_light();
        let (pad, m) = ([12.0, 12.0], metrics());
        let words = "time travel is unavailable for this session: it has no journal this window can read";
        let lines = |o: &Overlay| o.texts.iter().map(|t| t.text.clone()).collect::<Vec<_>>();
        let wide = notice([1200.0, 700.0], pad, &m, &th, 33, words);
        assert_eq!(lines(&wide), vec![words.to_string()]);
        for cols in [64.0f32, 40.0, 20.0, 7.0] {
            let screen = [2.0 * pad[0] + cols * m.cell_w, 700.0];
            let o = notice(screen, pad, &m, &th, 33, words);
            texts_fit(&o, screen[0], m.cell_w);
            let got = lines(&o);
            assert!(got.len() > 1 && got.iter().all(|l| !l.is_empty() && l.chars().count() <= cols as usize), "{cols} columns: {got:?}");
            // Nothing lost: the lines are the words (a word longer than a line is cut where the line ends).
            assert_eq!(got.join(" ").replace(' ', ""), words.replace(' ', ""), "{cols} columns");
            // The lines are the grid's last rows, and the band behind them starts above the first.
            let first = pad[1] + (33 - got.len()) as f32 * m.cell_h;
            assert_eq!(o.texts[0].pos[1], first);
            assert!(o.rects.iter().all(|r| r.pos[1] <= first && r.pos[1] >= first - m.cell_h));
        }
        assert_eq!(wrapped("", 10), vec![String::new()]);
    }
}
