//! Conformance: a sample that exercises the terminal the way full-screen
//! programs do, fed to the real parser and compared cell by cell with what
//! the specifications say the screen must hold. The expected text was
//! written by hand (`forge/samples/make.ps1`), never copied from a terminal.
//!
//! Where the terminal library differs from the expectation the difference is
//! listed in `KNOWN` with what it does instead, so the test documents the
//! gap, and fails the day the gap closes or a new one opens.
use std::sync::{Arc, Mutex};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};

use crate::term::{Notify, Terminal, UiEvent};
use crate::theme::Theme;

const SAMPLE: &[u8] = include_bytes!("../forge/samples/conformance.vt");
const EXPECT: &str = include_str!("../forge/samples/conformance.expect.txt");

/// Rows whose text the terminal library is known to get wrong: (row, what it
/// shows, why that is wrong). Empty: no row's text differs.
const KNOWN: &[(usize, &str, &str)] = &[];

/// Checks below that the terminal library is known to fail, and why.
const KNOWN_CHECKS: &[(&str, &str)] = &[(
    "SGR 21 double underline",
    "alacritty_terminal reads SGR 21 as 'bold off', the Linux console's reading; ECMA-48, xterm and kitty define it as double underline",
)];

fn cell(t: &Terminal, row: usize, col: usize) -> Cell {
    let term = t.term.lock().unwrap();
    term.grid()[Point::new(Line(row as i32 - 1), Column(col - 1))].clone()
}

/// The screen as text, one string per row: what a person would read.
fn screen(t: &Terminal) -> Vec<String> {
    let term = t.term.lock().unwrap();
    let grid = term.grid();
    (0..term.screen_lines())
        .map(|r| {
            let mut s = String::new();
            for c in 0..term.columns() {
                let cell = &grid[Point::new(Line(r as i32), Column(c))];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push(match cell.c {
                    '\t' | '\0' => ' ',
                    other => other,
                });
                s.extend(cell.zerowidth().into_iter().flatten());
            }
            s.trim_end().to_string()
        })
        .collect()
}

/// How many cells each character of a row occupies, for rows where width is the question.
fn widths(t: &Terminal, row: usize) -> Vec<(char, usize)> {
    let term = t.term.lock().unwrap();
    let grid = term.grid();
    let mut out: Vec<(char, usize)> = Vec::new();
    for c in 0..term.columns() {
        let cell = &grid[Point::new(Line(row as i32 - 1), Column(c))];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            if let Some(last) = out.last_mut() {
                last.1 += 1;
            }
        } else if cell.c != ' ' {
            out.push((cell.c, 1));
        }
    }
    out
}

fn terminal(titles: Arc<Mutex<Vec<String>>>) -> Terminal {
    let notify: Notify = Arc::new(move |e| {
        if let UiEvent::Title(t) = e {
            titles.lock().unwrap().push(t);
        }
    });
    Terminal::new(80, 24, Theme::void_light(), notify, None)
}

