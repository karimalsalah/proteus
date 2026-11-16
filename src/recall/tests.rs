//! Tests for recall.
//!
//! The journals are made by real programs on a real pseudo-terminal
//! (`recall_cli::record_to`), with one exception that says so: the storm, a
//! seeded stream of escape sequences written to a journal file, which reaches
//! corners of the terminal no everyday program visits.
use super::*;
use std::sync::Once;
use std::time::Duration;

use crate::journal::JournalWriter;

/// A folder for this run's files. The first call also makes sure the
/// console host is unpacked somewhere under the temp folder and cannot be
/// deleted by another test's clean-up while these tests still start programs.
fn dir() -> PathBuf {
    static ONCE: Once = Once::new();
    let d = std::env::temp_dir().join(format!("proteus-recall-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    ONCE.call_once(|| {
        if std::env::var_os("PROTEUS_DATA").is_none() {
            std::env::set_var("PROTEUS_DATA", std::env::temp_dir().join("proteus-recall-test-data"));
        }
        crate::runtime::ensure_conpty();
        for e in std::fs::read_dir(crate::runtime::dir()).into_iter().flatten().flatten() {
            use std::os::windows::fs::OpenOptionsExt;
            if let Ok(f) = std::fs::OpenOptions::new().read(true).share_mode(1).open(e.path().join("OpenConsole.exe")) {
                std::mem::forget(f);
            }
        }
    });
    d
}

fn forget(journal: &Path) {
    let _ = std::fs::remove_file(sidecar(journal));
    let _ = std::fs::remove_file(journal);
    let _ = std::fs::remove_dir(dir());
}

