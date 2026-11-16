//! Tests of `world.rs` (its `tests` module, kept in a file of its own).
//! Layout, grouping and the timeline are checked on ledgers built for the
//! purpose; the two recordings in `forge/samples` are real sessions.
use super::*;
use crate::procs::{Exit, Start};

fn start(pid: u32, ppid: u32, t: u64, cmd: &str) -> ProcEvent {
    ProcEvent::Start(Start {
        pid,
        ppid: Some(ppid),
        image: Some(format!("C:\\bin\\{}.exe", cmd.split(' ').next().unwrap())),
        cmd: Some(cmd.into()),
        start_us: Some(t),
        seen_us: t + 40,
    })
}

fn exit(pid: u32, t: u64, code: u32) -> ProcEvent {
    ProcEvent::Exit(Exit { pid, code: Some(code), end_us: Some(t), user_us: Some(1500), kernel_us: Some(500), abnormal: false, seen_us: t + 25 })
}

/// The real cell sizes of the embedded font at a font size.
fn ty(px: f32) -> Type {
    let f = crate::fonts::FontSet::new(px);
    let cell = |s: Size| {
        let m = f.metrics_at(s as u8);
        [m.cell_w, m.cell_h]
    };
    Type { cell: [cell(Size::Body), cell(Size::Small), cell(Size::Large)], unit: px / 15.0 }
}

const T0: u64 = 1_794_800_000_000_000;

/// The example in the brief: a build that started 45 compilers, of which
/// 42 ended well, one failed and two are still running.
fn build_ledger() -> Ledger {
    let mut l = Ledger::new();
    l.apply(&start(100, 1, T0, "pwsh -NoLogo"));
    l.apply(&start(200, 100, T0 + SEC, "cargo build --release"));
    for i in 0..45u32 {
        let t = T0 + 2 * SEC + i as u64 * 100_000;
        l.apply(&start(1000 + i, 200, t, &format!("rustc --crate-name c{i} src/lib.rs")));
        match i {
            30 => drop(l.apply(&exit(1000 + i, t + 90_000, 1))),
            43 | 44 => {}
            _ => drop(l.apply(&exit(1000 + i, t + 90_000, 0))),
        }
    }
    l
}

fn frame<'a>(ledger: &'a Ledger, activity: &'a Activity, theme: &'a Theme, size: [f32; 2], px: f32, t1: u64) -> Frame<'a> {
    Frame { ledger, activity, t0_us: T0, t1_us: t1, session: "test", ended: None, unplaced: 0, zoom: None, hover: None, born: &[], failed: &[], size, pad: 12.0 * px / 15.0, ty: ty(px), theme }
}

/// How many processes a drawn tree accounts for.
fn accounted(f: &Forest, shown: &[Shown]) -> usize {
    shown
        .iter()
        .map(|s| match &s.what {
            What::Proc { inside, .. } => 1 + inside,
            What::Group { members, .. } => members.iter().map(|&m| 1 + f.desc[m as usize] as usize).sum(),
        })
        .sum()
}

fn rows_of(shown: &[Shown]) -> usize {
    shown.iter().filter(|s| s.parent.is_none()).map(|s| s.rows).sum()
}