#[test]
fn the_sample_leaves_the_screen_the_specifications_describe() {
    let titles = Arc::new(Mutex::new(Vec::new()));
    let t = terminal(titles.clone());

    // Stop while the alternate screen is up to look at it, then go on.
    let leave = b"\x1b[?1049l";
    let at = SAMPLE.windows(leave.len()).position(|w| w == leave).expect("the sample leaves the alternate screen");
    t.feed(&SAMPLE[..at]);
    let mut wrong: Vec<String> = Vec::new();
    {
        let alt = screen(&t);
        let mode = *t.term.lock().unwrap().mode();
        if !mode.contains(TermMode::ALT_SCREEN) {
            wrong.push("mode 1049 did not switch to the alternate screen".into());
        }
        if alt[0] != "ALT SCREEN" || alt[1] != "should vanish" || alt[2..].iter().any(|r| !r.is_empty()) {
            wrong.push(format!("alternate screen holds {:?}", &alt[..3]));
        }
    }
    t.feed(&SAMPLE[at..]);

    // ---- text ----
    let got = screen(&t);
    let want: Vec<&str> = EXPECT.lines().collect();
    assert_eq!(want.len(), 24, "the expectation describes 24 rows");
    let mut known_seen = 0;
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        let row = i + 1;
        match KNOWN.iter().find(|k| k.0 == row) {
            Some((_, actual, why)) => {
                assert_ne!(actual, w, "row {row} is listed as a known difference but the expectation matches it");
                if g != actual {
                    wrong.push(format!("row {row}: listed as a known difference ({why}) but the screen now holds {g:?}; expected {w:?}"));
                }
                known_seen += 1;
            }
            None if g != w => wrong.push(format!("row {row}:\n   want {w:?}\n    got {g:?}")),
            None => {}
        }
    }
    assert_eq!(known_seen, KNOWN.len());

    // ---- what the text does not show: attributes, colours, widths, state ----
    let mut expect = |what: &str, ok: bool| match (ok, KNOWN_CHECKS.iter().find(|k| k.0 == what)) {
        (false, None) => wrong.push(what.to_string()),
        (true, Some((_, why))) => wrong.push(format!("{what}: listed as a known difference ({why}) but it is now right")),
        _ => {}
    };
    let flags = |row, col| cell(&t, row, col).flags;
    let only = |f: Flags, among: Flags, row: usize, col: usize| cell(&t, row, col).flags & among == f;

    // Row 2: one attribute per word, and every reset.
    let attrs = Flags::BOLD | Flags::DIM | Flags::ITALIC | Flags::INVERSE | Flags::HIDDEN | Flags::STRIKEOUT;
    expect("SGR 1 bold", only(Flags::BOLD, attrs, 2, 1));
    expect("SGR 2 dim", only(Flags::DIM, attrs, 2, 6));
    expect("SGR 3 italic", only(Flags::ITALIC, attrs, 2, 10));
    expect("SGR 7 inverse", only(Flags::INVERSE, attrs, 2, 17));
    expect("SGR 8 hidden", only(Flags::HIDDEN, attrs, 2, 25));
    expect("SGR 9 strike", only(Flags::STRIKEOUT, attrs, 2, 32));
    expect("SGR 22 23 27 28 29 undo them all", only(Flags::empty(), attrs, 2, 39));

    // Row 3: every underline style.
    let ul = Flags::ALL_UNDERLINES;
    expect("SGR 4 underline", only(Flags::UNDERLINE, ul, 3, 1));
    expect("SGR 4:2 double underline", only(Flags::DOUBLE_UNDERLINE, ul, 3, 4));
    expect("SGR 4:3 undercurl", only(Flags::UNDERCURL, ul, 3, 7));
    expect("SGR 4:4 dotted underline", only(Flags::DOTTED_UNDERLINE, ul, 3, 10));
    expect("SGR 4:5 dashed underline", only(Flags::DASHED_UNDERLINE, ul, 3, 13));
    expect("SGR 21 double underline", only(Flags::DOUBLE_UNDERLINE, ul, 3, 16));
    expect("underline does not leak into the gap", only(Flags::empty(), ul, 3, 3));
    expect("SGR 4 then 24", only(Flags::UNDERLINE, ul, 3, 18) && only(Flags::empty(), ul, 3, 19));
    expect("SGR 4:3 replaces the style", only(Flags::UNDERCURL, ul, 3, 20));
    expect("SGR 4:0 removes the underline", only(Flags::empty(), ul, 3, 21));
    expect("SGR 4:1 is a plain underline", only(Flags::UNDERLINE, ul, 3, 22));

    // Row 4: every way of naming a colour.
    let rgb = |r, g, b| Color::Spec(Rgb { r, g, b });
    let fg = |col| cell(&t, 4, col).fg;
    let bg = |col| cell(&t, 4, col).bg;
    let uc = |col| cell(&t, 4, col).underline_color();
    expect("SGR 31", fg(1) == Color::Named(NamedColor::Red));
    expect("SGR 91", fg(2) == Color::Named(NamedColor::BrightRed));
    expect("SGR 38;5;208", fg(3) == Color::Indexed(208));
    expect("SGR 38;2;1;2;3", fg(4) == rgb(1, 2, 3));
    expect("SGR 38:2::4:5:6 (colon form, empty colour space)", fg(5) == rgb(4, 5, 6));
    expect("SGR 38:2:7:8:9 (colon form, no colour space)", fg(6) == rgb(7, 8, 9));
    expect("SGR 38:5:39", fg(7) == Color::Indexed(39));
    expect("SGR 39", fg(8) == Color::Named(NamedColor::Foreground));
    expect("SGR 41", bg(9) == Color::Named(NamedColor::Red) && fg(9) == Color::Named(NamedColor::Foreground));
    expect("SGR 101", bg(10) == Color::Named(NamedColor::BrightRed));
    expect("SGR 48;5;17", bg(11) == Color::Indexed(17));
    expect("SGR 48;2;10;20;30", bg(12) == rgb(10, 20, 30));
    expect("SGR 48:2::40:50:60", bg(13) == rgb(40, 50, 60));
    expect("SGR 49", bg(14) == Color::Named(NamedColor::Background));
    expect("SGR 58;2;11;12;13 (underline colour)", uc(15) == Some(rgb(11, 12, 13)) && flags(4, 15).contains(Flags::UNDERLINE));
    expect("SGR 58:2::14:15:16", uc(16) == Some(rgb(14, 15, 16)));
    expect("SGR 58;5;99", uc(17) == Some(Color::Indexed(99)));
    expect("SGR 58:5:98", uc(18) == Some(Color::Indexed(98)));
    expect("SGR 59 resets the underline colour, not the underline", uc(19).is_none() && flags(4, 19).contains(Flags::UNDERLINE));
    expect("SGR 0 resets everything", flags(4, 20) & (ul | attrs) == Flags::empty() && fg(20) == Color::Named(NamedColor::Foreground));
    expect(
        "SGR 1;31;42 in one sequence",
        flags(4, 21).contains(Flags::BOLD) && fg(21) == Color::Named(NamedColor::Red) && bg(21) == Color::Named(NamedColor::Green),
    );
    expect("SGR 22;39;49", !flags(4, 22).contains(Flags::BOLD) && fg(22) == Color::Named(NamedColor::Foreground) && bg(22) == Color::Named(NamedColor::Background));

    // Row 8: widths, and marks that ride on the letter before them.
    expect("wide characters take two cells", flags(8, 1).contains(Flags::WIDE_CHAR) && flags(8, 2).contains(Flags::WIDE_CHAR_SPACER));
    expect("an emoji takes two cells", cell(&t, 8, 11).c == '\u{1f600}' && flags(8, 11).contains(Flags::WIDE_CHAR) && flags(8, 12).contains(Flags::WIDE_CHAR_SPACER));
    expect("a combining mark joins its letter", cell(&t, 8, 14).c == 'e' && cell(&t, 8, 14).zerowidth() == Some(&['\u{301}'][..]));
    expect("two combining marks join one letter", cell(&t, 8, 16).zerowidth() == Some(&['\u{308}', '\u{323}'][..]) && cell(&t, 8, 18).c == '.');
    // Rows 9 and 10: a wide character at the edge.
    expect("a wide character that does not fit leaves the last column empty", cell(&t, 9, 80).c == ' ' && flags(9, 80).contains(Flags::LEADING_WIDE_CHAR_SPACER));
    expect("writing over the second half of a wide character removes the first", cell(&t, 10, 4).c == ' ' && !flags(10, 4).contains(Flags::WIDE_CHAR));
    expect("writing over the first half removes the second", cell(&t, 10, 11).c == ' ' && !flags(10, 11).contains(Flags::WIDE_CHAR_SPACER));
    // Rows 20 and 21: the row that wrapped knows it did.
    expect("a wrapped row is marked as continuing", flags(20, 80).contains(Flags::WRAPLINE));
    expect("a row cut by 'no wrap' is not", !flags(21, 80).contains(Flags::WRAPLINE));
    // Row 22: the hyperlink covers its text and nothing else.
    let link = |col| cell(&t, 22, col).hyperlink().map(|h| h.uri().to_string());
    expect("OSC 8 hyperlink", link(11).as_deref() == Some("https://example.com/") && link(14).is_some() && link(10).is_none() && link(15).is_none());

    // ---- state left behind ----
    {
        let term = t.term.lock().unwrap();
        let mode = *term.mode();
        expect("back on the main screen", !mode.contains(TermMode::ALT_SCREEN));
        expect("origin mode is off again", !mode.contains(TermMode::ORIGIN));
        expect("insert mode is off again", !mode.contains(TermMode::INSERT));
        expect("wrap is on again", mode.contains(TermMode::LINE_WRAP));
        expect("DECSCUSR 4 gives a steady underline cursor", term.cursor_style().shape == CursorShape::Underline && !term.cursor_style().blinking);
        let cur = term.grid().cursor.point;
        // "end", four clusters and the bar: the cursor follows the last cell written.
        expect("the cursor is on the last row", cur.line == Line(23));
    }
    expect("OSC 2 sets the title", titles.lock().unwrap().as_slice() == ["conformance".to_string()]);

    for (row, _, why) in KNOWN {
        println!("known difference, row {row}: {why}");
    }
    for (what, why) in KNOWN_CHECKS {
        println!("known difference, {what}: {why}");
    }
    assert!(wrong.is_empty(), "{} differences:\n{}", wrong.len(), wrong.join("\n"));
}