/// Run a real program on a real pseudo-terminal and journal what it prints.
fn record(name: &str, exe: &str, args: &[&str], size: (u16, u16), wait: f64, resizes: &[(f64, u16, u16)], typed: &[(f64, &str)]) -> Option<PathBuf> {
    let p = dir().join(format!("{name}.prtj"));
    let _ = std::fs::remove_file(sidecar(&p));
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let typed = typed.iter().map(|(t, s)| (*t, s.as_bytes().to_vec())).collect();
    match crate::recall_cli::record_to(&p, exe, &args, size, Duration::from_secs_f64(wait), resizes.to_vec(), typed) {
        Ok((bytes, _)) if bytes > 0 => Some(p),
        other => {
            eprintln!("could not record `{exe}`: {:?}", other.map_err(|e| e.to_string()));
            None
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// What the terminal keeps to itself, read the only way it can be: by doing
/// things to it and watching. Destroys the terminal, so it comes last.
fn private_state(mut r: Replay) -> String {
    use ansi::CharsetIndex::*;
    r.settle();
    let t = &mut r.term;
    let (cols, rows) = (t.columns(), t.screen_lines());
    // Tab stops: where a backward tab lands from every column.
    let tabs: Vec<usize> = (1..cols)
        .map(|c| {
            t.grid_mut().cursor.point.column = Column(c);
            t.move_backward_tabs(1);
            t.grid().cursor.point.column.0
        })
        .collect();
    // Scroll region: where "home" and "far down" are in origin mode.
    t.set_private_mode(PrivateMode::Named(NamedPrivateMode::Origin));
    let top = t.grid().cursor.point.line.0;
    t.goto(rows as i32 * 2, 0);
    let bottom = t.grid().cursor.point.line.0;
    // Active charset: with line drawing on G1+G3, then on G2+G3, what does 'q' print?
    let mut drawn = String::new();
    t.unset_mode(Mode::Named(NamedMode::Insert));
    for sets in [[false, true, false, true], [false, false, true, true]] {
        for (i, ix) in [G0, G1, G2, G3].into_iter().enumerate() {
            t.grid_mut().cursor.charsets[ix] = if sets[i] { StandardCharset::SpecialCharacterAndLineDrawing } else { StandardCharset::Ascii };
        }
        t.goto(0, 0);
        let line = t.grid().cursor.point.line;
        t.input('q');
        drawn.push(t.grid()[line][Column(0)].c);
    }
    format!("tabs {tabs:?} region {top}..={bottom} charset {drawn}")
}

/// Two replays must be the same terminal: the screen in view, the screen
/// behind it, the private state, what a synchronized update still open is
/// holding back, and all scrollback (when `deep`; otherwise as much of it as
/// both hold).
fn assert_same(mut want: Replay, mut got: Replay, deep: bool, what: &str) {
    for side in ["showing", "hidden"] {
        let depth = (!deep).then(|| want.term.history_size().min(got.term.history_size()));
        if let Some(d) = diff(&want.term, &got.term, depth) {
            panic!("{what}: the {side} screen differs: {d}");
        }
        want.term.swap_alt();
        got.term.swap_alt();
    }
    assert_eq!(pending(&want.proc), pending(&got.proc), "{what}: a synchronized update is open on one side only");
    if pending(&want.proc) {
        want.settle();
        got.settle();
        let depth = (!deep).then(|| want.term.history_size().min(got.term.history_size()));
        if let Some(d) = diff(&want.term, &got.term, depth) {
            panic!("{what}: with the open update put on screen: {d}");
        }
    }
    assert_eq!(private_state(want), private_state(got), "{what}: private state");
}

/// Seek to `n` seeded instants (half by time, half by record) and require
/// each to equal a replay from the start. Returns how the seeks were served.
fn check_instants(path: &Path, n: usize, seed: u64, every: u64, view: Option<(usize, usize)>) -> Vec<&'static str> {
    let tl = Timeline::scan(path).unwrap();
    let mut ix = Index::open(path).unwrap();
    (ix.every, ix.ratio) = (every, 0);
    ix.build(&mut |_, _| {}).unwrap();
    assert_eq!(ix.stats.rejected, 0, "a keyframe did not reproduce its terminal: {:?}", ix.first_reject);
    let mut rng = Rng(seed | 1);
    let mut how = Vec::new();
    for i in 0..n {
        let target = if i % 2 == 0 { Target::Time(rng.below(tl.duration_us() + 1)) } else { Target::Pos(rng.below(tl.evs.len() as u64 + 1)) };
        let got = ix.seek(target, view).unwrap();
        let want = replay_from_start(path, target, view).unwrap();
        assert_eq!((got.pos, got.t_us, got.at_end), (want.pos, want.t_us, want.at_end), "{target:?}");
        assert_eq!(screen_text(&want.replay.term), screen_text(&got.replay.term), "{target:?} via {}", got.how);
        how.push(got.how);
        assert_same(want.replay, got.replay, false, &format!("{} {target:?} via {}", path.display(), got.how));
    }
    how
}

/// Full-scrollback snapshots taken along the way must each be the terminal
/// they were taken from, and replaying the rest of the journal from any of
/// them must end where a replay from the start ends, down to the last cell
/// of scrollback and the private state.
fn check_full_snapshots(path: &Path, stride: usize, limit: usize) -> usize {
    let (head, mut rd) = Reader::open(path).unwrap();
    let (cols, rows) = head.size();
    let mut live = Replay::new(cols, rows, true);
    let mut plain = Replay::new(cols, rows, false);
    let (mut recs, mut snaps, mut buf, mut clean) = (Vec::new(), Vec::new(), Vec::new(), 0usize);
    while let Some(h) = rd.next() {
        rd.payload(&h, &mut buf).unwrap();
        live.apply(h.kind, &buf);
        plain.apply(h.kind, &buf);
        recs.push((h.kind, buf.clone()));
        if h.kind == journal::K_OUTPUT && live.clean {
            clean += 1;
            if clean % stride == 0 && snaps.len() < limit {
                let snap = live.snapshot(usize::MAX).expect("a clean point gives a snapshot");
                let back = Replay::restore(&snap, false).unwrap();
                assert_eq!(diff(&live.term, &back.term, None), None, "snapshot after record {}", recs.len());
                // Taking it must not have disturbed the terminal it was taken from.
                assert_eq!(diff(&live.term, &plain.term, None), None, "taking a snapshot changed the terminal (record {})", recs.len());
                snaps.push((recs.len(), snap));
            }
        }
    }
    let finish = |mut r: Replay, from: usize| {
        recs[from..].iter().for_each(|(k, d)| r.apply(*k, d));
        r.settle();
        r
    };
    for (at, snap) in &snaps {
        for tracked in [false, true] {
            let got = finish(Replay::restore(snap, tracked).unwrap(), *at);
            let want = finish(Replay::new(cols, rows, false), 0);
            assert_same(want, got, true, &format!("{} resumed after record {at} (tracked {tracked})", path.display()));
        }
    }
    snaps.len()
}

/// `find` the slow, obvious way: at every look, the whole screen as text.
/// The real one keeps a memory of the screen and reads only what changed;
/// the two must report exactly the same.
fn find_slowly(journal: &Path, needle: &str, fold: bool) -> Vec<Hit> {
    let needle = if fold { needle.to_lowercase() } else { needle.to_string() };
    let (mut hits, mut open): (Vec<Hit>, bool) = (Vec::new(), false);
    walk(journal, |rp, t_us, _| {
        // Lines as they were printed: rows joined across soft wraps.
        let (mut text, mut starts, mut joined) = (String::new(), Vec::new(), false);
        for l in 0..rp.term.screen_lines() {
            let row = &rp.term.grid()[Line(l as i32)][..];
            if !joined {
                starts.push(l);
            }
            push_row(&mut text, row);
            // (A line still wrapping at the last row ends there: the rest is not on screen.)
            joined = row.last().is_some_and(|c| c.flags.contains(Flags::WRAPLINE)) && l + 1 < rp.term.screen_lines();
            if !joined {
                text.truncate(text.trim_end_matches(' ').len());
                text.push('\n');
            }
        }
        if fold {
            text = text.to_lowercase();
        }
        match (text.find(&needle), open) {
            (Some(i), false) => {
                let n = text[..i].matches('\n').count();
                hits.push(Hit { appeared_us: t_us, left_us: None, line: text.split('\n').nth(n).unwrap_or("").to_string(), row: starts[n] });
                open = true;
            }
            (None, true) => {
                hits.last_mut().unwrap().left_us = Some(t_us);
                open = false;
            }
            _ => {}
        }
    })
    .unwrap();
    hits
}

/// The quick `find` and the obvious one must agree, needle by needle.
fn check_find(journal: &Path, needles: &[&str]) -> usize {
    let mut seen = 0;
    for needle in needles {
        for fold in [false, true] {
            let (quick, slow) = (find(journal, needle, fold).unwrap().hits, find_slowly(journal, needle, fold));
            // (Compared one by one so a failure names the moment, not two long lists.)
            for (i, (q, s)) in quick.iter().zip(&slow).enumerate() {
                assert_eq!(q, s, "{} `{needle}` (ignoring case: {fold}), time on screen number {i}", journal.display());
            }
            assert_eq!(quick.len(), slow.len(), "{} `{needle}` (ignoring case: {fold})", journal.display());
            seen += quick.len();
        }
    }
    seen
}

// ---------------------------------------------------------- generated: the storm

/// A seeded stream of everything a terminal can be told to do. NOT a
/// recording of a program: it exists to reach states no sample session does.
fn storm(seed: u64, ops: usize) -> Vec<u8> {
    let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut o: Vec<u8> = Vec::new();
    let words = ["cargo", "build", "error[E0308]:", "warning:", "ok", "──────", "漢字かな", "e\u{301}tude", "😀", "│", "test result", "    ", "~/proteus", "0123456789", "q", "lqqk"];
    macro_rules! w {
        ($($t:tt)*) => { o.extend_from_slice(format!($($t)*).as_bytes()) };
    }
    for _ in 0..ops {
        let (a, b, c) = (r.below(60), r.below(140), r.below(256));
        match r.below(110) {
            0..=29 => {
                for _ in 0..1 + r.below(6) {
                    w!("{} ", words[r.below(words.len() as u64) as usize]);
                }
            }
            30..=41 => match r.below(16) {
                0 => w!("\x1b[0m"),
                1 => w!("\x1b[1m"),
                2 => w!("\x1b[2;3m"),
                3 => w!("\x1b[4m"),
                4 => w!("\x1b[4:3m"),
                5 => w!("\x1b[4:5;21m"),
                6 => w!("\x1b[7m"),
                7 => w!("\x1b[8m"),
                8 => w!("\x1b[9m"),
                9 => w!("\x1b[{}m", 30 + c % 8),
                10 => w!("\x1b[38;5;{c}m"),
                11 => w!("\x1b[38;2;{a};{b};{c}m"),
                12 => w!("\x1b[48;5;{c}m"),
                13 => w!("\x1b[58;2;{c};{a};{b}m"),
                14 => w!("\x1b[{};{}m", 90 + c % 8, 100 + a % 8),
                _ => w!("\x1b[22;23;24;27;28;29;39;49;59m"),
            },
            42..=53 => match r.below(11) {
                0 | 1 => w!("\x1b[{};{}H", 1 + a, 1 + b),
                2 => w!("\x1b[{}A", 1 + a % 5),
                3 => w!("\x1b[{}B", 1 + a % 5),
                4 => w!("\x1b[{}C", 1 + b % 20),
                5 => w!("\x1b[{}D", 1 + b % 20),
                6 => w!("\x1b[{}G", 1 + b),
                7 => w!("\x1b[{}d", 1 + a),
                8 => w!("\r"),
                9 => w!("\x1b[{}E", 1 + a % 3),
                _ => w!("\n"),
            },
            54..=63 => w!("\r\n"),
            64..=71 => match r.below(12) {
                0 => w!("\x1b[J"),
                1 => w!("\x1b[1J"),
                2 => w!("\x1b[2J"),
                3 => w!("\x1b[3J"),
                4 => w!("\x1b[K"),
                5 => w!("\x1b[1K"),
                6 => w!("\x1b[{}X", 1 + b % 30),
                7 => w!("\x1b[{}@", 1 + b % 10),
                8 => w!("\x1b[{}P", 1 + b % 10),
                9 => w!("\x1b[{}L", 1 + a % 4),
                10 => w!("\x1b[{}M", 1 + a % 4),
                _ => w!("\x1b[{}{}", 1 + a % 4, if c % 2 == 0 { 'S' } else { 'T' }),
            },
            72..=76 => {
                let on = if c % 2 == 0 { 'h' } else { 'l' };
                match r.below(14) {
                    0 => w!("\x1b[?6{on}"),
                    1 => w!("\x1b[?7{on}"),
                    2 => w!("\x1b[?25{on}"),
                    3 => w!("\x1b[?1{on}"),
                    4 => w!("\x1b[?1000{on}"),
                    5 => w!("\x1b[?1002{on}"),
                    6 => w!("\x1b[?1003{on}"),
                    7 => w!("\x1b[?1006{on}\x1b[?1005{on}"),
                    8 => w!("\x1b[?2004{on}"),
                    9 => w!("\x1b[?1004;1007{on}"),
                    10 => w!("\x1b[4{on}"),
                    11 => w!("\x1b[20{on}"),
                    12 => w!("\x1b[?12{on}\x1b[?1042{on}"),
                    _ => w!("\x1b{}", if c % 2 == 0 { '=' } else { '>' }),
                }
            }
            77..=81 => match r.below(8) {
                0 | 1 => w!("\x1b[{};{}r", 1 + a % 20, 2 + a % 20 + b % 30),
                2 => w!("\x1b[r"),
                3 => w!("\x1bH"),
                4 => w!("\x1b[g"),
                5 => w!("\x1b[3g"),
                6 => w!("\x1b[{}I", 1 + a % 3),
                _ => w!("\x1b[{}Z", 1 + a % 3),
            },
            82..=86 => match r.below(11) {
                0 | 1 => w!("\x1b7"),
                2 | 3 => w!("\x1b8"),
                4 => w!("\x1b(0"),
                5 => w!("\x1b(B"),
                6 => w!("\x1b)0"),
                7 => w!("\x0e"),
                8 => w!("\x0f"),
                9 => w!("\x1bD\x1bM"),
                _ => w!("\x1bM\x1bE"),
            },
            87..=91 => match r.below(12) {
                0 => w!("\x1b]0;title {c}\x07"),
                1 => w!("\x1b]2;other {c}\x1b\\"),
                2 => w!("\x1b[22;0t"),
                3 => w!("\x1b[23;0t"),
                4 => w!("\x1b]8;id=link{};https://example.com/{a}\x1b\\", c % 4),
                5 => w!("\x1b]8;;https://example.com/auto/{a}\x1b\\"),
                6 | 7 => w!("\x1b]8;;\x1b\\"),
                8 => w!("\x1b]4;{};rgb:{:02x}/{:02x}/{:02x}\x07", c % 20, a, b, c),
                9 => w!("\x1b]104;{}\x07", c % 20),
                10 => w!("\x1b]10;#{:02x}{:02x}{:02x}\x07\x1b]12;#{:02x}0000\x07", a, b, c, a),
                _ => w!("\x1b]110\x07\x1b]112\x07"),
            },
            92..=94 => w!("\x1b[?1049{}", if c % 3 == 0 { 'l' } else { 'h' }),
            95..=96 => w!("\x1b[{}b", 1 + b % 12),
            97..=98 => match r.below(5) {
                0 => w!("\x1b[?2026h"),
                1 | 2 => w!("\x1b[?2026l"),
                _ => w!("\x1b[?2026hframe {c}\r\n\x1b[32mdone\x1b[0m\x1b[?2026l"),
            },
            99..=101 => match r.below(8) {
                0 => w!("\x1b[{} q", c % 7),
                1 => w!("\x1b#8"),
                2 => w!("\x1b[c\x1b[6n\x1b]11;?\x07"),
                3 => w!("\t"),
                4 => w!("\x08\x08"),
                5 => w!("\x07"),
                6 => w!("\x1b[>1u\x1b[<u"),
                _ => w!("\x1b[!p"),
            },
            102 => w!("\x1b[?3{}", if c % 2 == 0 { 'h' } else { 'l' }),
            103 if c < 40 => w!("\x1bc"),
            _ => w!("\t{}\t", words[r.below(words.len() as u64) as usize]),
        }
    }
    o
}

/// Write the storm as a journal: records cut at arbitrary bytes (so escape
/// sequences and characters straddle records) with resizes in between.
fn storm_journal(name: &str, seed: u64, ops: usize) -> PathBuf {
    let p = dir().join(format!("{name}.prtj"));
    let _ = std::fs::remove_file(sidecar(&p));
    let bytes = storm(seed, ops);
    let mut r = Rng(seed ^ 0xabcdef);
    let mut j = JournalWriter::create(&p, &serde_json::json!({ "v": 1, "session": name, "cols": 80, "rows": 24, "generated": "vt storm" })).unwrap();
    j.record(journal::K_RESIZE, &crate::session::resize_payload(80, 24)).unwrap();
    let sizes = [(80u16, 24u16), (100, 30), (60, 20), (132, 43), (40, 10), (80, 24)];
    let mut i = 0;
    while i < bytes.len() {
        let most = if r.below(4) == 0 { 2000 } else { 120 };
        let n = (1 + r.below(most) as usize).min(bytes.len() - i);
        j.record(journal::K_OUTPUT, &bytes[i..i + n]).unwrap();
        i += n;
        if r.below(160) == 0 {
            let (c, rows) = sizes[r.below(sizes.len() as u64) as usize];
            j.record(journal::K_RESIZE, &crate::session::resize_payload(c, rows)).unwrap();
        }
    }
    j.flush().unwrap();
    p
}

/// The storm at another pace: the same records, their times spread out with
/// seeded pauses, some longer than the window waits for a synchronized
/// update to close. (Written at full speed no pause in a storm is that long,
/// so nothing in it is ever put on screen part-way.)
fn storm_paced(src: &Path, name: &str, seed: u64) -> PathBuf {
    let (head, mut rd) = Reader::open(src).unwrap();
    let mut out = std::fs::read(src).unwrap()[..head.data_start as usize].to_vec();
    let mut r = Rng(seed ^ 0x51ed_270b);
    let (mut buf, mut t_us) = (Vec::new(), 0u64);
    while let Some(h) = rd.next() {
        rd.payload(&h, &mut buf).unwrap();
        t_us += match r.below(8) {
            0 => 160_000 + r.below(300_000),
            1..=2 => 20_000 + r.below(100_000),
            _ => r.below(300),
        };
        out.push(h.kind);
        out.extend_from_slice(&t_us.to_le_bytes());
        out.extend_from_slice(&h.len.to_le_bytes());
        out.extend_from_slice(&buf);
    }
    let p = dir().join(format!("{name}.prtj"));
    let _ = std::fs::remove_file(sidecar(&p));
    std::fs::write(&p, out).unwrap();
    p
}

/// How many times a replay of `path` by the journal's clock stopped waiting
/// for a synchronized update and put it on screen before it closed.
fn shown_part_way(path: &Path) -> usize {
    let (head, mut rd) = Reader::open(path).unwrap();
    let (cols, rows) = head.size();
    let mut rp = Replay::new(cols, rows, false);
    let (mut buf, mut n) = (Vec::new(), 0);
    while let Some(h) = rd.next() {
        rd.payload(&h, &mut buf).unwrap();
        n += rp.overdue(h.t_us).is_some() as usize;
        rp.apply_at(h.kind, &buf, h.t_us);
    }
    n
}

#[test]
fn storm_keyframes_resume_exactly() {
    // PROTEUS_STORM_SEEDS=200 for a long soak; four seeds keep the gate quick.
    let seeds = std::env::var("PROTEUS_STORM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(4u64);
    let mut part_way = 0;
    for seed in 1..=seeds {
        let p = storm_journal(&format!("storm-{seed}"), seed, 9000);
        // Slowed down, with pauses the window would not have waited through.
        let slow = storm_paced(&p, &format!("storm-{seed}-slow"), seed);
        part_way += shown_part_way(&slow);
        let how = check_instants(&slow, 80, seed + 300, 3000, None);
        assert!(how.iter().filter(|h| **h == "keyframe").count() >= 10, "seed {seed}, slowed: seeks were served {how:?}");
        check_instants(&slow, 30, seed + 400, 3000, Some((70, 18)));
        check_instants(&slow, 30, seed + 500, 3000, Some((120, 50)));
        check_find(&slow, &["cargo build", "frame", "\u{6f22}\u{5b57}", "Test Result"]);
        forget(&slow);
        // Full-scrollback snapshots, resumed and compared down to private state.
        // (How many there are varies: a storm may hold a synchronized update open for long.)
        let n = check_full_snapshots(&p, 7, 60);
        assert!(n >= 5, "seed {seed}: only {n} clean points were found to snapshot");
        // The index as it is really used: tail keyframes, seeks by time and by record.
        let how = check_instants(&p, 80, seed, 3000, None);
        assert!(how.iter().filter(|h| **h == "keyframe").count() >= 10, "seed {seed}: seeks were served {how:?}");
        // And refitted to viewers smaller and larger than the session.
        check_instants(&p, 30, seed + 100, 3000, Some((70, 18)));
        check_instants(&p, 30, seed + 200, 3000, Some((120, 50)));
        // Searching it: the quick way against the obvious way.
        let seen = check_find(&p, &["cargo build", "error[E0308]:", "\u{6f22}\u{5b57}", "frame", "lqqk", "Test Result", "0123456789 ~/proteus"]);
        assert!(seen > 20, "seed {seed}: the needles were hardly ever on screen ({seen})");
        forget(&p);
    }
    // (Seeded, so this is a fact about the storm, not luck: 11 times in the first seed, 40 in the first four, 532 in sixty.)
    assert!(part_way as u64 >= seeds.min(4) * 4, "the slowed storms put a synchronized update on screen part-way only {part_way} times: that path was not tested");
}

/// `find`'s memory of the screen, kept up to date from what the terminal
/// says changed, against a fresh reading of the whole screen at every look.
#[test]
fn the_watch_never_loses_track_of_the_screen() {
    for seed in [1u64, 2, 3] {
        let p = storm_journal(&format!("watch-{seed}"), seed, 6000);
        let mut watch = Watch::new("cargo", false);
        let mut looks = 0u64;
        walk(&p, |rp, _, lone| {
            looks += 1;
            watch.look(&mut rp.term, lone);
            let mut fresh = Watch::new("cargo", false);
            fresh.look(&mut rp.term, false);
            for r in 0..fresh.text.len() {
                let (kept, now) = ((&watch.text[r], watch.wrap[r], watch.hit[r]), (&fresh.text[r], fresh.wrap[r], fresh.hit[r]));
                if kept != now {
                    for q in 0..fresh.text.len() {
                        eprintln!("{q:2} kept {:?} {} {} | now {:?} {} {}", watch.text[q], watch.wrap[q], watch.hit[q], fresh.text[q], fresh.wrap[q], fresh.hit[q]);
                    }
                    eprintln!("cursor {:?} mode {:?}", rp.term.grid().cursor.point, rp.term.mode());
                }
                assert_eq!(kept, now, "seed {seed}, look {looks} (after a lone line feed: {lone}), row {r}");
            }
        })
        .unwrap();
        forget(&p);
    }
}

/// The tracking handler must change nothing: a tracked and an untracked
/// replay of the same bytes are the same terminal at every record.
#[test]
fn tracking_does_not_change_the_terminal() {
    let bytes = storm(77, 6000);
    let (mut a, mut b) = (Replay::new(90, 28, true), Replay::new(90, 28, false));
    let mut r = Rng(5);
    let mut i = 0;
    while i < bytes.len() {
        let n = (1 + r.below(90) as usize).min(bytes.len() - i);
        a.feed(&bytes[i..i + n]);
        b.feed(&bytes[i..i + n]);
        i += n;
        if r.below(40) == 0 {
            assert_eq!(diff(&a.term, &b.term, None), None, "after byte {i}");
        }
    }
    assert_same(a, b, true, "tracked against untracked");
}

/// The shadow of the terminal's private state against the terminal itself.
#[test]
fn shadow_matches_what_the_terminal_does() {
    let bytes = storm(4242, 5000);
    let mut r = Rng(9);
    let mut i = 0;
    let mut live = Replay::new(80, 24, true);
    let mut checked = 0;
    while i < bytes.len() {
        let n = (1 + r.below(200) as usize).min(bytes.len() - i);
        live.feed(&bytes[i..i + n]);
        i += n;
        if live.clean && r.below(6) == 0 {
            // Rebuild a terminal from the snapshot (which carries the shadow)
            // and a second one from the same bytes, and probe both.
            let snap = live.snapshot(usize::MAX).unwrap();
            let mut from_start = Replay::new(80, 24, false);
            from_start.feed(&bytes[..i]);
            assert_eq!(private_state(Replay::restore(&snap, false).unwrap()), private_state(from_start), "after byte {i}");
            checked += 1;
        }
    }
    assert!(checked > 30, "only {checked} points checked");
}

// ------------------------------------------------------------- real programs

#[test]
fn a_flood_of_real_output_seeks_exactly() {
    let Some(p) = record("dir", "cmd.exe", &["/c", "dir", "C:\\Windows\\System32"], (100, 30), 60.0, &[], &[]) else {
        panic!("cmd.exe could not be run on a pseudo-terminal");
    };
    let tl = Timeline::scan(&p).unwrap();
    assert!(tl.out_bytes > 100_000, "expected a long listing, got {} bytes", tl.out_bytes);
    assert_eq!(tl.exit, Some(0));
    assert_eq!(tl.histogram(64).iter().sum::<u64>(), tl.out_bytes);

    let how = check_instants(&p, 40, 11, 16 << 10, None);
    assert!(how.iter().any(|h| *h == "keyframe"));
    // Every line of a flood is seen, though each is on screen for an instant.
    assert!(check_find(&p, &["notepad.exe", "KERNEL32.DLL", "File(s)", "<DIR>"]) > 4);
    // The default spacing too (one keyframe or none in a journal this small).
    let _ = std::fs::remove_file(sidecar(&p));
    check_instants(&p, 10, 12, KF_BYTES, None);
    assert!(check_full_snapshots(&p, 150, 12) >= 4);

    // A viewer much taller than the session wants more scrollback than a
    // keyframe carries: the seek must notice and start over from the beginning.
    let mut ix = Index::open(&p).unwrap();
    (ix.every, ix.ratio) = (64 << 10, 0);
    ix.build(&mut |_, _| {}).unwrap();
    let at = Target::Pos(ix.frames.last().expect("keyframes").pos);
    let tall = Some((100, 30 + HIST_TAIL + 240));
    let got = ix.seek(at, tall).unwrap();
    assert_eq!(got.how, "full");
    assert_same(replay_from_start(&p, at, tall).unwrap().replay, got.replay, false, "tall viewer");

    // Stepping through time lands on output records and knows where the present is.
    let end = tl.evs.len();
    let back1 = tl.step(end, -1).unwrap();
    assert!(back1 < end && tl.evs[back1 - 1].kind == journal::K_OUTPUT);
    assert_eq!(tl.step(back1, 1), None, "one step forward from one step back is the present");
    let back2 = tl.step(back1, -1).unwrap();
    assert_eq!(tl.step(back2, 1), Some(back1));
    assert_eq!(tl.step(back2, -1_000_000), tl.outs.first().map(|&p| p as usize));
    assert_eq!(tl.pos_at(tl.duration_us()), end);
    forget(&p);
}

#[test]
fn coloured_real_output_seeks_exactly() {
    let args = ["--no-pager", "log", "-p", "-n", "12", "--color=always"];
    let Some(p) = record("gitlog", "git", &args, (120, 36), 60.0, &[(0.15, 90, 28), (0.4, 140, 44)], &[]) else {
        eprintln!("SKIPPED: git is not available here, so no coloured real output was tested");
        return;
    };
    let how = check_instants(&p, 40, 21, 32 << 10, None);
    assert!(how.iter().any(|h| *h == "keyframe"));
    check_instants(&p, 12, 22, 32 << 10, Some((100, 50)));
    assert!(check_find(&p, &["diff --git", "Co-Authored-By", "@@", "fn main"]) > 4);
    assert!(check_full_snapshots(&p, 400, 10) >= 3);
    forget(&p);
}

/// A program that paints: colours, cursor addressing, the alternate screen
/// and back, while its window is resized under it.
#[test]
fn a_full_screen_program_and_resizes_seek_exactly() {
    let script = dir().join("paint.ps1");
    std::fs::write(
        &script,
        r#"$e=[char]27
[Console]::Write("$e[2J$e[H")
1..70 | % { [Console]::Write("$e[3$($_ % 8)mline $_ of the main screen$e[0m`r`n") }
[Console]::Write("$e[?1049h$e[2J$e[5;10H$e[44;97m ALT SCREEN $e[0m$e[7;3Hmenu one$e[8;3Hmenu two$e7")
Start-Sleep -Milliseconds 500
[Console]::Write("$e[9;3H$e[1;4mbold choice$e[0m$e[12;1H" + ('wide ' * 40))
Start-Sleep -Milliseconds 500
[Console]::Write("$e8restored$e[?1049l")
[Console]::Write("back on the main screen`r`n")
1..40 | % { [Console]::Write("tail $_ " + ('x' * ($_ * 3)) + "`r`n"); Start-Sleep -Milliseconds 10 }
"#,
    )
    .unwrap();
    let file = script.to_string_lossy().into_owned();
    let args = ["-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", file.as_str()];
    // Resizes are timed to land while the script is running, wherever that is.
    let resizes: Vec<(f64, u16, u16)> = (1..=12).map(|i| (i as f64 * 0.35, [70u16, 110, 96][i % 3], [20u16, 34, 28][i % 3])).collect();
    let Some(p) = record("paint", "powershell.exe", &args, (96, 28), 60.0, &resizes, &[]) else {
        panic!("powershell.exe could not be run on a pseudo-terminal");
    };
    let _ = std::fs::remove_file(&script);
    let seen = find(&p, "ALT SCREEN", false).unwrap();
    assert!(!seen.hits.is_empty(), "the alternate screen never showed");
    assert!(seen.hits.last().unwrap().left_us.is_some(), "the alternate screen never went away");
    let tl = Timeline::scan(&p).unwrap();
    assert!(tl.evs.iter().filter(|e| e.kind == journal::K_RESIZE).count() >= 3, "no resize landed while it ran");

    let how = check_instants(&p, 60, 31, 1 << 10, None);
    assert!(how.iter().any(|h| *h == "keyframe"));
    assert!(check_find(&p, &["ALT SCREEN", "menu two", "tail 2", "wide wide", "main screen"]) > 4);
    check_instants(&p, 20, 32, 1 << 10, Some((80, 40)));
    assert!(check_full_snapshots(&p, 3, 60) >= 5);
    forget(&p);
}

#[test]
fn find_says_when_text_appeared_and_when_it_left() {
    let typed = [(1.0, "echo alpha-marker\r"), (1.8, "cls\r"), (2.6, "echo beta-marker\r"), (3.4, "exit\r")];
    let Some(p) = record("find", "cmd.exe", &[], (90, 24), 30.0, &[], &typed) else {
        panic!("cmd.exe could not be run on a pseudo-terminal");
    };
    let alpha = find(&p, "alpha-marker", false).unwrap();
    let beta = find(&p, "BETA-MARKER", true).unwrap();
    assert!(!alpha.hits.is_empty() && !beta.hits.is_empty(), "alpha {:?} beta {:?}", alpha.hits, beta.hits);
    let (seen, gone) = (alpha.hits[0].appeared_us, alpha.hits.last().unwrap().left_us.expect("cls took it off the screen"));
    assert!(seen < gone && gone <= beta.hits[0].appeared_us, "alpha {:?} beta {:?}", alpha.hits, beta.hits);
    assert!(alpha.hits[0].line.contains("alpha-marker"));
    assert!(find(&p, "ALPHA-MARKER", false).unwrap().hits.is_empty(), "the search is case-sensitive unless asked");
    assert!(find(&p, "never-printed-anywhere", false).unwrap().hits.is_empty());

    // The screen at those moments agrees with what find reported.
    let text = |t_us: u64| screen_text(&seek(&p, t_us, None).unwrap().replay.term).join("\n");
    assert!(text(seen).contains("alpha-marker"));
    assert!(!text(gone).contains("alpha-marker"));
    assert!(text(beta.hits[0].appeared_us).contains("beta-marker"));
    forget(&p);
}

/// The sidecar is a cache: it grows with the journal, and damaging,
/// truncating, replacing or deleting it costs time, never correctness.
#[test]
fn the_sidecar_grows_with_the_journal_and_survives_damage() {
    let Some(src) = record("grow-src", "cmd.exe", &["/c", "dir", "/s", "C:\\Windows\\System32\\drivers"], (110, 32), 60.0, &[], &[]) else {
        panic!("cmd.exe could not be run on a pseudo-terminal");
    };
    let bytes = std::fs::read(&src).unwrap();
    // The first half of the session, cut where a record ends: a journal still being written.
    let (_, mut rd) = Reader::open(&src).unwrap();
    while let Some(h) = rd.next() {
        rd.skip(&h).unwrap();
        if rd.pos > bytes.len() as u64 / 2 {
            break;
        }
    }
    let p = dir().join("grow.prtj");
    let side = sidecar(&p);
    let _ = std::fs::remove_file(&side);
    std::fs::write(&p, &bytes[..rd.pos as usize]).unwrap();
    let every = 12 << 10;
    let build = |p: &Path| {
        let mut ix = Index::open(p).unwrap();
        (ix.every, ix.ratio) = (every, 0);
        let had = ix.frames.len();
        ix.build(&mut |_, _| {}).unwrap();
        assert_eq!(ix.stats.rejected, 0, "{:?}", ix.first_reject);
        (had, ix.frames.len())
    };
    let (had, half) = build(&p);
    assert!(had == 0 && half >= 2, "expected keyframes in the first half, got {half}");
    assert_eq!(build(&p), (half, half), "a second build must find the first one's work and add nothing");

    // The session carries on: the rest of its records are appended.
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(&bytes[rd.pos as usize..]).unwrap();
    let (had, all) = build(&p);
    assert!(had == half && all > half, "had {had}, now {all}, first half {half}");
    check_instants(&p, 24, 41, every, None);

    // One flipped byte inside a keyframe: found out by its checksum when used.
    let good = std::fs::read(&side).unwrap();
    let mut hurt = good.clone();
    hurt[good.len() / 2] ^= 0x40;
    std::fs::write(&side, &hurt).unwrap();
    check_instants(&p, 24, 42, every, None);
    assert_eq!(build(&p).1, all, "the damaged part was made again");
    // Cut off mid-entry, replaced by something else, and gone.
    std::fs::write(&side, &good[..good.len() * 3 / 5]).unwrap();
    check_instants(&p, 12, 43, every, None);
    std::fs::write(&side, b"this is not an index").unwrap();
    check_instants(&p, 12, 44, every, None);
    std::fs::remove_file(&side).unwrap();
    check_instants(&p, 12, 45, every, None);
    assert!(side.exists(), "the sidecar comes back by itself");
    // The same header over other records: a journal written again under its
    // old name, or one whose oldest output has been trimmed away. The index
    // left beside it describes records that are no longer there.
    assert_eq!(build(&p).1, all);
    let (head, mut rd) = Reader::open(&p).unwrap();
    while let Some(h) = rd.next() {
        rd.skip(&h).unwrap();
        if rd.pos > bytes.len() as u64 / 3 {
            break;
        }
    }
    let mut other = bytes[..head.data_start as usize].to_vec();
    other.extend_from_slice(&bytes[rd.pos as usize..]);
    std::fs::write(&p, &other).unwrap();
    let stale = Index::open(&p).unwrap();
    assert_eq!(stale.frames.len(), 0, "an index made for other records was taken for this journal's");
    check_instants(&p, 24, 46, every, None);
    forget(&p);
    forget(&src);
}

#[test]
fn a_record_must_end_at_rest_to_be_a_keyframe_point() {
    let mut r = Replay::new(80, 24, true);
    let clean = |r: &mut Replay, bytes: &[u8]| {
        r.feed(bytes);
        r.clean
    };
    assert!(clean(&mut r, b"plain text"));
    assert!(clean(&mut r, b"line\r\n"), "trailing control characters do not disturb a parser at rest");
    assert!(clean(&mut r, b"\x1b[31mred\x1b[0m"), "a completed sequence leaves the parser at rest");
    assert!(clean(&mut r, "caf\u{e9}".as_bytes()));
    assert!(!clean(&mut r, b"\x1b[3"), "the middle of a sequence");
    assert!(!clean(&mut r, b"1"), "still inside it");
    assert!(clean(&mut r, b"m"), "its last byte");
    assert!(!clean(&mut r, b"\x1b]0;a title that has not ended"), "inside a string");
    assert!(!clean(&mut r, b" and goes on"));
    assert!(!clean(&mut r, b"\x07"), "a bell ends the string, but one byte alone cannot show that");
    assert!(clean(&mut r, b"x"));
    assert!(!clean(&mut r, &"\u{6f22}".as_bytes()[..2]), "half a character");
    assert!(!clean(&mut r, &"\u{6f22}".as_bytes()[2..]), "its last byte alone is not a character");
    assert!(clean(&mut r, b"!"));
    assert!(!clean(&mut r, b"\x1b[?2026hframe"), "a synchronized update is open: its text is not on the grid yet");
    assert!(r.snapshot(usize::MAX).is_none());
    assert!(clean(&mut r, b" done\x1b[?2026l"), "closed: everything buffered has been applied");
    assert!(r.snapshot(usize::MAX).is_some());
    assert_eq!(screen_text(&r.term)[1], "redcaf\u{e9}x\u{6f22}!frame done");
}

/// A line far longer than a keyframe's scrollback tail gets cut by keyframes.
/// Half a line cannot be re-wrapped the way the whole one wraps, so a seek
/// that has to re-wrap (a viewer of another width) must notice and start
/// from the beginning of the journal instead.
#[test]
fn a_line_longer_than_a_keyframes_tail_survives_being_rewrapped() {
    let file = dir().join("giant.txt");
    let mut text: String = (0..9000).map(|i| format!("{i:05}-")).collect();
    text.push_str("\r\nthe end of the giant line\r\n");
    std::fs::write(&file, &text).unwrap();
    let path = file.to_string_lossy().into_owned();
    let Some(p) = record("giant", "cmd.exe", &["/c", "type", path.as_str()], (100, 30), 60.0, &[], &[]) else {
        panic!("cmd.exe could not be run on a pseudo-terminal");
    };
    let _ = std::fs::remove_file(&file);
    let tl = Timeline::scan(&p).unwrap();
    let mut ix = Index::open(&p).unwrap();
    (ix.every, ix.ratio) = (8 << 10, 0);
    ix.build(&mut |_, _| {}).unwrap();
    assert!(ix.frames.len() >= 3, "{} keyframes", ix.frames.len());
    let mut from_the_start = 0;
    for view in [(90usize, 30usize), (120, 30), (100, 40), (64, 20), (100, 30)] {
        for k in 1..=6u64 {
            let target = Target::Time(tl.duration_us() * k / 6);
            let got = ix.seek(target, Some(view)).unwrap();
            let want = replay_from_start(&p, target, Some(view)).unwrap();
            let what = format!("viewer {view:?} at {target:?} via {}", got.how);
            assert_eq!(screen_text(&want.replay.term), screen_text(&got.replay.term), "{what}");
            from_the_start += (got.how == "full") as usize;
            assert_same(want.replay, got.replay, false, &what);
        }
    }
    assert!(from_the_start > 0, "no seek met a keyframe that cut the line: the test did not test it");
    forget(&p);
}

/// The window's side of time travel: requests go to a worker, only the
/// newest matters, what comes back is the screen a replay from the start
/// gives, and with nothing asked the worker says nothing.
#[test]
fn the_scrub_worker_shows_the_newest_request_and_then_sleeps() {
    let Some(p) = record("scrubber", "cmd.exe", &["/c", "dir", "C:\\Windows\\System32"], (100, 30), 60.0, &[], &[]) else {
        panic!("cmd.exe could not be run on a pseudo-terminal");
    };
    let (tx, woke) = crossbeam_channel::unbounded::<()>();
    let notify: crate::term::Notify = std::sync::Arc::new(move |_| {
        let _ = tx.send(());
    });
    let mut s = crate::scrub::Scrub::open(&p, notify).unwrap();
    let view = (100, 30 - crate::scrub::BAR_ROWS);
    let settle = |s: &mut crate::scrub::Scrub| {
        while s.busy() {
            woke.recv_timeout(Duration::from_secs(20)).expect("the worker never answered");
            s.poll();
        }
    };
    let same_as_replay = |s: &crate::scrub::Scrub, pos: usize| {
        let v = s.view.as_ref().expect("a past screen");
        assert_eq!(v.pos, pos);
        let want = replay_from_start(&p, Target::Pos(pos as u64), Some(view)).unwrap();
        assert_eq!(screen_text(&want.replay.term), screen_text(&v.term));
        assert_eq!(diff(&want.replay.term, &v.term, Some(0)), None);
    };
    // The first key press: one output record back from the present.
    let first = s.tl.step(s.pos, -1).unwrap();
    s.go(first, view);
    settle(&mut s);
    same_as_replay(&s, first);
    // A held key: a burst of requests, further back each time.
    let mut pos = first;
    for i in 0..300 {
        pos = s.tl.step(pos, -(1 + i / 20)).unwrap();
        s.go(pos, view);
    }
    settle(&mut s);
    same_as_replay(&s, pos);
    assert!(s.error.is_none());
    // The bar is made of this journal's numbers.
    let m = crate::fonts::CellMetrics { cell_w: 9.0, cell_h: 20.0, baseline: 15.0, ul_y: 17.0, ul_h: 1.0, strike_y: 10.0 };
    let bar = s.overlay([1200.0, 700.0], [12.0, 12.0], &m, &crate::theme::Theme::void_light(), 33);
    let words: Vec<&str> = bar.texts.iter().map(|t| t.text.as_str()).collect();
    assert!(words.contains(&"PAST") && words.contains(&clock(s.tl.time_at(pos)).as_str()), "{words:?}");
    assert!(words.contains(&format!("output {} of {}", s.tl.outputs_at(pos), s.tl.outs.len()).as_str()), "{words:?}");
    // Its histogram is this session's output and nothing else: one bar for
    // every slice of the session's life that printed something, standing in
    // that slice's place, the tallest where the most was printed, and the
    // playhead where the moment on screen falls.
    let (x0, w, n, pitch) = crate::scrub::track(1200.0, 12.0, m.cell_w);
    let hist = s.tl.histogram(n);
    assert_eq!(hist.iter().sum::<u64>(), s.tl.out_bytes);
    let base = (crate::scrub::band_top(12.0, m.cell_h, 33) + m.cell_h * 1.86).round();
    let slot = |r: &crate::renderer::OverlayRect| ((r.pos[0] - x0) / pitch).round() as usize;
    let bars: Vec<&crate::renderer::OverlayRect> = bar.rects.iter().filter(|r| r.size[0] == pitch - 1.0 && r.pos[1] + r.size[1] == base).collect();
    let printed: Vec<usize> = (0..n).filter(|&i| hist[i] > 0).collect();
    assert!(printed.len() >= 3, "{} of {n} slices printed something", printed.len());
    assert_eq!(bars.iter().map(|r| slot(r)).collect::<Vec<_>>(), printed, "the bars are not where the session printed");
    assert!(bars.iter().all(|r| r.pos[0] == x0 + slot(r) as f32 * pitch));
    let most = bars.iter().max_by_key(|r| hist[slot(r)]).unwrap();
    assert!(bars.iter().all(|r| r.size[1] <= most.size[1]), "a bar stands taller than the one where the most was printed");
    for pair in bars.windows(2) {
        let (a, b) = (hist[slot(pair[0])], hist[slot(pair[1])]);
        assert!((a <= b) == (pair[0].size[1] <= pair[1].size[1]) || pair[0].size[1] == pair[1].size[1], "bar heights contradict the bytes printed: {a} bytes is {} high, {b} bytes is {} high", pair[0].size[1], pair[1].size[1]);
    }
    let head = bar.rects.iter().find(|r| r.glow > 0.0).expect("a playhead").pos[0];
    let want = x0 + s.tl.time_at(pos) as f32 / s.tl.duration_us() as f32 * w;
    assert!((head - want).abs() <= 1.0, "the playhead stands at {head}, the moment on screen is at {want}");
    // Nothing asked: nothing happens. (Leftover wake-ups from the burst are drained first.)
    while woke.try_recv().is_ok() {}
    assert!(woke.recv_timeout(Duration::from_millis(400)).is_err(), "the worker spoke with nothing asked of it");
    drop(s);
    forget(&p);
}

/// A screen cleared to a colour, widened, then cleared to the default must
/// come out clean. (The terminal's own resize leaves the old colour behind in
/// the columns that existed before: `term::resize_term` is why this passes.)
#[test]
fn widening_a_coloured_screen_leaves_no_stale_stripes() {
    let mut r = Replay::new(12, 5, false);
    r.feed(b"\x1b[41m\x1b[2J\x1b[H");
    r.resize(24, 5);
    r.feed(b"\x1b[0m\x1b[H\x1b[J");
    for line in 0..5 {
        for (col, cell) in r.term.grid()[Line(line)][..].iter().enumerate() {
            assert!(is_blank(cell), "line {line} column {col} kept {:?}", cell.bg);
        }
    }
}

/// `resize_term` must show exactly what the terminal's own resize shows, on
/// both screens; it only adds bookkeeping. Checked from many real states.
#[test]
fn our_resize_shows_what_the_terminals_own_resize_shows() {
    let bytes = storm(31337, 7000);
    let mut r = Rng(3);
    let mut live = Replay::new(80, 24, true);
    let (mut i, mut checked) = (0, 0);
    while i < bytes.len() {
        let n = (1 + r.below(300) as usize).min(bytes.len() - i);
        live.feed(&bytes[i..i + n]);
        i += n;
        let Some(snap) = live.snapshot(usize::MAX).filter(|_| r.below(5) == 0) else { continue };
        let (cols, rows) = [(100, 30), (60, 20), (132, 43), (40, 10), (80, 12), (81, 24)][r.below(6) as usize];
        let (mut ours, mut theirs) = (Replay::restore(&snap, false).unwrap(), Replay::restore(&snap, false).unwrap());
        let in_alt = ours.term.mode().contains(TermMode::ALT_SCREEN);
        ours.resize(cols, rows);
        theirs.term.resize(Dim { cols, rows });
        assert_eq!(diff(&ours.term, &theirs.term, None), None, "showing screen after byte {i}, to {cols}x{rows}");
        ours.term.swap_alt();
        theirs.term.swap_alt();
        match diff(&ours.term, &theirs.term, None) {
            None => {}
            // By design: behind the alternate screen the saved cursor follows the cursor.
            Some(d) if in_alt && d.starts_with("saved cursor") => {
                let g = ours.term.grid();
                assert!(cursor_eq(&g.cursor, &g.saved_cursor));
            }
            Some(d) => panic!("hidden screen after byte {i}, to {cols}x{rows}: {d}"),
        }
        checked += 1;
        if r.below(3) == 0 {
            live.resize(cols, rows);
        }
    }
    assert!(checked > 40, "only {checked} resizes checked");
}

/// The repeat sequence repeats the last printed character, which the parser
/// remembers across records: a keyframe has to carry it.
#[test]
fn a_keyframe_remembers_the_character_a_repeat_would_repeat() {
    let mut live = Replay::new(40, 6, true);
    live.feed(b"ab\x1b[2;1H");
    let snap = live.snapshot(usize::MAX).unwrap();
    for tracked in [false, true] {
        let mut back = Replay::restore(&snap, tracked).unwrap();
        back.feed(b"\x1b[3b");
        assert_eq!(screen_text(&back.term)[1], "bbb");
    }
}

// ------------------------------------------------- found by the adversarial review

/// The positions (records applied) at which a synchronized update was open,
/// and at which the alternate screen was up.
fn moments(path: &Path) -> (Vec<u64>, Vec<u64>) {
    let (head, mut rd) = Reader::open(path).unwrap();
    let (cols, rows) = head.size();
    let mut rp = Replay::new(cols, rows, false);
    let (mut buf, mut pos, mut in_sync, mut in_alt) = (Vec::new(), 0u64, Vec::new(), Vec::new());
    while let Some(h) = rd.next() {
        rd.payload(&h, &mut buf).unwrap();
        rp.apply(h.kind, &buf);
        pos += 1;
        if pending(&rp.proc) {
            in_sync.push(pos);
        }
        if rp.term.mode().contains(TermMode::ALT_SCREEN) {
            in_alt.push(pos);
        }
    }
    (in_sync, in_alt)
}

/// Run a PowerShell script on a real pseudo-terminal and journal it.
fn record_script(name: &str, script: &str, size: (u16, u16), resizes: &[(f64, u16, u16)]) -> PathBuf {
    let file = dir().join(format!("{name}.ps1"));
    std::fs::write(&file, script).unwrap();
    let path = file.to_string_lossy().into_owned();
    let args = ["-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", path.as_str()];
    let p = record(name, "powershell.exe", &args, size, 60.0, resizes, &[]).expect("powershell.exe could not be run on a pseudo-terminal");
    let _ = std::fs::remove_file(&file);
    p
}

/// Where `needle` first occurs in `bytes` at or after `from`.
fn offset_of(bytes: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    bytes.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Erasing the scrollback while the alternate screen is up erases nothing:
/// that screen has none, and the main screen's is out of reach. A seek that
/// started from a keyframe (which carries only a tail of the scrollback) must
/// go on knowing that older rows exist. Believing they were erased, it showed
/// a viewer taller than that tail blank rows where the terminal had text, and
/// wrote keyframes that said the same.
#[test]
fn erasing_scrollback_from_the_alternate_screen_does_not_fool_a_seek() {
    let p = record_script(
        "alt3j",
        r#"$e=[char]27
1..400 | % { [Console]::Write("scrollback line $_ of four hundred`r`n") }
Start-Sleep -Milliseconds 300
[Console]::Write("$e[?1049h$e[2J$e[H$e[3Jon the alternate screen")
Start-Sleep -Milliseconds 300
[Console]::Write("$e[?1049l")
[Console]::Write("back on the main screen`r`n")
Start-Sleep -Milliseconds 200
"#,
        (80, 24),
        &[],
    );
    // The journal must hold what this test is about, in this order.
    let bytes = std::fs::read(&p).unwrap();
    let enter = offset_of(&bytes, b"\x1b[?1049h", 0).expect("the alternate screen was never entered");
    let erase = offset_of(&bytes, b"\x1b[3J", enter).expect("the console host did not pass the scrollback erase through");
    assert!(offset_of(&bytes, b"\x1b[?1049l", erase).is_some(), "the alternate screen was never left");
    // Index the flood only: its keyframes carry a tail of the scrollback.
    let (_, mut rd) = Reader::open(&p).unwrap();
    let mut before_alt = 0u64;
    while let Some(h) = rd.next() {
        rd.skip(&h).unwrap();
        if rd.pos > enter as u64 {
            break;
        }
        before_alt += 1;
    }
    let mut ix = Index::open(&p).unwrap();
    (ix.every, ix.ratio) = (2 << 10, 0);
    ix.seek(Target::Pos(before_alt), None).unwrap();
    assert!(!ix.frames.is_empty(), "no keyframe in the flood");
    // Everything after is replayed from the last of them, for a viewer who
    // wants far more scrollback than a keyframe has.
    let tall = Some((80, 24 + HIST_TAIL + 240));
    for target in [Target::End, Target::Pos(before_alt + 1)] {
        let got = ix.seek(target, tall).unwrap();
        let want = replay_from_start(&p, target, tall).unwrap();
        assert_eq!(screen_text(&want.replay.term), screen_text(&got.replay.term), "{target:?} via {}", got.how);
        assert_same(want.replay, got.replay, false, &format!("{target:?} via {}", got.how));
    }
    // And from whatever that left in the sidecar.
    check_instants(&p, 12, 51, 2 << 10, tall);
    forget(&p);
}

/// A full-screen program after a long scroll, in a window of another size
/// (as the scrubbing window always is: it gives three rows to the bar), and
/// resized while it runs. Seeks into it must be served from keyframes: every
/// one of them used to start over from the beginning of the journal, because
/// the scrollback it looked at was the alternate screen's, which is empty.
#[test]
fn a_full_screen_program_after_a_long_scroll_is_reached_from_a_keyframe() {
    let resizes: Vec<(f64, u16, u16)> = (1..=14).map(|i| (i as f64 * 0.3, [84u16, 100, 90][i % 3], [22u16, 30, 26][i % 3])).collect();
    let p = record_script(
        "altview",
        r#"$e=[char]27
1..300 | % { [Console]::Write("scrollback line $_ of three hundred`r`n") }
[Console]::Write("$e[?1049h$e[2J$e[H")
1..60 | % { [Console]::Write("$e[$(1 + $_ % 18);$(1 + ($_ * 3) % 50)Hpainted $_ "); Start-Sleep -Milliseconds 40 }
[Console]::Write("$e[?1049l")
[Console]::Write("back on the main screen`r`n")
1..30 | % { [Console]::Write("tail $_`r`n") }
"#,
        (90, 26),
        &resizes,
    );
    let tl = Timeline::scan(&p).unwrap();
    let (_, in_alt) = moments(&p);
    assert!(in_alt.len() >= 20, "the alternate screen was up for only {} records", in_alt.len());
    let resized_in_alt = tl.evs.iter().enumerate().filter(|(i, e)| e.kind == journal::K_RESIZE && in_alt.contains(&(*i as u64 + 1))).count();
    assert!(resized_in_alt >= 1, "no resize landed while the alternate screen was up");

    let mut ix = Index::open(&p).unwrap();
    (ix.every, ix.ratio) = (2 << 10, 0);
    ix.build(&mut |_, _| {}).unwrap();
    assert_eq!(ix.stats.rejected, 0, "{:?}", ix.first_reject);
    let stride = (in_alt.len() / 40).max(1);
    let (mut from_keyframe, mut from_start) = (0, 0);
    for view in [None, Some((90, 23)), Some((100, 31)), Some((80, 20))] {
        for &pos in in_alt.iter().step_by(stride).chain(in_alt.last()) {
            let target = Target::Pos(pos);
            let got = ix.seek(target, view).unwrap();
            let want = replay_from_start(&p, target, view).unwrap();
            let what = format!("{target:?} viewer {view:?} via {}", got.how);
            assert_eq!(screen_text(&want.replay.term), screen_text(&got.replay.term), "{what}");
            from_keyframe += (got.how == "keyframe") as usize;
            from_start += (got.how == "full") as usize;
            // Both screens: the main one, hidden here, was resized behind the other.
            assert_same(want.replay, got.replay, false, &what);
        }
    }
    assert!(from_keyframe >= 20, "only {from_keyframe} seeks were served from a keyframe");
    assert_eq!(from_start, 0, "{from_start} seeks into the full-screen program started over from the beginning of the journal");
    check_instants(&p, 40, 52, 2 << 10, Some((90, 23)));
    forget(&p);
}

/// `cls` erases the scrollback, so after it nothing older is missing from a
/// replay that began at a keyframe. Seeks to the moments between a `cls` and
/// the next keyframe, for a viewer of another size, used not to know that
/// and started over from the beginning of the journal.
#[test]
fn after_the_scrollback_is_erased_a_seek_still_starts_at_a_keyframe() {
    let typed = [(0.8, "dir C:\\Windows\\System32\r"), (4.0, "cls\r"), (4.6, "echo after-the-clear\r"), (5.4, "dir C:\\Windows\\System32\r"), (8.6, "exit\r")];
    let Some(p) = record("cls", "cmd.exe", &[], (100, 30), 40.0, &[], &typed) else {
        panic!("cmd.exe could not be run on a pseudo-terminal");
    };
    let bytes = std::fs::read(&p).unwrap();
    let erase = offset_of(&bytes, b"\x1b[3J", 0).expect("cls did not erase the scrollback") as u64;
    let seen = find(&p, "echo after-the-clear", false).unwrap();
    let (shown, gone) = (seen.hits[0].appeared_us, seen.hits[0].left_us.expect("the second listing scrolled it away"));
    let mut ix = Index::open(&p).unwrap();
    (ix.every, ix.ratio) = (64 << 10, 0);
    ix.build(&mut |_, _| {}).unwrap();
    let before = ix.frames.iter().filter(|f| f.off < erase).count();
    assert!(before >= 2 && ix.frames.len() > before, "{before} keyframes before the erase, {} in all", ix.frames.len());
    assert!(ix.frames[before].t_us > gone, "a keyframe was taken while the line was on screen: nothing to test");
    // The window's view of a 100x30 session: three rows go to the bar.
    let view = Some((100, 27));
    for k in 0..=8u64 {
        let target = Target::Time(shown + (gone - 1 - shown) * k / 8);
        let got = ix.seek(target, view).unwrap();
        assert_eq!(got.how, "keyframe", "{target:?}: {} journal bytes replayed", got.replayed);
        let want = replay_from_start(&p, target, view).unwrap();
        // (Later in the stretch the second listing has pushed the line above a viewer three rows shorter.)
        assert!(k > 0 || screen_text(&got.replay.term).join("\n").contains("after-the-clear"));
        assert_same(want.replay, got.replay, false, &format!("{target:?}"));
    }
    check_instants(&p, 40, 53, 64 << 10, view);
    forget(&p);
}

/// A synchronized update is not on screen until it closes: the window waits
/// for it, 150 ms at most. A seek to a moment inside one must show what the
/// window showed then (the screen as it stood before the update began) and
/// not a half-drawn frame that nobody ever saw.
#[test]
fn a_seek_inside_a_synchronized_update_shows_what_the_window_showed() {
    let p = record_script(
        "syncshown",
        r#"$e=[char]27
[Console]::Write("$e[2J$e[H")
1..10 | % {
  $k = $_
  [Console]::Write("$e[?2026h$e[H$e[2J")
  1..5 | % { [Console]::Write("frame $k row $_ of ten`r`n") }
  Start-Sleep -Milliseconds 20
  6..10 | % { [Console]::Write("frame $k row $_ of ten`r`n") }
  [Console]::Write("frame $k is whole$e[?2026l")
  Start-Sleep -Milliseconds 60
}
[Console]::Write("$e[?2026h$e[H$e[2Jslow update, first half`r`n")
Start-Sleep -Milliseconds 700
[Console]::Write("slow update, second half$e[?2026l")
Start-Sleep -Milliseconds 100
"#,
        (80, 24),
        &[],
    );
    // Where the updates are, read from the journal by a plain replay: for each
    // position, how long the update open there had been open (if one was).
    let (head, mut rd) = Reader::open(&p).unwrap();
    let (cols, rows) = head.size();
    let mut plain = Replay::new(cols, rows, false);
    let (mut buf, mut since, mut open_for, mut slow_since) = (Vec::new(), None::<u64>, vec![None::<u64>], None::<u64>);
    while let Some(h) = rd.next() {
        rd.payload(&h, &mut buf).unwrap();
        plain.apply(h.kind, &buf);
        since = match (pending(&plain.proc), since) {
            (false, _) => None,
            (true, None) => Some(h.t_us),
            (true, Some(_)) if offset_of(&buf, b"\x1b[?2026h", 0).is_some() => Some(h.t_us),
            (true, s) => s,
        };
        open_for.push(since.map(|s| h.t_us - s));
        if offset_of(&buf, b"slow update, first half", 0).is_some() {
            slow_since = since;
        }
    }
    let inside = open_for.iter().filter(|o| o.is_some_and(|us| us < 150_000)).count();
    assert!(inside >= 10, "only {inside} records fell inside a synchronized update: nothing to test");

    let mut ix = Index::open(&p).unwrap();
    (ix.every, ix.ratio) = (1 << 10, 0);
    let frames_on = |lines: &[String]| -> Vec<(u32, usize, bool)> {
        (1..=10u32)
            .filter_map(|k| {
                let n = lines.iter().filter(|l| l.starts_with(&format!("frame {k} row "))).count();
                let whole = lines.iter().any(|l| l.starts_with(&format!("frame {k} is whole")));
                (n > 0 || whole).then_some((k, n, whole))
            })
            .collect()
    };
    let mut checked = 0;
    for (pos, open) in open_for.iter().enumerate() {
        // (An update open for 150 ms or more is shown as far as it has got: the window stopped waiting.)
        if open.is_some_and(|us| us >= 150_000) {
            continue;
        }
        let got = ix.seek(Target::Pos(pos as u64), None).unwrap();
        let on = frames_on(&screen_text(&got.replay.term));
        assert!(on.len() <= 1 && on.iter().all(|&(_, n, whole)| n == 10 && whole), "after record {pos} (update open for {open:?} us) the screen held a half-drawn frame: {on:?}");
        checked += open.is_some() as usize;
    }
    assert!(checked >= 10, "{checked}");

    // The slow one: unseen while the window waited, shown as far as it had got once it stopped waiting.
    let began = slow_since.expect("the slow update was not found in the journal");
    let text = |ix: &mut Index, target| screen_text(&ix.seek(target, None).unwrap().replay.term).join("\n");
    let early = text(&mut ix, Target::Time(began + 60_000));
    assert!(!early.contains("slow update") && early.contains("frame 10 is whole"), "60 ms into the slow update: {early}");
    let late = text(&mut ix, Target::Time(began + 350_000));
    assert!(late.contains("slow update, first half") && !late.contains("second half"), "350 ms into the slow update: {late}");
    let end = text(&mut ix, Target::End);
    assert!(end.contains("slow update, first half") && end.contains("slow update, second half"), "at the end: {end}");

    check_instants(&p, 80, 55, 1 << 10, None);
    check_instants(&p, 40, 56, 1 << 10, Some((80, 21)));
    forget(&p);
}

/// Every record of a journal: kind, time, payload.
fn records_of(path: &Path) -> Vec<(u8, u64, Vec<u8>)> {
    let (_, mut rd) = Reader::open(path).unwrap();
    let (mut buf, mut out) = (Vec::new(), Vec::new());
    while let Some(h) = rd.next() {
        rd.payload(&h, &mut buf).unwrap();
        out.push((h.kind, h.t_us, buf.clone()));
    }
    out
}

/// The time of the first output record at or after record `from` (counted
/// from 0) that holds `needle`, and which record that is.
fn when(recs: &[(u8, u64, Vec<u8>)], needle: &[u8], from: usize) -> Option<(usize, u64)> {
    recs.iter().enumerate().skip(from).find(|(_, r)| r.0 == journal::K_OUTPUT && offset_of(&r.2, needle, 0).is_some()).map(|(i, r)| (i, r.1))
}

/// The same session written out again with record `rec` cut in two after
/// `at` bytes, both halves keeping its time. Nothing about the session
/// changes: a pseudo-terminal hands output over in whatever pieces a read
/// happens to end at, and this is one more way it could have been cut.
fn recut(src: &Path, name: &str, rec: usize, at: usize) -> PathBuf {
    let (head, _) = Reader::open(src).unwrap();
    let mut out = std::fs::read(src).unwrap()[..head.data_start as usize].to_vec();
    for (i, (kind, t_us, data)) in records_of(src).iter().enumerate() {
        let pieces: Vec<&[u8]> = if i == rec { vec![&data[..at], &data[at..]] } else { vec![&data[..]] };
        for piece in pieces {
            out.push(*kind);
            out.extend_from_slice(&t_us.to_le_bytes());
            out.extend_from_slice(&(piece.len() as u32).to_le_bytes());
            out.extend_from_slice(piece);
        }
    }
    let p = dir().join(format!("{name}.prtj"));
    let _ = std::fs::remove_file(sidecar(&p));
    std::fs::write(&p, out).unwrap();
    p
}

/// Record a script whose pauses matter, again if the machine was too busy
/// for them to come out as `timed` needs (given the journal's records).
fn record_timed(name: &str, script: &str, timed: impl Fn(&[(u8, u64, Vec<u8>)]) -> bool) -> PathBuf {
    for attempt in 1..=4 {
        let p = record_script(name, script, (80, 24), &[]);
        if timed(&records_of(&p)) {
            return p;
        }
        eprintln!("{name}: try {attempt}, the script's pauses did not come out as asked; recording it again");
        forget(&p);
    }
    panic!("{name}: the machine is too busy to time a script's pauses to within 80 ms");
}

const BSU: &[u8] = b"\x1b[?2026h";

/// A program may begin a synchronized update again while one is open, and
/// the window then waits afresh. A replay has to know that wherever the
/// records happen to be cut: here the second "begin" is cut in two, which a
/// search of each record for it does not see. Not knowing, a seek between
/// the two deadlines showed the half-drawn update the window was still
/// holding back.
#[test]
fn an_update_begun_again_is_waited_for_again_wherever_the_record_was_cut() {
    let script = r#"$e=[char]27
[Console]::Write("$e[2J$e[Hbefore the update`r`n")
Start-Sleep -Milliseconds 300
[Console]::Write("$e[?2026hpart one`r`n")
Start-Sleep -Milliseconds 60
[Console]::Write("$e[?2026hpart two`r`n")
Start-Sleep -Milliseconds 140
[Console]::Write("part three$e[?2026l")
Start-Sleep -Milliseconds 300
"#;
    // (first begin, second begin, close): record and time of each.
    let marks = |recs: &[(u8, u64, Vec<u8>)]| {
        let one = when(recs, b"part one", 0)?;
        let two = when(recs, b"part two", one.0 + 1)?;
        let three = when(recs, b"part three", two.0 + 1)?;
        Some((one, two, three))
    };
    // The second begin must fall inside the first one's wait, and the close after that wait would have ended.
    let p = record_timed("syncagain", script, |recs| marks(recs).is_some_and(|(one, two, three)| two.1 - one.1 < 140_000 && three.1 - one.1 > 170_000));
    let recs = records_of(&p);
    let (one, two, three) = marks(&recs).unwrap();
    let at = offset_of(&recs[two.0].2, BSU, 0).expect("the console host did not pass the second begin through");
    assert!(offset_of(&recs[one.0].2, BSU, 0).is_some() && offset_of(&recs[three.0].2, ESU, 0).is_some());
    let cut = recut(&p, "syncagain-cut", two.0, at + 5);
    assert_eq!(records_of(&cut).iter().filter(|r| offset_of(&r.2, BSU, 0).is_some()).count(), 1, "the second begin was not cut in two");

    for path in [&p, &cut] {
        let mut ix = Index::open(path).unwrap();
        (ix.every, ix.ratio) = (1 << 10, 0);
        let mut text = |target| screen_text(&ix.seek(target, None).unwrap().replay.term).join("\n");
        // After the first deadline, before the second: the window is still waiting.
        let waiting = text(Target::Time(one.1 + 160_000));
        assert!(waiting.contains("before the update") && !waiting.contains("part"), "{}: 160 ms after the first begin, {} ms after the second: {waiting}", path.display(), (one.1 + 160_000 - two.1) / 1000);
        let closed = text(Target::Time(three.1));
        assert!(["part one", "part two", "part three"].iter().all(|s| closed.contains(s)), "{}: once closed: {closed}", path.display());
        check_instants(path, 40, 57, 1 << 10, None);
        check_instants(path, 20, 58, 1 << 10, Some((80, 21)));
    }
    // Cut differently, the session is still the same session: `find` sees the same.
    for needle in ["part one", "part three", "before the update"] {
        assert_eq!(find(&p, needle, false).unwrap().hits, find(&cut, needle, false).unwrap().hits, "`{needle}`");
    }
    forget(&cut);
    forget(&p);
}

/// The window waits 150 ms for a synchronized update to close and then
/// shows what has arrived, whatever the program does next. A replay goes by
/// the same rule on the journal's clock: what was held back is on screen
/// from that moment, for a seek and for `find` alike, also when the program
/// begins another update without closing the first, and when it ends with
/// one still open.
#[test]
fn an_update_left_open_is_on_screen_from_when_the_window_stopped_waiting() {
    let script = r#"$e=[char]27
[Console]::Write("$e[2J$e[Hbefore the update`r`n")
Start-Sleep -Milliseconds 300
[Console]::Write("$e[?2026hheld back at first`r`n")
Start-Sleep -Milliseconds 400
[Console]::Write("$e[?2026hbegun again`r`n")
Start-Sleep -Milliseconds 400
[Console]::Write("closed at last$e[?2026l`r`n")
Start-Sleep -Milliseconds 300
[Console]::Write("$e[?2026hnever closed")
Start-Sleep -Milliseconds 400
"#;
    let marks = |recs: &[(u8, u64, Vec<u8>)]| {
        let first = when(recs, b"held back at first", 0)?;
        let again = when(recs, b"begun again", first.0 + 1)?;
        let closed = when(recs, b"closed at last", again.0 + 1)?;
        let open = when(recs, b"never closed", closed.0 + 1)?;
        Some((first.1, again.1, closed.1, open.1, recs.last()?.1))
    };
    // Each pause must have outlasted the window's wait by a margin.
    let p = record_timed("syncopen", script, |recs| marks(recs).is_some_and(|(a, b, c, d, end)| b - a > 230_000 && c - b > 230_000 && d > c && end - d > 230_000));
    let recs = records_of(&p);
    let (first, again, closed, open, _) = marks(&recs).unwrap();
    assert_eq!(recs.iter().filter(|r| offset_of(&r.2, BSU, 0).is_some()).count(), 3, "the console host did not pass the updates through as written");
    assert!(recs.iter().all(|r| r.1 < open || offset_of(&r.2, ESU, 0).is_none()), "the last update was closed after all");

    let wait = SYNC_WAIT_US;
    let mut ix = Index::open(&p).unwrap();
    (ix.every, ix.ratio) = (1 << 10, 0);
    let mut text = |target| screen_text(&ix.seek(target, None).unwrap().replay.term).join("\n");
    let has = |screen: &str, yes: &[&str], no: &[&str]| yes.iter().all(|s| screen.contains(s)) && !no.iter().any(|s| screen.contains(s));
    // While the window waits, nothing of the update; from the deadline on, what had arrived.
    let s = text(Target::Time(first + wait - 1));
    assert!(has(&s, &["before the update"], &["held back"]), "just before the first deadline: {s}");
    let s = text(Target::Time(first + wait));
    assert!(has(&s, &["held back at first"], &["begun again"]), "at the first deadline: {s}");
    // A second begin after the deadline is a new update: the first one's text stays, the second's waits its own turn.
    let s = text(Target::Time(again + 30_000));
    assert!(has(&s, &["held back at first"], &["begun again"]), "30 ms into the second update: {s}");
    let s = text(Target::Time(again + wait));
    assert!(has(&s, &["held back at first", "begun again"], &["closed at last"]), "at the second deadline: {s}");
    let s = text(Target::Time(closed));
    assert!(has(&s, &["begun again", "closed at last"], &["never closed"]), "once closed: {s}");
    // The one the program never closed.
    let s = text(Target::Time(open + wait - 1));
    assert!(has(&s, &["closed at last"], &["never closed"]), "just before the last deadline: {s}");
    let s = text(Target::Time(open + wait));
    assert!(has(&s, &["never closed"], &[]), "at the last deadline: {s}");
    let s = text(Target::End);
    assert!(has(&s, &["never closed"], &[]), "at the end: {s}");

    // `find` reports the same moments, and the screen at each agrees with it.
    for (needle, began) in [("held back at first", first), ("begun again", again), ("never closed", open)] {
        let hits = find(&p, needle, false).unwrap().hits;
        assert_eq!(hits.len(), 1, "`{needle}`: {hits:?}");
        assert_eq!(hits[0].appeared_us, began + wait, "`{needle}` appeared when the window stopped waiting for it");
        assert_eq!(hits[0].left_us, None, "`{needle}`");
        assert!(text(Target::Time(hits[0].appeared_us)).contains(needle), "`{needle}` is not on screen when find says it appeared");
        assert!(!text(Target::Time(hits[0].appeared_us - 1)).contains(needle), "`{needle}` is on screen before find says it appeared");
    }
    assert!(check_find(&p, &["held back", "begun again", "closed at last", "never closed", "before the update"]) >= 5);
    check_instants(&p, 60, 59, 1 << 10, None);
    check_instants(&p, 30, 60, 1 << 10, Some((80, 21)));
    forget(&p);
}

/// A moment the command line prints must be that same moment when the
/// command line reads it back. `find` printed to the millisecond and `at`
/// cut the fraction short, so a moment `find` reported, handed to `at`, could
/// fall just before the record that put the text on screen.
#[test]
fn a_moment_find_prints_is_the_moment_at_reads() {
    let typed = [(1.0, "echo alpha-marker\r"), (1.8, "cls\r"), (2.6, "echo beta-marker\r"), (3.3, "dir C:\\Windows\\System32\r"), (6.5, "exit\r")];
    let Some(p) = record("moments", "cmd.exe", &[], (110, 30), 40.0, &[], &typed) else {
        panic!("cmd.exe could not be run on a pseudo-terminal");
    };
    // Every record's time survives being printed and read back.
    let recs = records_of(&p);
    assert!(recs.len() > 1000 && recs.iter().any(|r| r.1 % 1000 != 0), "{} records", recs.len());
    for (_, t_us, _) in &recs {
        let printed = serde_json::to_string(&seconds(*t_us)).unwrap();
        assert_eq!(micros(printed.parse::<f64>().unwrap()), *t_us, "printed as {printed}");
    }
    // And through the command line's own two ends: what `find --json` prints
    // for a moment, given to `at --until` as text.
    let screen = |until: &str| screen_text(&crate::recall_cli::seek_until(&p, until).unwrap().replay.term).join("\n");
    let mut checked = 0;
    for needle in ["alpha-marker", "beta-marker", "notepad.exe", "File(s)", "echo"] {
        let report = crate::recall_cli::find_report(&p, needle, false).unwrap();
        let hits = report["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "`{needle}` was never seen");
        for (i, h) in hits.iter().enumerate() {
            // (Text a flood carried past inside one record was never on screen at the end of any record.)
            if h["left"] == h["appeared"] {
                continue;
            }
            let (seen, gone) = (h["appeared"].to_string(), h["left"].to_string());
            assert!(screen(&seen).contains(needle), "`{needle}` is not on screen at {seen}, where find says it appeared");
            // (Unless it came back within the same microsecond it left.)
            let back_at_once = hits.get(i + 1).is_some_and(|next| next["appeared"] == h["left"]);
            if gone != "null" && !back_at_once {
                assert!(!screen(&gone).contains(needle), "`{needle}` is still on screen at {gone}, where find says it left");
            }
            checked += 1;
        }
    }
    assert!(checked >= 5, "only {checked} moments were checked");
    forget(&p);
}

/// A journal still being written by a running program: seeks, the record
/// table and the index must each see a prefix of it that ends at a whole
/// record, and what they saw must be what the finished journal says was
/// there at that point.
#[test]
fn a_journal_is_read_correctly_while_it_grows() {
    let script = dir().join("growing.cmd");
    // Two long listings of known size with a pause between them, printed by a
    // real program through the real console host. (Listing a system folder
    // instead made the size depend on the machine: it failed on CI.)
    let data = dir().join("growing.txt");
    let line = "0123456789 the quick brown fox jumps over the lazy dog 0123456789 abcdefghijklmnopqrstuvwxyz\r\n";
    std::fs::write(&data, line.repeat(2_400)).unwrap();
    let d = data.to_string_lossy().into_owned();
    std::fs::write(&script, format!("@echo off\r\ntype \"{d}\"\r\nping -n 2 127.0.0.1 >nul\r\ntype \"{d}\"\r\n")).unwrap();
    let p = dir().join("growing.prtj");
    let _ = std::fs::remove_file(sidecar(&p));
    let _ = std::fs::remove_file(&p);
    let (out, file) = (p.clone(), script.to_string_lossy().into_owned());
    let writer = std::thread::spawn(move || {
        let args = vec!["/c".to_string(), file];
        crate::recall_cli::record_to(&out, "cmd.exe", &args, (110, 32), Duration::from_secs(60), vec![(0.5, 96, 28), (1.4, 120, 36)], Vec::new()).map(|r| r.0).map_err(|e| e.to_string())
    });
    let view = Some((100, 29));
    // One index and one record table kept open for the whole session, as the
    // window keeps them, and a fresh index each round, as another process would open one.
    let (mut ix, mut tl): (Option<Index>, Option<Timeline>) = (None, None);
    let mut seen: Vec<(Target, Seeked)> = Vec::new();
    let mut lens: Vec<usize> = Vec::new();
    let mut rng = Rng(0x9e37);
    while !writer.is_finished() {
        if ix.is_none() {
            // The file may not exist yet, or not hold its header yet.
            ix = Index::open(&p).ok().map(|mut i| {
                (i.every, i.ratio) = (16 << 10, 0);
                i
            });
        }
        if tl.is_none() {
            tl = Timeline::scan(&p).ok();
        }
        if let (Some(ix), Some(tl)) = (ix.as_mut(), tl.as_mut()) {
            tl.refresh(&p).unwrap();
            lens.push(tl.evs.len());
            let now = ix.seek(Target::End, view).unwrap();
            assert!(now.pos as usize >= tl.evs.len(), "the index saw {} records after the table saw {}", now.pos, tl.evs.len());
            let earlier = Target::Pos(rng.below(now.pos + 1));
            let mut other = Index::open(&p).unwrap();
            (other.every, other.ratio) = (16 << 10, 0);
            let then = other.seek(earlier, view).unwrap();
            if seen.len() < 120 {
                seen.push((Target::Pos(now.pos), now));
                seen.push((earlier, then));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let bytes = writer.join().unwrap().expect("cmd.exe could not be run on a pseudo-terminal");
    let _ = std::fs::remove_file(&script);
    let _ = std::fs::remove_file(&data);
    assert!(bytes > 300_000, "expected two long listings, got {bytes} bytes");
    let (mut ix, mut tl) = (ix.expect("the journal never became readable"), tl.expect("the journal never became readable"));
    let mut steps = lens.clone();
    steps.dedup();
    assert!(lens.windows(2).all(|w| w[0] <= w[1]), "the record table shrank");
    assert!(steps.len() >= 4, "the journal was seen at only {} lengths: it did not grow while it was read", steps.len());

    // Everything seen on the way was a true moment of the finished journal.
    for (target, got) in seen {
        let want = replay_from_start(&p, target, view).unwrap();
        assert_eq!((got.pos, got.t_us), (want.pos, want.t_us), "{target:?}");
        assert_eq!(screen_text(&want.replay.term), screen_text(&got.replay.term), "{target:?} via {}", got.how);
        assert_same(want.replay, got.replay, false, &format!("{target:?} via {} while the journal grew", got.how));
    }
    // The table kept up to date piece by piece is the table read at once.
    tl.refresh(&p).unwrap();
    let whole = Timeline::scan(&p).unwrap();
    assert_eq!((tl.evs.len(), tl.outs.len(), tl.end, tl.out_bytes, tl.exit), (whole.evs.len(), whole.outs.len(), whole.end, whole.out_bytes, whole.exit));
    assert_eq!(whole.end, std::fs::metadata(&p).unwrap().len(), "the finished journal ends at a whole record");
    assert!(tl.evs.iter().zip(&whole.evs).all(|(a, b)| (a.t_us, a.len, a.kind) == (b.t_us, b.len, b.kind)));
    // The index that grew with it serves the end, and its sidecar holds its keyframes.
    let end = ix.seek(Target::End, view).unwrap();
    assert!(end.at_end && end.pos as usize == whole.evs.len());
    assert_same(replay_from_start(&p, Target::End, view).unwrap().replay, end.replay, false, "the end, from the index that grew with the journal");
    assert!(ix.frames.len() >= 4 && ix.stats.rejected == 0, "{} keyframes, {:?}", ix.frames.len(), ix.first_reject);
    assert_eq!(Index::open(&p).unwrap().frames.len(), ix.frames.len(), "the sidecar does not hold what the index holds");
    check_instants(&p, 24, 54, 16 << 10, view);
    forget(&p);
}

/// While the past is on screen, what the live session prints must not draw a
/// frame; only a past screen arriving may. This is the test the window's
/// event loop applies to every wake-up (`scrub::wake_draws`).
#[test]
fn a_wake_up_draws_a_frame_in_the_past_only_for_news_from_the_past() {
    use crate::scrub::{wake_draws, Scrub};
    let Some(p) = record("wake", "cmd.exe", &["/c", "dir", "C:\\Windows\\System32"], (100, 30), 60.0, &[], &[]) else {
        panic!("cmd.exe could not be run on a pseudo-terminal");
    };
    let (tx, woke) = crossbeam_channel::unbounded::<()>();
    let notify: crate::term::Notify = std::sync::Arc::new(move |_| {
        let _ = tx.send(());
    });
    // Not in the past: every wake-up is the live session's and is drawn.
    assert!(wake_draws(None));
    let mut s = Scrub::open(&p, notify).unwrap();
    // In the past with nothing asked: the live session's output draws nothing.
    assert!(!wake_draws(Some(&s)));
    let back = s.tl.step(s.pos, -3).unwrap();
    s.go(back, (100, 27));
    // The worker's own wake-up, once the past screen is ready, does draw.
    woke.recv_timeout(Duration::from_secs(20)).expect("the worker never answered");
    assert!(wake_draws(Some(&s)));
    while {
        s.poll();
        s.busy()
    } {
        woke.recv_timeout(Duration::from_secs(20)).expect("the worker never finished");
    }
    assert!(s.view.is_some());
    // That frame drawn, the window is idle again however much the live session prints.
    assert!(!s.busy() && !wake_draws(Some(&s)));
    while woke.try_recv().is_ok() {}
    assert!(woke.recv_timeout(Duration::from_millis(300)).is_err(), "the worker spoke with nothing asked of it");
    assert!(!wake_draws(Some(&s)));
    drop(s);
    forget(&p);
}

// ------------------------------------------------------------------- the soak

/// Positions worth aiming at on purpose: the first and last records, both
/// sides of every resize, and a sample of the moments a synchronized update
/// was open and of the moments the alternate screen was up.
fn hard_positions(path: &Path, tl: &Timeline, sample: usize) -> (Vec<u64>, usize, usize) {
    let n = tl.evs.len() as u64;
    let mut out: Vec<u64> = vec![0, 1, 2, n.saturating_sub(2), n.saturating_sub(1), n];
    out.extend(tl.outs.first().map(|&p| p as u64));
    for (i, e) in tl.evs.iter().enumerate() {
        if e.kind == journal::K_RESIZE {
            out.extend([i as u64, i as u64 + 1, i as u64 + 2]);
        }
    }
    let (in_sync, in_alt) = moments(path);
    let (n_sync, n_alt) = (in_sync.len(), in_alt.len());
    for set in [in_sync, in_alt] {
        let stride = (set.len() / sample.max(1)).max(1);
        out.extend(set.iter().step_by(stride));
        out.extend(set.first());
        out.extend(set.last());
    }
    out.retain(|&p| p <= n);
    out.sort_unstable();
    out.dedup();
    (out, n_sync, n_alt)
}

/// Seek against a replay from the start, on journals of real sessions named
/// in PROTEUS_SOAK_JOURNALS (separated by ';'): PROTEUS_SOAK_N seeded
/// instants each (default 200) plus every hard position, with the keyframe
/// spacing the program really uses and with a close one, with no viewer and
/// with viewers of other sizes. Both screens, the scrollback both sides hold
/// and the private state must be the same. Prints how the seeks were served
/// and what they cost.
///
///   $env:PROTEUS_SOAK_JOURNALS = "a.prtj;b.prtj"
///   cargo test --release soak_real_journals -- --ignored --nocapture
#[test]
#[ignore = "needs PROTEUS_SOAK_JOURNALS, journals of real sessions"]
fn soak_real_journals() {
    let list = std::env::var("PROTEUS_SOAK_JOURNALS").expect("set PROTEUS_SOAK_JOURNALS to journal files separated by ';'");
    let n: usize = std::env::var("PROTEUS_SOAK_N").ok().and_then(|s| s.parse().ok()).unwrap_or(200);
    let big: u64 = 8 << 20;
    for path in list.split(';').filter(|s| !s.is_empty()).map(PathBuf::from) {
        let tl = Timeline::scan(&path).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let (cols, rows) = tl.head.size();
        let (hard, n_sync, n_alt) = hard_positions(&path, &tl, 24);
        let resizes = tl.evs.iter().filter(|e| e.kind == journal::K_RESIZE).count();
        eprintln!(
            "{}: {} bytes, {} records, {} resizes, {} records inside a synchronized update, {} with the alternate screen up; {} hard positions",
            path.display(), len, tl.evs.len(), resizes, n_sync, n_alt, hard.len()
        );
        // The window's view (the grid less the bar), then a larger and a smaller viewer.
        let mut views = vec![None, Some((cols, rows.saturating_sub(3).max(1)))];
        if len <= big {
            views.extend([Some((cols + 13, rows + 7)), Some((cols.saturating_sub(9).max(2), rows.saturating_sub(5).max(1)))]);
        }
        let spacings: &[(u64, u64)] = if len <= big { &[(KF_BYTES, KF_RATIO), (24 << 10, 0)] } else { &[(KF_BYTES, KF_RATIO)] };
        for &(every, ratio) in spacings {
            let _ = std::fs::remove_file(sidecar(&path));
            let mut ix = Index::open(&path).unwrap();
            (ix.every, ix.ratio) = (every, ratio);
            let t0 = std::time::Instant::now();
            ix.build(&mut |_, _| {}).unwrap();
            let build_ms = t0.elapsed().as_secs_f64() * 1e3;
            assert_eq!(ix.stats.rejected, 0, "a keyframe did not reproduce its terminal: {:?}", ix.first_reject);
            let mut rng = Rng(0x5eed ^ every);
            let mut targets: Vec<Target> = hard.iter().map(|&p| Target::Pos(p)).collect();
            for i in 0..n {
                targets.push(if i % 2 == 0 { Target::Time(rng.below(tl.duration_us() + 1)) } else { Target::Pos(rng.below(tl.evs.len() as u64 + 1)) });
            }
            for (vi, view) in views.iter().enumerate() {
                // The hard positions go to every viewer; the seeded ones are shared out on a large journal.
                let mut served: std::collections::BTreeMap<&'static str, (usize, f64, f64)> = Default::default();
                let mut slowest = (0.0f64, String::new());
                for (i, target) in targets.iter().enumerate() {
                    if len > big && i >= hard.len() && vi > 0 && i % 2 == 1 {
                        continue;
                    }
                    let t1 = std::time::Instant::now();
                    let got = ix.seek(*target, *view).unwrap();
                    let ms = t1.elapsed().as_secs_f64() * 1e3;
                    let e = served.entry(got.how).or_insert((0, 0.0, 0.0));
                    (e.0, e.1, e.2) = (e.0 + 1, e.1 + ms, e.2.max(ms));
                    if ms > slowest.0 {
                        slowest = (ms, format!("{target:?} via {}, {} journal bytes replayed", got.how, got.replayed));
                    }
                    let want = replay_from_start(&path, *target, *view).unwrap();
                    let what = format!("{} {target:?} viewer {view:?} via {} (keyframes every {every})", path.display(), got.how);
                    assert_eq!((got.pos, got.t_us, got.at_end), (want.pos, want.t_us, want.at_end), "{what}");
                    let (mut want, mut got) = (want.replay, got.replay);
                    for side in ["showing", "hidden"] {
                        let depth = want.term.history_size().min(got.term.history_size());
                        if let Some(d) = diff(&want.term, &got.term, Some(depth)) {
                            panic!("{what}: the {side} screen differs: {d}");
                        }
                        want.term.swap_alt();
                        got.term.swap_alt();
                    }
                    // And what a synchronized update still open was holding back.
                    want.settle();
                    got.settle();
                    let depth = want.term.history_size().min(got.term.history_size());
                    if let Some(d) = diff(&want.term, &got.term, Some(depth)) {
                        panic!("{what}: with the open update put on screen: {d}");
                    }
                    assert_eq!(private_state(want), private_state(got), "{what}: private state");
                }
                let line: Vec<String> = served.iter().map(|(how, (k, sum, max))| format!("{how} x{k} mean {:.2} ms max {:.2} ms", sum / *k as f64, max)).collect();
                eprintln!("  keyframes every {every} ({} of them, built in {build_ms:.0} ms), viewer {view:?}: {}", ix.frames.len(), line.join("; "));
                eprintln!("    slowest: {:.2} ms for {}", slowest.0, slowest.1);
            }
        }
        let _ = std::fs::remove_file(sidecar(&path));
    }
}