#[test]
fn finished_identical_siblings_are_counted_and_failures_and_running_stay_expanded() {
    let ledger = build_ledger();
    let f = Forest::of(&ledger);
    let (shown, level) = arrange(&f, 10, 6);
    assert!(level > 0, "47 processes cannot be drawn one per row in 10 rows");
    assert!(rows_of(&shown) <= 10, "{} rows", rows_of(&shown));
    assert_eq!(accounted(&f, &shown), 47, "every process is a node, in a group, or folded into a node");
    let own = |pid: u32| shown.iter().any(|s| matches!(s.what, What::Proc { index, .. } if f.procs[index].pid == pid));
    assert!(own(1030), "the failure has a node of its own");
    assert!(own(1043) && own(1044), "what is running has nodes of its own");
    let groups: Vec<(u8, Option<String>, Vec<u32>)> = shown
        .iter()
        .filter_map(|s| match &s.what {
            What::Group { class, name, members, .. } => Some((*class, name.clone(), members.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(groups.len(), 1, "{groups:?}");
    let (class, name, members) = &groups[0];
    assert_eq!((*class as usize, name.as_deref()), (QUIET, Some("rustc")));
    assert!(members.iter().all(|&m| f.state[m as usize] == State::Ok), "only clean exits are counted together");
    // As many of the 42 as fit stay expanded: 10 rows less the failure, the
    // two running and the group itself.
    assert_eq!(members.len(), 42 - 6);
    // The label a person reads.
    let theme = Theme::void_light();
    let a = Activity::default();
    let sc = build(&frame(&ledger, &a, &theme, [2360.0, 760.0], 30.0, T0 + 8 * SEC));
    let said = |needle: &str| sc.texts.iter().any(|t| t.s.contains(needle));
    assert!(sc.texts.iter().any(|t| t.s.trim() == "rustc") && said("\u{d7}"), "{:?}", sc.texts.iter().map(|t| &t.s).collect::<Vec<_>>());
    assert!(said("exit 1"), "the failure says its code");
    assert_eq!((sc.counts.procs, sc.counts.running, sc.counts.failed), (47, 4, 1));
}

#[test]
fn failures_that_cannot_all_be_drawn_are_counted_in_red_never_hidden_among_successes() {
    let mut l = Ledger::new();
    l.apply(&start(1, 0, T0, "cmd"));
    for i in 0..300u32 {
        let t = T0 + 1000 + i as u64 * 20_000;
        l.apply(&start(10 + i, 1, t, &format!("cmd /c exit {}", i % 3)));
        l.apply(&exit(10 + i, t + 15_000, i % 3));
    }
    let f = Forest::of(&l);
    for max_rows in [1usize, 2, 3, 5, 12, 40] {
        let (shown, _) = arrange(&f, max_rows, 4);
        assert_eq!(accounted(&f, &shown), 301, "max_rows {max_rows}");
        for s in &shown {
            if let What::Group { class, members, .. } = &s.what {
                let fails = members.iter().filter(|&&m| f.fails[m as usize] > 0).count();
                match *class as usize {
                    QUIET => assert_eq!(fails, 0, "a failure inside a group of successes"),
                    FAIL => assert_eq!(fails, members.len()),
                    _ => panic!("nothing is running"),
                }
            }
        }
        // One row cannot hold a count of successes, a count of failures and
        // the latest failure itself: the least is three.
        assert!(rows_of(&shown) <= max_rows.max(3), "{} rows for a budget of {max_rows}", rows_of(&shown));
        // The most recent failure is always its own node (pid 309 exited with 2).
        assert!(shown.iter().any(|s| matches!(s.what, What::Proc { index, .. } if f.procs[index].pid == 309)), "max_rows {max_rows}");
    }
    // With room to spare nothing is folded at all.
    let (all, level) = arrange(&f, 400, 4);
    assert_eq!((all.len(), level), (301, 0));
}

#[test]
fn the_same_input_gives_the_same_picture() {
    let theme = Theme::void_light();
    let mut a = Activity::default();
    for i in 0..200u64 {
        a.add(T0 + i * 37_000, 100 + i % 7 * 400);
    }
    let (l1, l2) = (build_ledger(), build_ledger());
    for (size, px) in [([2360.0, 1520.0], 30.0), ([1200.0, 700.0], 15.0), ([1200.0, 700.0], 30.0), ([640.0, 400.0], 15.0)] {
        let one = build(&frame(&l1, &a, &theme, size, px, T0 + 8 * SEC));
        let two = build(&frame(&l2, &a, &theme, size, px, T0 + 8 * SEC));
        assert!(one == two, "two builds from equal input differ at {size:?}");
        assert!(!one.quads.is_empty() && !one.texts.is_empty());
        // Everything drawn is inside the window, and text sits on whole pixels.
        for q in one.quads.iter().chain(&one.over) {
            assert!(q.x >= -0.5 && q.y >= -0.5 && q.x + q.w <= size[0] + 0.5 && q.y + q.h <= size[1] + 0.5, "{q:?} outside {size:?}");
        }
        for t in &one.texts {
            assert_eq!((t.x.fract(), t.y.fract()), (0.0, 0.0), "{t:?}");
            assert!(t.x >= 0.0 && t.x + t.s.chars().count() as f32 * ty(px).w(t.size) <= size[0] + 0.5, "{:?} runs off a {} px window", t.s, size[0]);
        }
        // Node boxes never overlap each other.
        let nodes: Vec<&Hit> = one.hits.iter().filter(|h| !matches!(h.target, Target::Span(_))).collect();
        assert!(nodes.len() >= 4, "{} nodes at {size:?}", nodes.len());
        for (i, a) in nodes.iter().enumerate() {
            for b in &nodes[i + 1..] {
                let apart = a.rect[0] + a.rect[2] <= b.rect[0] || b.rect[0] + b.rect[2] <= a.rect[0] || a.rect[1] + a.rect[3] <= b.rect[1] || b.rect[1] + b.rect[3] <= a.rect[1];
                assert!(apart, "{a:?} overlaps {b:?} at {size:?}");
            }
        }
    }
}

#[test]
fn time_ends_at_the_latest_event_and_nothing_reads_a_clock() {
    let theme = Theme::void_light();
    let ledger = build_ledger();
    let a = Activity::default();
    let t1 = T0 + 8 * SEC;
    let fr = frame(&ledger, &a, &theme, [2360.0, 1520.0], 30.0, t1);
    let sc = build(&fr);
    let axis = sc.axis.unwrap();
    assert_eq!((axis.lo, axis.hi), (T0, t1), "the axis is the session: first event to latest event");
    // The shell is still running: its span ends at the right edge, which is
    // the latest event.
    let right = axis.x0 + axis.w;
    let shell = sc.hits.iter().find(|h| h.target == Target::Span(0)).unwrap();
    assert!((shell.rect[0] + shell.rect[2] - right).abs() <= 1.0, "{shell:?} should end at {right}");
    assert!(sc.texts.iter().any(|t| t.s == "8.00s"), "elapsed is the time of the latest event");
    // The picture is a function of its input alone: built later, it is the
    // same picture.
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(build(&fr) == sc);
    // A later event moves the edge; nothing else does.
    let later = build(&frame(&ledger, &a, &theme, [2360.0, 1520.0], 30.0, t1 + 2 * SEC));
    assert_eq!(later.axis.unwrap().hi, t1 + 2 * SEC);
}

#[test]
fn the_axis_maps_time_to_pixels_and_back_and_ticks_are_round() {
    let axis = Axis { lo: T0, hi: T0 + 10 * SEC, x0: 24.0, w: 2000.0 };
    assert_eq!((axis.x(T0), axis.x(T0 + 5 * SEC), axis.x(T0 + 10 * SEC)), (24.0, 1024.0, 2024.0));
    assert_eq!(axis.x(T0 + 99 * SEC), 2024.0, "later than the latest event is the right edge");
    for t in [T0, T0 + 1, T0 + 3_333_333, T0 + 10 * SEC] {
        // One pixel is 5 ms here.
        assert!(axis.t(axis.x(t)).abs_diff(t) <= 2_501, "pixel round trip of {t}");
    }
    assert_eq!(tick_step(10 * SEC, 12), SEC);
    assert_eq!(tick_step(10 * SEC, 5), 2 * SEC);
    assert_eq!(tick_step(700_000, 16), 50_000);
    assert_eq!(tick_step(4 * 3600 * SEC, 10), 1800 * SEC);
    assert_eq!(tick_step(u64::MAX / 2, 4), 30 * 86_400 * SEC);
    let labels: Vec<String> = [(0, SEC), (45 * SEC, SEC), (90 * SEC, 30 * SEC), (3723 * SEC, 60 * SEC), (1_250_000, 50_000), (250_000, 100_000), (61_500_000, 500_000), (12_345, 1000)]
        .into_iter()
        .map(|(t, s)| tick_label(t, s))
        .collect();
    assert_eq!(labels, ["0s", "45s", "1:30", "1:02:03", "1.25s", "0.2s", "1:01.5", "0.012s"]);
}

#[test]
fn zooming_keeps_the_moment_under_the_mouse_where_it_is() {
    let full = (T0, T0 + 100 * SEC);
    let anchor = T0 + 30 * SEC;
    let z = zoomed(full, None, anchor, 0.5).unwrap();
    assert_eq!(z.1 - z.0, 50 * SEC);
    // The anchor was 30% of the way across, and still is.
    assert!(((anchor - z.0) as f64 / (z.1 - z.0) as f64 - 0.3).abs() < 1e-6, "{z:?}");
    let z2 = zoomed(full, Some(z), anchor, 0.5).unwrap();
    assert!(((anchor - z2.0) as f64 / (z2.1 - z2.0) as f64 - 0.3).abs() < 1e-6);
    // Out past the whole session is the whole session.
    assert_eq!(zoomed(full, Some(z), anchor, 4.0), None);
    // In stops at the shortest span, and never leaves the session.
    let mut cur = None;
    for _ in 0..80 {
        cur = zoomed(full, cur, full.1, 0.5);
    }
    let (lo, hi) = cur.unwrap();
    assert_eq!(hi - lo, MIN_SPAN_US);
    assert_eq!(hi, full.1, "anchored at the latest moment, it stays there");
    // Sliding stops at both ends.
    assert_eq!(panned(full, Some(z), -9.0), Some((full.0, full.0 + 50 * SEC)));
    assert_eq!(panned(full, Some(z), 9.0), Some((full.1 - 50 * SEC, full.1)));
    assert_eq!(panned(full, None, 0.2), None, "the whole session has nowhere to slide to");
}

#[test]
fn output_is_counted_into_exact_slices_and_no_byte_is_lost() {
    let mut a = Activity::default();
    let mut sum = 0u64;
    for i in 0..5000u64 {
        let b = 1 + i % 97;
        a.add(T0 + i * 7_013, b);
        sum += b;
    }
    a.add(T0 + 5, 11); // history arriving late, out of order
    sum += 11;
    assert_eq!(a.total, sum);
    let hi = T0 + 5000 * 7_013;
    for max_n in [1usize, 7, 100, 578, 100_000] {
        let (slice, first, b) = a.slices(T0, hi, max_n);
        assert_eq!(b.iter().sum::<u64>(), sum, "max_n {max_n}");
        assert!(b.len() <= max_n.max(1) + 1 && slice % BIN_US == 0 && first <= T0 && first + b.len() as u64 * slice > hi, "max_n {max_n}: {} slices of {slice}", b.len());
    }
    // A window in the middle holds exactly what was printed in it (to the slice).
    let (slice, first, b) = a.slices(T0 + 10 * SEC, T0 + 20 * SEC, 50);
    assert!(first <= T0 + 10 * SEC && first + b.len() as u64 * slice > T0 + 20 * SEC);
    let inside = |t: u64| (first..=T0 + 20 * SEC).contains(&(t / BIN_US * BIN_US));
    let want: u64 = (0..5000u64).filter(|i| inside(T0 + i * 7_013)).map(|i| 1 + i % 97).sum();
    assert_eq!(b.iter().sum::<u64>(), want);
    // A session too long to keep at full resolution is kept coarser, with
    // every byte.
    let mut long = Activity::default();
    for i in 0..(MAX_BINS as u64 + 10) {
        long.add(i * BIN_US, 3);
    }
    assert_eq!(long.total, (MAX_BINS as u64 + 10) * 3);
    assert_eq!(long.bins.iter().map(|b| b.1).sum::<u64>(), long.total);
    assert!(long.bin_us == 2 * BIN_US && long.bins.len() <= MAX_BINS / 2 + 16);
}

#[test]
fn names_and_arguments_are_read_from_what_the_os_reported() {
    let mut l = Ledger::new();
    let s = |pid: u32, image: Option<&str>, cmd: Option<&str>| ProcEvent::Start(Start { pid, ppid: None, image: image.map(String::from), cmd: cmd.map(String::from), start_us: Some(T0), seen_us: T0 });
    l.apply(&s(1, Some("C:\\Program Files\\Git\\cmd\\git.EXE"), Some("\"C:\\Program Files\\Git\\cmd\\git.exe\"  status\t--short\r\n")));
    l.apply(&s(2, None, Some("tools\\build.cmd  all")));
    l.apply(&s(3, None, None));
    let p = l.procs();
    assert_eq!((name_of(&p[0]), args_of(&p[0]).as_str()), (Some("git"), "status --short"));
    assert_eq!((name_of(&p[1]), args_of(&p[1]).as_str()), (Some("build.cmd"), "all"));
    assert_eq!((name_of(&p[2]), args_of(&p[2]).as_str()), (None, ""));
    assert_eq!(Forest::of(&l).label(2), "pid 3");
    // Long paths give way to their ends; short ones, options and addresses
    // are left alone.
    assert_eq!(shorten("build --manifest-path=C:\\Users\\someone\\src\\tiny\\Cargo.toml -q"), "build --manifest-path=\u{2026}\\tiny\\Cargo.toml -q");
    assert_eq!(shorten("\"C:\\Users\\someone\\AppData\\Local\\Temp\\x\\out.txt\""), "\"\u{2026}\\x\\out.txt\"");
    assert_eq!(shorten("/NOLOGO src\\main.rs https://example.com/a/b/c/d/e/f/g/h"), "/NOLOGO src\\main.rs https://example.com/a/b/c/d/e/f/g/h");
    assert_eq!((fit("abcdef", 6), fit("abcdef", 4), fit("abcdef", 0)), ("abcdef".to_string(), "abc\u{2026}".to_string(), String::new()));
}

/// The claim "zero frames when nothing changes", as far as it can be checked
/// without a window: nothing asks for a picture unless something in the
/// picture changed. (`renderer/scene.rs` checks the other half: that a built
/// picture stops asking for frames once its arrivals have settled.)
#[test]
fn nothing_asks_for_a_frame_unless_the_picture_changed() {
    let theme = Theme::void_light();
    let mut ledger = Ledger::new();
    let mut live = Live::new("test");
    let (t, size, pad) = (ty(30.0), [2360.0, 1520.0], 24.0);
    let events = [start(100, 1, T0, "pwsh"), start(200, 100, T0 + SEC, "git status"), exit(200, T0 + 2 * SEC, 128), start(300, 100, T0 + 3 * SEC, "cargo test")];
    for ev in &events {
        let id = ledger.apply(ev);
        live.on_proc(&ledger, id, ev, true, 5.0);
    }
    live.on_output(4096, T0 + 3 * SEC + 500);
    assert!(live.dirty);
    let sc = live.scene(&ledger, t, size, pad, &theme, 5.0);
    assert!(!live.dirty, "a built picture is current");
    // What has just arrived is still settling: frames are owed until it has.
    assert_eq!(sc.last_birth, Some(5.0));
    assert!(sc.emits.iter().any(|e| e.color == theme.accent) && sc.emits.iter().any(|e| e.color == theme.ansi[1]), "a start casts the accent, a failure casts red");
    // Nothing new: the same bytes, the mouse still, the wheel still.
    live.on_output(4096, T0 + 9 * SEC);
    assert!(!live.wheel(0.0, 500.0) && !live.dirty, "no event, no picture");
    // The mouse moving over nothing, then within one node, changes nothing.
    let node = sc.hits.iter().find(|h| h.target == Target::Node(1)).unwrap().rect;
    assert!(!live.mouse(5.0, 5.0) && !live.dirty);
    assert!(live.mouse(node[0] + 4.0, node[1] + 4.0), "onto a node: one picture");
    let hovered = live.scene(&ledger, t, size, pad, &theme, 5.1);
    assert!(hovered.clip.is_some() && hovered.texts.iter().any(|t| t.top && t.s.contains("exit 128")), "the card says the exit code");
    assert!(hovered.texts.iter().any(|t| t.top && t.s == "git status"), "and the whole command line");
    assert!(!live.mouse(node[0] + 9.0, node[1] + 6.0) && !live.dirty, "moving within the node asks for nothing");
    assert!(live.mouse(5.0, 5.0), "leaving it: one picture");
    let _ = live.scene(&ledger, t, size, pad, &theme, 5.2);
    // The wheel zooms: one picture per notch that changes the range.
    assert!(live.wheel(1.0, 1000.0) && live.dirty);
    let zoomed = live.scene(&ledger, t, size, pad, &theme, 5.3);
    let (a0, a1) = (sc.axis.unwrap(), zoomed.axis.unwrap());
    assert!(a1.hi - a1.lo < a0.hi - a0.lo && a1.t(1000.0).abs_diff(a0.t(1000.0)) < 20_000, "zoomed about the mouse");
    assert!(live.key(Key::Fit) && live.scene(&ledger, t, size, pad, &theme, 5.4).axis == sc.axis);
    assert!(!live.key(Key::Fit) && !live.key(Key::Left) && !live.dirty, "already whole: nothing to do");
    // Long after the arrivals, with no new event: a picture built then has
    // nothing arriving in it, so no frame would follow it.
    live.dirty = true;
    let late = live.scene(&ledger, t, size, pad, &theme, 5.0 + ARRIVAL_S + 0.1);
    assert!(late.last_birth.is_none() && late.emits.is_empty() && late.texts.iter().all(|t| t.born.is_none()));
    // And it is the settled form of the first picture: same marks, same places.
    assert!(late.quads == sc.quads && late.texts.len() == sc.texts.len());
    // Keys close the view.
    live.toggle();
    assert!(live.shown && live.key(Key::Close) && !live.shown);
}

#[test]
fn history_replayed_to_a_window_is_placed_by_the_journals_clock_not_the_moment_of_attaching() {
    use crate::journal::{JournalWriter, K_OUTPUT, K_PROC, K_RESIZE};
    let path = std::env::temp_dir().join(format!("proteus-world-history-{}.prtj", std::process::id()));
    let now = crate::procs_win::now_us();
    {
        let mut j = JournalWriter::create(&path, &serde_json::json!({"v": 1, "session": "h"})).unwrap();
        j.record(K_RESIZE, &[80, 0, 24, 0]).unwrap();
        j.record(K_PROC, &start(10, 1, now, "cmd.exe").to_bytes()).unwrap();
        j.record(K_OUTPUT, &vec![b'a'; 1000]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(60));
        j.record(K_OUTPUT, &vec![b'b'; 500]).unwrap();
        j.flush().unwrap();
    }
    // The window was replayed 1200 of those bytes (the last 300 had not been
    // sent yet), then 800 more arrived live.
    let attach = now + 10 * SEC;
    let mut live = Live::new("h");
    live.on_replayed(1200, attach);
    assert_eq!((live.unplaced, live.activity.total), (1200, 0), "history is not drawn as a burst at the moment of attaching");
    live.on_output(2000, attach + SEC);
    assert_eq!(live.activity.total, 800);
    live.load_history(Some(&path));
    assert_eq!((live.unplaced, live.activity.total), (0, 2000), "every byte once");
    let (_, _, early) = live.activity.slices(now.saturating_sub(SEC), now + 2 * SEC, 1);
    assert_eq!(early.iter().sum::<u64>(), 1200, "placed when it was printed");
    // No journal this window can read: it stays off the timeline, and the
    // view says so.
    let mut blind = Live::new("h");
    blind.on_replayed(1200, attach);
    blind.load_history(None);
    blind.load_history(Some(std::path::Path::new("Z:\\no\\such.prtj")));
    assert_eq!((blind.unplaced, blind.activity.total), (1200, 0));
    let mut ledger = Ledger::new();
    let ev = start(10, 1, now, "cmd.exe");
    let id = ledger.apply(&ev);
    blind.on_proc(&ledger, id, &ev, false, 1.0);
    let theme = Theme::void_light();
    let sc = blind.scene(&ledger, ty(30.0), [2360.0, 1520.0], 24.0, &theme, 1.0);
    assert!(sc.texts.iter().any(|t| t.s.contains("before this window attached")), "the gap is stated");
    assert!(sc.last_birth.is_none() && sc.emits.is_empty(), "history is placed without being lit");
    let _ = std::fs::remove_file(&path);
}

/// A window that never opens the view still hears of every process. What it
/// keeps for the view must stay small however long the session runs.
#[test]
fn arrivals_are_forgotten_even_if_the_view_is_never_opened() {
    let mut ledger = Ledger::new();
    let mut live = Live::new("long");
    let shell = start(1, 0, T0, "pwsh");
    let id = ledger.apply(&shell);
    live.on_proc(&ledger, id, &shell, true, 0.0);
    for i in 0..20_000u32 {
        let t = T0 + 1000 + i as u64 * 5_000;
        let now = i as f64 * 0.005; // two hundred a second, for a hundred seconds
        for ev in [start(10 + i, 1, t, "cmd /c exit 1"), exit(10 + i, t + 4_000, 1)] {
            let id = ledger.apply(&ev);
            live.on_proc(&ledger, id, &ev, true, now);
        }
    }
    let most = (ARRIVAL_S / 0.005) as usize + 2;
    assert!(live.born.len() <= most && live.failed.len() <= most, "{} and {} remembered", live.born.len(), live.failed.len());
    assert_eq!(live.full().1 - live.full().0, 1000 + 19_999 * 5_000 + 4_000 + 25, "and the latest event is still the latest event");
}

/// The two recordings in forge/samples are real sessions (forge/lanes/world.json
/// has the commands that made them).
fn sample(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("forge").join("samples").join(name)
}

#[test]
fn a_recorded_loop_of_thirty_failing_commands_reads_back_whole() {
    let scan = scan_journal(&sample("world-loop30.prtj"), None).unwrap();
    let procs = scan.ledger.procs();
    assert_eq!((procs.len(), scan.ledger.live_count(), scan.ended), (31, 0, Some(Some(30))));
    for (i, p) in procs.iter().enumerate().skip(1) {
        assert_eq!((p.exit_code(), p.parent), (Some(i as u32), Some(procs[0].id)), "{:?}", p.cmd);
    }
    assert!(scan.epoch_exact && scan.activity.total == 359);
    assert_eq!((scan.born.len(), scan.failed.len()), (31, 31), "every start, and every failing exit, has the time it was recorded");
    // Time travel: half way through, some had started and the rest had not.
    let half = scan_journal(&sample("world-loop30.prtj"), Some(330_000)).unwrap();
    let n = half.ledger.procs().len();
    assert!(n < 31 && n > 8 && half.ended.is_none(), "{n} processes at 0.33 s");
    assert!(half.ledger.live_count() >= 1, "the shell was still running");
    assert!(half.last_us - half.epoch_us <= 330_000);

    // Drawn at the sizes of the brief: all 31 accounted for, 31 spans,
    // nothing cut.
    let theme = Theme::void_light();
    for (size, px) in [([2360.0, 1520.0], 30.0), ([1200.0, 700.0], 15.0), ([1200.0, 700.0], 30.0)] {
        let mut live = Live::from_scan(&scan, "loop30");
        let sc = live.scene(&scan.ledger, ty(px), size, 12.0 * px / 15.0, &theme, 60.0);
        let c = sc.counts;
        assert_eq!((c.procs, c.failed, c.running, c.spans), (31, 31, 0, 31), "{size:?}");
        assert_eq!(c.nodes + c.folded, 31, "{c:?}");
        assert!(c.rows_cut == 0 && c.groups >= 1, "{c:?} at {size:?}");
        assert!(sc.texts.iter().any(|t| t.s.contains("exit 30")), "the latest failure is on screen at {size:?}");
        assert!(sc.last_birth.is_none(), "a recording shown settled owes no frames");
    }
}

#[test]
fn a_recorded_build_shows_its_tree_and_its_failures() {
    let scan = scan_journal(&sample("world-build.prtj"), None).unwrap();
    let f = Forest::of(&scan.ledger);
    assert_eq!((f.n(), scan.ledger.live_count(), scan.ended), (25, 0, Some(Some(0))));
    assert_eq!(f.state.iter().filter(|s| **s == State::Failed).count(), 4, "findstr, and the compiler, cargo and its launcher of the broken crate");
    assert_eq!(f.depth.iter().max(), Some(&4), "shell, launcher, cargo, rustc, linker");
    let failing_rustc = (0..f.n()).find(|&i| f.label(i) == "rustc" && f.state[i] == State::Failed).unwrap();
    let chain: Vec<String> = std::iter::successors(Some(failing_rustc), |&i| (f.parent[i] != NONE).then_some(f.parent[i] as usize)).map(|i| f.label(i)).collect();
    assert_eq!(chain.len(), 4, "{chain:?}");
    assert!(chain[1] == "cargo" && chain[3] == "cmd", "{chain:?}");
    let theme = Theme::void_light();
    let mut live = Live::from_scan(&scan, "build");
    let sc = live.scene(&scan.ledger, ty(30.0), [2360.0, 1520.0], 24.0, &theme, 60.0);
    assert_eq!((sc.counts.nodes, sc.counts.groups, sc.counts.level, sc.counts.spans), (25, 0, 0, 25), "small enough to draw whole");
    assert!(sc.texts.iter().any(|t| t.s.contains("exit 101")));
    // The mouse on the failing compiler: the card holds its whole command
    // line (792 characters), its exit code and how long it lived.
    let id = scan.ledger.procs()[failing_rustc].id;
    let node = sc.hits.iter().find(|h| h.target == Target::Node(id)).unwrap().rect;
    assert!(live.mouse(node[0] + 5.0, node[1] + 5.0));
    let hovered = live.scene(&scan.ledger, ty(30.0), [2360.0, 1520.0], 24.0, &theme, 60.0);
    let card: Vec<&str> = hovered.texts.iter().filter(|t| t.top).map(|t| t.s.as_str()).collect();
    let whole = one_line(scan.ledger.procs()[failing_rustc].cmd.as_deref().unwrap());
    assert!(whole.chars().count() > 700, "{}", whole.len());
    let said: String = card.join(" ");
    for word in whole.split(' ') {
        assert!(word.chars().count() > 100 || said.contains(word), "the card leaves out {word:?}");
    }
    assert!(!said.contains("characters in all"), "nothing was cut: {card:?}");
    assert!(said.contains("exit 1, lived") && said.contains("pid "), "{card:?}");
    let k = hovered.clip.unwrap();
    assert!(k[0] >= 0.0 && k[1] >= 0.0 && k[0] + k[2] <= 2360.0 && k[1] + k[3] <= 1520.0, "the card is inside the window: {k:?}");
    assert!(live.mouse(1.0, 1.0));

    // In a small window the same session folds, and still accounts for
    // everything.
    let small = live.scene(&scan.ledger, ty(30.0), [1200.0, 700.0], 24.0, &theme, 60.0);
    assert!(small.counts.groups >= 1 && small.counts.rows_cut == 0, "{:?}", small.counts);
    assert_eq!(small.counts.nodes + small.counts.folded, 25);
    assert!(small.texts.iter().any(|t| t.s.contains("exit 101")) && small.texts.iter().any(|t| t.s.contains("exit 1")));
    // The output record: all of it, on the session's own clock.
    assert_eq!(scan.activity.total, 2950);
    let (_, _, b) = scan.activity.slices(scan.epoch_us, scan.last_us, 400);
    assert_eq!(b.iter().sum::<u64>(), 2950);
}

/// A ledger of `n` processes shaped like an agent's working session, made
/// from a fixed sequence (so it is the same every run): a shell that runs
/// commands one after another, some of which start helpers of their own,
/// about one in forty failing, the last few still running. GENERATED, not
/// recorded: it exists to time the scene builder at exact sizes.
fn generated(n: u32) -> (Ledger, u64) {
    const KINDS: [&str; 8] = ["git", "cargo", "rustc", "node", "rg", "python", "cmd", "link"];
    let mut l = Ledger::new();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut t = T0;
    l.apply(&start(1, 0, t, "pwsh -NoLogo"));
    let mut open: Vec<(u32, u64)> = Vec::new(); // (pid, when it ends)
    let mut parents: Vec<u32> = vec![1];
    for i in 1..n {
        t += 2_000 + next() % 40_000;
        let pid = 1 + i;
        // Three in four are started by the shell; the rest by a recent process.
        let parent = if next() % 4 != 0 { 1 } else { parents[parents.len() - 1 - (next() % parents.len().min(6) as u64) as usize] };
        let kind = KINDS[(next() % 8) as usize];
        l.apply(&start(pid, parent, t, &format!("{kind} --job {i} C:/work/project/src/module{}/file{}.rs", i % 17, i % 101)));
        parents.push(pid);
        open.push((pid, t + 5_000 + next() % 400_000));
        let (done, still): (Vec<_>, Vec<_>) = open.iter().partition(|(_, end)| *end <= t);
        for (p, end) in done {
            l.apply(&exit(p, end, if next() % 40 == 0 { 1 + (next() % 3) as u32 } else { 0 }));
        }
        open = still;
    }
    // All but the last few end.
    let keep = open.len().saturating_sub(3);
    for (p, end) in open.drain(..keep) {
        l.apply(&exit(p, end.min(t), 0));
    }
    (l, t)
}

/// Scene build time at three sizes of session. Run by hand:
///   cargo test --release -- --ignored --nocapture measure_scene_build
#[test]
#[ignore]
fn measure_scene_build() {
    let theme = Theme::void_light();
    let mut out = Vec::new();
    for n in [50u32, 500, 5000] {
        let (ledger, t1) = generated(n);
        let mut a = Activity::default();
        for i in 0..(n as u64 * 4) {
            a.add(T0 + i * (t1 - T0) / (n as u64 * 4), 200 + i % 900);
        }
        let mut row = serde_json::json!({ "processes": ledger.procs().len(), "running": ledger.live_count() });
        for (name, size, px) in [("2360x1520", [2360.0, 1520.0], 30.0), ("1200x700", [1200.0, 700.0], 15.0)] {
            let fr = frame(&ledger, &a, &theme, size, px, t1);
            let mut us: Vec<u64> = (0..300)
                .map(|_| {
                    let t = std::time::Instant::now();
                    let sc = build(&fr);
                    let d = t.elapsed().as_micros() as u64;
                    std::hint::black_box(sc);
                    d
                })
                .collect();
            us.sort_unstable();
            let sc = build(&fr);
            row[name] = serde_json::json!({ "p50_us": us[150], "p95_us": us[285], "max_us": us[299], "rectangles": sc.quads.len() + sc.over.len(), "text_runs": sc.texts.len(), "nodes": sc.counts.nodes, "groups": sc.counts.groups, "rows": sc.counts.rows, "rows_cut": sc.counts.rows_cut, "spans_drawn": sc.counts.spans });
        }
        out.push(row);
    }
    println!("MEASURE scene_build (generated ledgers, 300 builds each) {}", serde_json::Value::Array(out));
}

/// Every mark the mouse can be over, hovered, at every size: the card is
/// built, stays inside the window, and nothing panics. (A hover on one span
/// of the recorded build once did: a full row followed by a word longer than
/// a row. A panic in a window is the end of that window.)
#[test]
fn everything_can_be_hovered_at_every_size() {
    let theme = Theme::void_light();
    let (big, big_t1) = generated(600);
    let built = scan_journal(&sample("world-build.prtj"), None).unwrap();
    let looped = scan_journal(&sample("world-loop30.prtj"), None).unwrap();
    let a = Activity::default();
    let sets: [(&Ledger, &Activity, u64, u64); 3] = [
        (&built.ledger, &built.activity, built.epoch_us, built.last_us),
        (&looped.ledger, &looped.activity, looped.epoch_us, looped.last_us),
        (&big, &a, T0, big_t1),
    ];
    let mut cards = 0;
    for (ledger, activity, t0, t1) in sets {
        for (size, px) in [([2360.0, 1520.0], 30.0), ([1200.0, 700.0], 15.0), ([1200.0, 700.0], 30.0), ([700.0, 420.0], 15.0), ([3840.0, 2160.0], 45.0)] {
            let mut fr = frame(ledger, activity, &theme, size, px, t1);
            fr.t0_us = t0;
            let plain = build(&fr);
            let mut targets: Vec<Target> = plain.hits.iter().map(|h| h.target).collect();
            targets.dedup();
            for target in targets {
                fr.hover = Some(target);
                let sc = build(&fr);
                let k = sc.clip.unwrap_or_else(|| panic!("no card for {target:?} at {size:?}"));
                assert!(k[0] >= 0.0 && k[1] >= 0.0 && k[0] + k[2] <= size[0] + 0.5 && k[1] + k[3] <= size[1] + 0.5, "card {k:?} for {target:?} leaves a {size:?} window");
                assert!(sc.texts.iter().filter(|t| t.top).count() >= 3, "{target:?}");
                for q in &sc.over {
                    assert!(q.x + q.w <= k[0] || q.x >= k[0] + k[2] || q.y + q.h <= k[1] || q.y >= k[1] + k[3], "an outline crosses the card");
                }
                cards += 1;
            }
        }
    }
    assert!(cards > 1500, "{cards} cards built");
}

/// What a sensor on a real machine can hand over, and what a person can do
/// to a window: none of it may panic.
#[test]
fn odd_input_and_odd_windows_never_panic() {
    let theme = Theme::void_light();
    let mut l = Ledger::new();
    let s = |pid: u32, ppid: Option<u32>, image: Option<&str>, cmd: Option<String>, t: Option<u64>| ProcEvent::Start(Start { pid, ppid, image: image.map(String::from), cmd, start_us: t, seen_us: t.unwrap_or(T0) + 7 });
    l.apply(&s(1, None, Some("C:\\Windows\\System32\\cmd.exe"), Some("cmd".into()), Some(T0)));
    l.apply(&s(2, Some(1), Some(""), Some(String::new()), Some(T0 + 10)));
    l.apply(&s(3, Some(1), Some(".exe"), Some("\"unterminated quote and then some".into()), Some(T0 + 20)));
    l.apply(&s(4, Some(1), None, Some("x".repeat(20_000)), Some(T0 + 30)));
    l.apply(&s(5, Some(4), Some("C:\\\u{65e5}\u{672c}\u{8a9e}\\\u{30c4}\u{30fc}\u{30eb}.exe"), Some("\u{30c4}\u{30fc}\u{30eb} \u{2014}\u{2014}\u{540d}\u{524d}=\u{1f680} C:\\\u{65e5}\u{672c}\u{8a9e}\\\u{6df1}\u{3044}\\\u{968e}\u{5c64}\\\u{306e}\\\u{30d5}\u{30a1}\u{30a4}\u{30eb}\\\u{540d}.txt".into()), Some(T0 + 40)));
    l.apply(&s(6, Some(999), None, None, None));
    l.apply(&s(7, Some(5), Some("a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/u/v/w/x/y/z.exe"), Some(format!("{} {}", "word ".repeat(400), "y".repeat(131))), Some(T0 + 50)));
    for (pid, code) in [(2u32, Some(0u32)), (3, Some(0xC000_0005)), (6, None)] {
        l.apply(&ProcEvent::Exit(Exit { pid, code, end_us: None, user_us: None, kernel_us: None, abnormal: code.is_some_and(|c| c > 0x8000_0000), seen_us: T0 + 60 }));
    }
    // A chain deeper than any window is wide.
    for i in 0..40u32 {
        l.apply(&s(100 + i, Some(if i == 0 { 1 } else { 99 + i }), Some("C:\\bin\\deep.exe"), Some(format!("deep --level {i}")), Some(T0 + 100 + i as u64)));
    }
    let mut a = Activity::default();
    a.add(T0, 1);
    a.add(T0 + 3 * SEC, u32::MAX as u64);
    let mut built = 0;
    for size in [[0.0f32, 0.0], [1.0, 1.0], [40.0, 4000.0], [4000.0, 40.0], [200.0, 120.0], [320.0, 200.0], [1200.0, 700.0], [5000.0, 3000.0]] {
        for px in [6.0f32, 15.0, 30.0, 72.0] {
            for zoom in [None, Some((T0, T0 + 1)), Some((T0 + SEC, T0 + 2 * SEC)), Some((0, u64::MAX)), Some((T0 + 9 * SEC, T0 + 9 * SEC))] {
                for t1 in [T0, T0 + 1, T0 + 3 * SEC] {
                    let mut fr = frame(&l, &a, &theme, size, px, t1);
                    fr.zoom = zoom;
                    let sc = build(&fr);
                    for target in sc.hits.iter().map(|h| h.target).chain([Target::Node(u64::MAX), Target::Group(3, 2), Target::Span(1)]) {
                        fr.hover = Some(target);
                        built += build(&fr).texts.len().min(1);
                    }
                }
            }
        }
    }
    assert!(built > 1000, "{built}");
    // The view's own state, driven hard.
    let mut live = Live::new("odd");
    for key in [Key::Left, Key::Right, Key::In, Key::Out, Key::Fit, Key::Latest] {
        live.key(key);
    }
    assert!(!live.wheel(5.0, 100.0), "no picture yet, nothing to zoom");
    let _ = live.scene(&l, ty(15.0), [1200.0, 700.0], 12.0, &theme, 0.0);
    for i in 0..200 {
        live.wheel(if i % 3 == 0 { -2.5 } else { 4.0 }, (i * 37 % 1300) as f32 - 50.0);
        live.key([Key::Left, Key::Right, Key::In, Key::Out, Key::Latest][i % 5]);
        live.mouse((i * 53 % 1300) as f32, (i * 29 % 800) as f32);
        live.on_output(i as u64 * 1000, T0 + i as u64 * 10_000);
        let sc = live.scene(&l, ty(15.0), [1200.0, 700.0], 12.0, &theme, i as f64);
        let ax = sc.axis.unwrap();
        assert!(ax.lo < ax.hi, "{ax:?}");
    }
}

/// Is this one of the red tags a node of the tree carries ("exit 7",
/// "1 failed")? Nothing else on screen is both in the failure colour and
/// worded like one: the header's count is a bare number, its label and the
/// legend are in muted ink.
fn says_failure(t: &Text, theme: &Theme) -> bool {
    t.color == theme.ansi[1] && !t.top && (t.s.contains("failed") || t.s.contains("exit ") || t.s.contains("0x") || t.s.contains("abnormal"))
}

/// forge/samples/world-chain.prtj is a real session five shells deep
/// (`cmd /c cmd /c cmd /c cmd /c cmd /c exit 7 ^^^^^^^& ver ^^^& ver ^& ver & ver`,
/// run in C:\Windows\System32): the innermost command fails with 7 and every
/// shell above it ends with 0. In a window with fewer columns than the tree
/// is deep, the failure is folded into the last node that is drawn. That
/// node must still say that something failed inside it: the header counting
/// "1 failed" over a tree of nothing but clean exits is not the truth.
#[test]
fn a_failure_folded_into_a_narrow_column_is_still_said_in_the_tree() {
    let scan = scan_journal(&sample("world-chain.prtj"), None).unwrap();
    let codes: Vec<Option<u32>> = scan.ledger.procs().iter().map(|p| p.exit_code()).collect();
    assert_eq!(codes, [Some(0), Some(0), Some(0), Some(0), Some(7)], "the recording: four clean shells and the failure inside them");
    let f = Forest::of(&scan.ledger);
    assert_eq!(f.depth, [0, 1, 2, 3, 4]);
    let theme = Theme::void_light();
    for (size, px) in [([2360.0, 1520.0], 30.0), ([1200.0, 700.0], 15.0), ([1200.0, 700.0], 30.0), ([900.0, 600.0], 30.0), ([700.0, 500.0], 30.0), ([520.0, 600.0], 30.0), ([1200.0, 700.0], 45.0)] {
        let mut live = Live::from_scan(&scan, "chain");
        let sc = live.scene(&scan.ledger, ty(px), size, 12.0 * px / 15.0, &theme, 60.0);
        let c = sc.counts;
        assert_eq!((c.procs, c.failed, c.nodes + c.folded, c.rows_cut), (5, 1, 5, 0), "{c:?} at {size:?}");
        let tags: Vec<&str> = sc.texts.iter().filter(|t| says_failure(t, &theme)).map(|t| t.s.trim()).collect();
        assert!(!tags.is_empty(), "at {size:?} with a {px} px font the tree draws {} nodes for 5 processes, 1 of them failed, and says nothing of a failure: {:?}", c.nodes, sc.texts.iter().map(|t| t.s.as_str()).collect::<Vec<_>>());
        if c.folded == 0 {
            assert_eq!(tags, ["exit 7"], "drawn whole, the failure is its own node");
        } else {
            assert!(tags.iter().any(|t| t.contains("1 failed")), "{tags:?} at {size:?}");
        }
    }
}

/// How a session ended is said in the header at every size the header is
/// drawn at. Where there is not room for the session's name as well, the
/// name gives way, not the ending; in the narrowest windows the exit code
/// alone is left.
#[test]
fn a_small_window_still_says_how_the_session_ended() {
    let theme = Theme::void_light();
    // The ink of the header's line about the session (`Palette::faint`);
    // no label in the tree that mentions an exit code is drawn in it.
    let faint = mix(theme.fg, theme.bg, 0.66);
    for (name, code) in [("world-loop30.prtj", 30), ("world-chain.prtj", 0), ("world-build.prtj", 0)] {
        let scan = scan_journal(&sample(name), None).unwrap();
        assert_eq!(scan.ended, Some(Some(code)));
        // The header calls a session by its file name: 15 or 16 characters in practice.
        let session = scan.meta["session"].as_str().unwrap().to_string();
        for (size, px) in [([2360.0, 1520.0], 30.0), ([1200.0, 700.0], 15.0), ([1200.0, 700.0], 30.0), ([1000.0, 600.0], 30.0), ([900.0, 600.0], 30.0)] {
            let mut live = Live::from_scan(&scan, &session);
            let sc = live.scene(&scan.ledger, ty(px), size, 12.0 * px / 15.0, &theme, 60.0);
            let said: Vec<&str> = sc.texts.iter().filter(|t| t.color == faint && t.s.contains(&format!("exit {code}"))).map(|t| t.s.as_str()).collect();
            assert_eq!(said.len(), 1, "{name} at {size:?} with a {px} px font does not say how the session ended: {:?}", sc.texts.iter().take(12).map(|t| t.s.as_str()).collect::<Vec<_>>());
            // With room to spare it is said in full, and with the name.
            if size[0] >= 1200.0 && px <= 15.0 {
                assert!(said[0].contains("ended, exit") && sc.texts.iter().any(|t| t.s.contains(&session)), "{said:?}");
            }
        }
    }
}

/// Read the pictures the way a person does: on the three recorded sessions,
/// at the sizes of the brief and some smaller, no piece of text runs into
/// another or off the window. (With the mouse on something, text under the
/// card is not drawn, and the card's own lines are checked the same way.)
#[test]
fn no_text_overlaps_or_leaves_the_window_on_the_recorded_sessions() {
    let theme = Theme::void_light();
    let mut checked = 0;
    for name in ["world-build.prtj", "world-loop30.prtj", "world-chain.prtj"] {
        let scan = scan_journal(&sample(name), None).unwrap();
        let session = scan.meta["session"].as_str().unwrap().to_string();
        for (size, px) in [([2360.0f32, 1520.0f32], 30.0f32), ([1200.0, 700.0], 15.0), ([1200.0, 700.0], 30.0), ([900.0, 600.0], 30.0), ([700.0, 500.0], 30.0), ([3840.0, 2160.0], 45.0)] {
            let t = ty(px);
            let mut live = Live::from_scan(&scan, &session);
            let plain = live.scene(&scan.ledger, t, size, 12.0 * px / 15.0, &theme, 60.0);
            let mut scenes = vec![plain.clone()];
            // And with the mouse on the first and the last thing it can be on.
            for h in [plain.hits.first(), plain.hits.last()].into_iter().flatten() {
                if live.mouse(h.rect[0] + 1.0, h.rect[1] + 1.0) {
                    scenes.push(live.scene(&scan.ledger, t, size, 12.0 * px / 15.0, &theme, 60.0));
                }
            }
            for sc in &scenes {
                let boxes: Vec<([f32; 4], &Text)> = sc
                    .texts
                    .iter()
                    .filter(|x| x.top || sc.clip.is_none_or(|k| {
                        // Drawn at all: not wholly under the card.
                        let (w, h) = (x.s.chars().count() as f32 * t.cell[x.size as usize][0], t.cell[x.size as usize][1]);
                        x.x + w <= k[0] || x.x >= k[0] + k[2] || x.y + h <= k[1] || x.y >= k[1] + k[3]
                    }))
                    .map(|x| {
                        // Where the ink of a line is: the cell is taller than
                        // the letters (the header sets its small labels close
                        // under its large numbers, inside the numbers' cells).
                        let h = t.cell[x.size as usize][1];
                        ([x.x, x.y + 0.10 * h, x.s.chars().count() as f32 * t.cell[x.size as usize][0], 0.74 * h], x)
                    })
                    .collect();
                for (i, (a, ta)) in boxes.iter().enumerate() {
                    assert!(a[0] >= 0.0 && a[1] >= 0.0 && a[0] + a[2] <= size[0] + 0.5 && a[1] + a[3] <= size[1] + 0.5, "{name} at {size:?}/{px}: {:?} leaves the window ({a:?})", ta.s);
                    for (b, tb) in &boxes[i + 1..] {
                        if ta.top != tb.top {
                            continue; // the card is drawn over what is beneath it
                        }
                        let apart = a[0] + a[2] <= b[0] + 0.5 || b[0] + b[2] <= a[0] + 0.5 || a[1] + a[3] <= b[1] + 0.5 || b[1] + b[3] <= a[1] + 0.5;
                        assert!(apart, "{name} at {size:?}/{px}: {:?} at {a:?} runs into {:?} at {b:?}", ta.s, tb.s);
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 10_000, "{checked} pairs of texts compared");
}

/// Opening the view for the first time in a window that picked a session up
/// again places that session's earlier output on the timeline, on the
/// window's own thread. How long that takes must grow with how much there
/// is, not with how much there is times how long this window has been
/// attached. The slices here are GENERATED (60,000 with output in the
/// journal, 60,000 more seen live: about an hour of each from a program that
/// prints steadily): a recording that long cannot be made inside a test, and
/// the property is one of arithmetic, not of the recording. Placing real
/// history from a real journal is checked for exactness by
/// `history_replayed_to_a_window_is_placed_by_the_journals_clock_not_the_moment_of_attaching`.
#[test]
fn placing_a_long_history_under_a_long_lived_window_is_one_pass() {
    const N: u64 = 60_000;
    let attach = T0 + 2 * N * 50_000;
    let mut recorded = Activity::default();
    for i in 0..N {
        recorded.add(T0 + i * 50_000, 100 + i % 7);
    }
    let mut live = Live::new("long");
    live.on_replayed(recorded.total, attach);
    for i in 0..N {
        live.on_output(recorded.total + (i + 1) * 10, attach + i * 50_000);
    }
    assert_eq!((live.unplaced, live.activity.total), (recorded.total, N * 10));
    let t = std::time::Instant::now();
    live.place_history(&recorded, T0);
    let took = t.elapsed();
    // Every byte once, each where it was printed, and the window's own untouched.
    assert_eq!((live.unplaced, live.activity.total), (0, recorded.total + N * 10));
    let (_, _, before) = live.activity.slices(T0, attach - 1, 1);
    let (_, _, after) = live.activity.slices(attach, attach + N * 50_000, 1);
    assert_eq!((before.iter().sum::<u64>(), after.iter().sum::<u64>()), (recorded.total, N * 10));
    assert_eq!(live.activity.bins.len() as u64, 2 * N);
    assert!(live.activity.bins.windows(2).all(|w| w[0].0 < w[1].0), "slices stay in order of time");
    assert_eq!(live.full().0, T0, "the session now begins where its history does");
    println!("MEASURE history_placement {N} recorded slices under {N} live ones: {} us", took.as_micros());
    assert!(took < std::time::Duration::from_millis(250), "placing {N} slices of history took {took:?}");
}

/// A key held a moment too long repeats. The press that showed or hid the
/// view must do so once, and its repeats must go nowhere: not back into the
/// view (which would flicker), and, after Esc has closed it, not into the
/// program underneath.
#[test]
fn a_held_key_shows_or_hides_the_view_once_and_is_not_typed_into_the_program() {
    let mut live = Live::new("keys");
    // Ctrl+Shift+W goes down and is held.
    assert!(!live.repeat_of_toggle(false), "the press itself is handled");
    assert!(live.toggle());
    for _ in 0..30 {
        assert!(live.repeat_of_toggle(true), "a repeat of the press that opened the view");
    }
    assert!(live.shown, "still shown after being held");
    // Esc goes down and is held: the view closes once, and the repeats that
    // follow are dropped rather than passed on.
    assert!(!live.repeat_of_toggle(false));
    assert!(live.key(Key::Close) && !live.shown);
    for _ in 0..30 {
        assert!(live.repeat_of_toggle(true), "a repeat of the Esc that closed the view would reach the program");
    }
    assert!(!live.shown);
    // Any other key afterwards is its own press, and repeats as keys do.
    assert!(!live.repeat_of_toggle(false) && !live.repeat_of_toggle(true) && !live.repeat_of_toggle(true));
    // Keys that move the timeline repeat while held: only showing and hiding is once.
    live.toggle();
    assert!(!live.repeat_of_toggle(false));
    live.key(Key::In);
    assert!(!live.repeat_of_toggle(true), "holding an arrow keeps zooming");
}