/// Text alone cannot show how many cells a character was given. Four things
/// a person sees as one character each (a heart with its emoji selector, a
/// thumb with a skin tone, a family joined with ZWJ, a flag) should take two
/// cells apiece. The terminal library has no grapheme clustering: it places
/// every code point by its own width. This pins what it does today, so the
/// gap is on record and the test fails the day it closes.
#[test]
fn clusters_are_not_yet_one_character() {
    let t = terminal(Arc::new(Mutex::new(Vec::new())));
    t.feed(SAMPLE);
    let w = widths(&t, 24);
    let cells = |from: usize, n: usize| w[from..from + n].iter().map(|(_, c)| c).sum::<usize>();
    assert_eq!(w[..3].iter().map(|(c, _)| *c).collect::<String>(), "end");
    assert_eq!(w.last().map(|(c, _)| *c), Some('|'));
    let (heart, thumb, family, flag) = (cells(3, 1), cells(4, 2), cells(6, 3), cells(9, 2));
    println!("cells given to ❤️ 👍🏽 👨‍👩‍👧 🇯🇵: {heart}, {thumb}, {family}, {flag} (a terminal with grapheme clustering gives 2, 2, 2, 2)");
    assert_eq!((heart, thumb, family, flag), (1, 4, 6, 2), "the terminal library's handling of clusters changed: {w:?}");
}

/// Synchronized updates (mode 2026), which agent interfaces use to redraw
/// without flicker: while a program holds an update open nothing is shown,
/// in however many pieces it arrives; it is shown whole when the update
/// ends, and a program that never ends it cannot freeze the screen.
#[test]
fn a_synchronized_update_is_shown_whole_or_not_at_all() {
    use crate::session::SessionEvent;
    use std::time::{Duration, Instant};
    let (tx, rx) = crossbeam_channel::bounded::<SessionEvent>(16);
    let t = Terminal::new(80, 24, Theme::void_light(), Arc::new(|_| {}), None);
    t.attach(rx);
    let send = |b: &[u8]| tx.send(SessionEvent::Output(b.to_vec())).unwrap();
    let woke_within = |ms: u64| {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            if t.take_wake() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    };

    send(b"before ");
    assert!(woke_within(1000), "ordinary output asks for a frame");
    send(b"\x1b[?2026hhalf");
    send(b" of it");
    assert!(!woke_within(60), "no frame while the update is open");
    assert_eq!(screen(&t)[0], "before", "and the screen does not hold half of it");
    send(b" done\x1b[?2026l");
    assert!(woke_within(1000), "the finished update asks for a frame");
    assert_eq!(screen(&t)[0], "before half of it done");

    send(b"\x1b[?2026h and stuck");
    assert!(!woke_within(60));
    assert!(woke_within(2000), "an update left open is shown anyway after the time limit");
    assert_eq!(screen(&t)[0], "before half of it done and stuck");
}

/// The keyboard protocol's own bookkeeping: push, pop, set, query, and a
/// separate stack for the alternate screen.
#[test]
fn keyboard_protocol_flags_are_tracked_and_reported() {
    use crate::session::SessionCmd;
    let (tx, rx) = crossbeam_channel::unbounded::<SessionCmd>();
    let t = Terminal::new(80, 24, Theme::void_light(), Arc::new(|_| {}), Some(tx));
    let flags = || crate::input::kitty_flags(*t.term.lock().unwrap().mode());
    let replies = || -> Vec<String> {
        rx.try_iter()
            .filter_map(|c| match c {
                SessionCmd::Input(b) => Some(crate::probe::show(&b)),
                _ => None,
            })
            .collect()
    };

    assert_eq!(flags(), 0);
    t.feed(b"\x1b[?u");
    assert_eq!(replies(), ["\\e[?0u"], "a terminal that supports the protocol always answers the query");
    t.feed(b"\x1b[>1u\x1b[?u");
    assert_eq!((flags(), replies()), (1, vec!["\\e[?1u".to_string()]), "push");
    t.feed(b"\x1b[>27u\x1b[?u");
    assert_eq!((flags(), replies()), (27, vec!["\\e[?27u".to_string()]), "push again");
    t.feed(b"\x1b[<u");
    assert_eq!(flags(), 1, "pop restores what was there before");
    // Set in place: all bits, then add some, then remove some. The answer to
    // the query must follow (the terminal library alone would still say 1).
    t.feed(b"\x1b[=5u\x1b[?u");
    assert_eq!((flags(), replies()), (5, vec!["\\e[?5u".to_string()]), "set");
    t.feed(b"\x1b[=10;2u\x1b[?u\x1b[=4;3u\x1b[?u");
    assert_eq!((flags(), replies()), (11, vec!["\\e[?15u".to_string(), "\\e[?11u".to_string()]), "add bits, then remove bits");
    // The alternate screen has its own stack; leaving it brings the main one back.
    t.feed(b"\x1b[<9u\x1b[>3u\x1b[?1049h");
    assert_eq!(flags(), 0, "a full-screen program starts from nothing");
    t.feed(b"\x1b[>8u\x1b[?1049l\x1b[?u");
    assert_eq!((flags(), replies()), (3, vec!["\\e[?3u".to_string()]), "back on the main screen");
    t.feed(b"\x1b[<u\x1b[<u\x1b[<u");
    assert_eq!(flags(), 0, "popping an empty stack leaves no flags");
    // Ctrl+Shift+Delete in the window: whatever a dead program left on is dropped.
    t.feed(b"\x1b[>1u\x1b[>3u\x1b[>31u\x1b[=15u");
    assert_eq!(flags(), 15);
    t.feed(crate::input::DROP_KEYBOARD_FLAGS);
    assert_eq!(flags(), 0, "the way out of a protocol left switched on");
    // A device-attributes query after the flags query: the flags answer comes first,
    // which is how programs detect the protocol.
    t.feed(b"\x1b[?u\x1b[c");
    let r = replies();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0], "\\e[?0u");
    assert!(r[1].ends_with('c'), "{r:?}");
}
