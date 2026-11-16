//! `proteus recall`: reach into a session's past from the command line.
//!
//!   proteus recall find TEXT --journal F [--ignore-case] [--json]
//!       every stretch of time TEXT was on screen: when it appeared, when it left
//!   proteus recall at --journal F --until SECS [--json]
//!       the screen as text at that moment
//!   proteus recall index --journal F [--json]
//!       build or extend the keyframe index and say what it holds
//!   proteus recall bench --journal F [--n 20] [--seed 1] [--json]
//!       time seeks to random instants with and without the index, and check
//!       that both give the same terminal
//!   proteus recall snapshot --journal F
//!       what one snapshot of the whole session, scrollback included, costs
//!       to take and to restore (what a fast attach would be built on)
//!   proteus recall record --out F --cmd "EXE ARGS" [--size COLSxROWS] [--wait SECS]
//!                         [--resize SECS:COLSxROWS]... [--type SECS:TEXT]...
//!       run a program on a real pseudo-terminal with no window and journal it
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use serde_json::json;

use crate::journal::{self, JournalWriter};
use crate::recall::{self, Index, Target};
use crate::session::{self, SessionCmd, SessionEvent, SessionSpec};
use crate::term::Terminal;
use crate::theme::Theme;
use crate::Res;

struct Args {
    words: Vec<String>,
    opts: Vec<(String, String)>,
    flags: Vec<String>,
}

impl Args {
    fn parse(args: &[String], flags: &[&str]) -> Args {
        let mut a = Args { words: Vec::new(), opts: Vec::new(), flags: Vec::new() };
        let mut i = 0;
        while i < args.len() {
            let s = &args[i];
            if flags.contains(&s.as_str()) {
                a.flags.push(s.clone());
            } else if s.starts_with("--") {
                a.opts.push((s.clone(), args.get(i + 1).cloned().unwrap_or_default()));
                i += 1;
            } else {
                a.words.push(s.clone());
            }
            i += 1;
        }
        a
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.opts.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.opts.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    fn journal(&self) -> Res<PathBuf> {
        Ok(PathBuf::from(self.get("--journal").ok_or("give --journal FILE")?))
    }
}

fn secs(us: u64) -> f64 {
    recall::seconds(us)
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e5).round() / 100.0
}

fn size(s: &str) -> Res<(u16, u16)> {
    let (c, r) = s.split_once('x').ok_or("size must be COLSxROWS")?;
    Ok((c.parse()?, r.parse()?))
}

pub fn run(args: &[String]) -> Res<()> {
    let a = Args::parse(args.get(1..).unwrap_or(&[]), &["--json", "--ignore-case"]);
    match args.first().map(String::as_str) {
        Some("find") => find(&a),
        Some("at") => at(&a),
        Some("index") => index(&a),
        Some("bench") => bench(&a),
        Some("snapshot") => snapshot(&a),
        Some("record") => record(&a),
        _ => Err("usage: proteus recall find|at|index|bench|snapshot|record ... (see src/recall_cli.rs)".into()),
    }
}

/// What `find --json` prints. Each moment is given in seconds to the
/// microsecond: handed to `at --until` as printed, it is that moment.
pub fn find_report(path: &Path, text: &str, ignore_case: bool) -> Res<serde_json::Value> {
    let t0 = Instant::now();
    let f = recall::find(path, text, ignore_case)?;
    let took = t0.elapsed();
    let hits: Vec<_> = f
        .hits
        .iter()
        .map(|h| json!({ "appeared": secs(h.appeared_us), "left": h.left_us.map(secs), "row": h.row, "line": h.line }))
        .collect();
    Ok(json!({
        "text": text, "journal": path.to_string_lossy(), "hits": hits,
        "records": f.records, "bytes": f.bytes, "screens_examined": f.looks, "ms": ms(took),
    }))
}

/// The terminal at the moment `at --until SECONDS` names.
pub fn seek_until(path: &Path, until: &str) -> Res<recall::Seeked> {
    let seconds: f64 = until.parse()?;
    Ok(recall::seek(path, recall::micros(seconds), None)?)
}

fn find(a: &Args) -> Res<()> {
    let path = a.journal()?;
    let text = a.words.join(" ");
    if text.is_empty() {
        return Err("give the text to look for".into());
    }
    let v = find_report(&path, &text, a.has("--ignore-case"))?;
    if a.has("--json") {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let hits = v["hits"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    for h in hits {
        // Seconds as `at --until` takes them, then the same moment as a clock reads.
        let (seen, gone) = (h["appeared"].as_f64().unwrap_or(0.0), h["left"].as_f64());
        let left = match gone {
            Some(t) if t == seen => "flew past".to_string(),
            Some(t) => format!("left {t:.6} ({:.3} s on screen)", t - seen),
            None => "still on screen at the end".to_string(),
        };
        let row = h["row"].as_u64().unwrap_or(0) + 1;
        println!("appeared {seen:.6} ({})  {left}  row {row}: {}", recall::clock(recall::micros(seen)), h["line"].as_str().unwrap_or("").trim());
    }
    println!("{} time(s) on screen; {} records, {} bytes, {:.1} ms", hits.len(), v["records"], v["bytes"], v["ms"].as_f64().unwrap_or(0.0));
    Ok(())
}

fn at(a: &Args) -> Res<()> {
    let path = a.journal()?;
    let s = seek_until(&path, a.get("--until").ok_or("give --until SECONDS")?)?;
    let lines = recall::screen_text(&s.replay.term);
    if a.has("--json") {
        let c = s.replay.term.grid().cursor.point;
        let (cols, rows) = s.replay.size();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "t": secs(s.t_us), "records": s.pos, "at_end": s.at_end, "cols": cols, "rows": rows,
                "cursor": [c.line.0, c.column.0], "how": s.how, "replayed_bytes": s.replayed, "lines": lines,
            }))?
        );
    } else {
        let last = lines.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
        lines[..last].iter().for_each(|l| println!("{l}"));
    }
    Ok(())
}

fn index(a: &Args) -> Res<()> {
    let path = a.journal()?;
    let t0 = Instant::now();
    let mut ix = Index::open(&path)?;
    let had = ix.frames.len();
    ix.build(&mut |_, _| {})?;
    let took = t0.elapsed();
    let journal_bytes = std::fs::metadata(&path)?.len();
    let side = std::fs::metadata(recall::sidecar(&path)).map(|m| m.len()).unwrap_or(0);
    let v = json!({
        "journal": path.to_string_lossy(), "journal_bytes": journal_bytes,
        "keyframes": ix.frames.len(), "keyframe_bytes": ix.bytes(), "keyframes_already_there": had, "added": ix.stats.added,
        "rejected": ix.stats.rejected, "first_reject": ix.first_reject, "deferred": ix.stats.deferred,
        "sidecar": recall::sidecar(&path).to_string_lossy(), "sidecar_bytes": side,
        "sidecar_pct_of_journal": (side as f64 * 1000.0 / journal_bytes.max(1) as f64).round() / 10.0,
        "ms": ms(took),
    });
    if a.has("--json") {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "{} keyframes ({} new, {} dropped) over {} bytes of journal; sidecar {} bytes ({}%); {} ms",
            v["keyframes"], v["added"], v["rejected"], journal_bytes, side, v["sidecar_pct_of_journal"], v["ms"]
        );
    }
    Ok(())
}

/// What a snapshot of the whole session, scrollback included, costs: the
/// numbers a fast attach would be built on. Replays the journal to its last
/// point of rest, takes the snapshot, restores it, and checks the two are
/// the same terminal.
fn snapshot(a: &Args) -> Res<()> {
    let path = a.journal()?;
    let (head, mut rd) = recall::Reader::open(&path)?;
    let (cols, rows) = head.size();
    let mut live = recall::Replay::new(cols, rows, true);
    let (mut buf, mut best, mut records, mut rest) = (Vec::new(), None, 0u64, 0u64);
    let t0 = Instant::now();
    while let Some(h) = rd.next() {
        rd.payload(&h, &mut buf)?;
        live.apply(h.kind, &buf);
        records += 1;
        if live.at_rest() {
            rest = records;
        }
    }
    let replay = t0.elapsed();
    if !live.at_rest() {
        // The journal ends mid-sequence: go back to the last point of rest.
        (live, rd) = (recall::Replay::new(cols, rows, true), recall::Reader::open(&path)?.1);
        for _ in 0..rest {
            let Some(h) = rd.next() else { break };
            rd.payload(&h, &mut buf)?;
            live.apply(h.kind, &buf);
        }
        records = rest;
    }
    let t1 = Instant::now();
    if let Some(snap) = live.snapshot(usize::MAX) {
        let take = t1.elapsed();
        let t2 = Instant::now();
        let back = recall::Replay::restore(&snap, false)?;
        let restore = t2.elapsed();
        best = Some((snap.len(), take, restore, recall::diff(&live.term, &back.term, None)));
    }
    let v = match &best {
        Some((bytes, take, restore, difference)) => json!({
            "journal_bytes": std::fs::metadata(&path)?.len(), "records": records, "grid": [cols, rows],
            "scrollback_rows": live.term.history_size(), "replay_from_start_ms": ms(replay),
            "snapshot_bytes": bytes, "take_ms": ms(*take), "restore_ms": ms(*restore), "difference": difference,
        }),
        None => json!({ "records": records, "note": "the journal does not end at a point of rest, so no snapshot can be taken there" }),
    };
    println!("{}", serde_json::to_string_pretty(&v)?);
    match best {
        Some((.., Some(d))) => Err(format!("the restored snapshot differs: {d}").into()),
        _ => Ok(()),
    }
}

/// The path `shot --journal F --until T` took before recall existed: load
/// the whole file, then feed every output record up to the moment.
fn old_path(path: &Path, t_us: u64, cols: usize, rows: usize) -> Res<Duration> {
    let t0 = Instant::now();
    let (_, recs) = journal::read(path)?;
    let term = Terminal::new(cols, rows, Theme::void_light(), Arc::new(|_| {}), None);
    for rec in recs.iter().filter(|r| r.kind == journal::K_OUTPUT && r.t_us <= t_us) {
        term.feed(&rec.data);
    }
    term.settle();
    Ok(t0.elapsed())
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v.get(((v.len() as f64 - 1.0) * p).round().max(0.0) as usize).copied().unwrap_or(0.0)
}

fn bench(a: &Args) -> Res<()> {
    let path = a.journal()?;
    let n: usize = a.get("--n").map(str::parse).transpose()?.unwrap_or(20);
    let mut seed: u64 = a.get("--seed").map(str::parse).transpose()?.unwrap_or(1);
    // Reading every record's header: what the window does at the first step into the past.
    let t0 = Instant::now();
    let tl = recall::Timeline::scan(&path)?;
    let scan = t0.elapsed();
    let (cols, rows) = tl.head.size();
    let journal_bytes = std::fs::metadata(&path)?.len();

    // Build the index first, timed by itself; seeks are then measured the
    // way a second and later use would find them.
    let had_sidecar = recall::sidecar(&path).exists();
    let t0 = Instant::now();
    let mut ix = Index::open(&path)?;
    let had = ix.frames.len();
    ix.build(&mut |_, _| {})?;
    let build = t0.elapsed();
    let (keyframes, stats, first_reject) = (ix.frames.len(), ix.stats, ix.first_reject.clone());
    drop(ix);

    let (mut before, mut after, mut same, mut diffs, mut rows_out) = (Vec::new(), Vec::new(), 0usize, Vec::new(), Vec::new());
    for _ in 0..n {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let t_us = seed % (tl.duration_us() + 1);

        let old = old_path(&path, t_us, cols, rows)?;
        // Open and seek together: what one `recall at` or `shot --until` pays.
        let t1 = Instant::now();
        let got = Index::open(&path)?.seek(Target::Time(t_us), None)?;
        let new = t1.elapsed();
        // The reference: every record from the start, at the session's own sizes.
        let want = recall::replay_from_start(&path, Target::Time(t_us), None)?;
        let depth = want.replay.term.history_size().min(got.replay.term.history_size());
        match recall::diff(&want.replay.term, &got.replay.term, Some(depth)) {
            None if want.pos == got.pos => same += 1,
            None => diffs.push(format!("t={}: {} records vs {}", secs(t_us), want.pos, got.pos)),
            Some(d) => diffs.push(format!("t={}: {d}", secs(t_us))),
        }
        before.push(ms(old));
        after.push(ms(new));
        rows_out.push(json!({
            "t": secs(t_us), "records": got.pos, "before_ms": ms(old), "after_ms": ms(new),
            "how": got.how, "replayed_bytes": got.replayed, "scrollback_rows_compared": depth,
        }));
    }
    let side = std::fs::metadata(recall::sidecar(&path)).map(|m| m.len()).unwrap_or(0);
    let v = json!({
        "journal": path.to_string_lossy(), "journal_bytes": journal_bytes, "records": tl.evs.len(),
        "output_records": tl.outs.len(), "session_seconds": secs(tl.duration_us()), "grid": [cols, rows],
        "timeline_scan_ms": ms(scan),
        "index": {
            "keyframes": keyframes, "already_there": had, "sidecar_existed": had_sidecar, "build_ms": ms(build),
            "sidecar_bytes": side, "rejected": stats.rejected, "first_reject": first_reject, "deferred": stats.deferred,
        },
        "instants": n,
        "before_ms": { "p50": pct(&mut before, 0.5), "max": pct(&mut before, 1.0), "mean": before.iter().sum::<f64>() / n.max(1) as f64 },
        "after_ms": { "p50": pct(&mut after, 0.5), "max": pct(&mut after, 1.0), "mean": after.iter().sum::<f64>() / n.max(1) as f64 },
        "identical": same, "different": diffs, "each": rows_out,
    });
    if a.has("--json") {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "{} bytes, {} records (headers read in {} ms), {} keyframes (built in {} ms, sidecar {} bytes)",
            journal_bytes, tl.evs.len(), ms(scan), keyframes, ms(build), side
        );
        println!("seek to {n} random instants: before p50 {} ms max {} ms | after p50 {} ms max {} ms", v["before_ms"]["p50"], v["before_ms"]["max"], v["after_ms"]["p50"], v["after_ms"]["max"]);
        println!("identical to a replay from the start: {same} of {n}");
        diffs.iter().for_each(|d| println!("  DIFFERENT {d}"));
    }
    if !diffs.is_empty() {
        return Err(format!("{} of {n} instants did not match a replay from the start", diffs.len()).into());
    }
    Ok(())
}

/// Journal a program's output with no window. Records are made the way the
/// daemon makes them: chunks already waiting are merged into one.
pub fn record_to(
    out: &Path,
    exe: &str,
    args: &[String],
    (cols, rows): (u16, u16),
    wait: Duration,
    mut resizes: Vec<(f64, u16, u16)>,
    mut typed: Vec<(f64, Vec<u8>)>,
) -> Res<(u64, Option<u32>)> {
    let h = session::spawn_local(SessionSpec {
        exe: exe.to_string(),
        args: args.to_vec(),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        journal: false,
        env: None,
    })?;
    let meta = json!({ "v": 1, "session": "recorded", "exe": exe, "args": args, "cols": cols, "rows": rows, "started_unix": crate::paths::unix_secs() });
    let mut j = JournalWriter::create(out, &meta)?;
    j.record(journal::K_RESIZE, &session::resize_payload(cols, rows))?;
    // A terminal answers the program's questions (cursor position, colours),
    // as a window would; without answers some programs wait forever.
    let mut term = Terminal::new(cols as usize, rows as usize, Theme::void_light(), Arc::new(|_| {}), Some(h.cmds.clone()));
    resizes.sort_by(|a, b| a.0.total_cmp(&b.0));
    typed.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (start, mut bytes, mut code) = (Instant::now(), 0u64, None);
    let deadline = start + wait;
    let mut held: Option<SessionEvent> = None;
    loop {
        if Instant::now() >= deadline {
            let _ = h.cmds.send(SessionCmd::Kill);
            break;
        }
        let now = start.elapsed().as_secs_f64();
        while resizes.first().is_some_and(|r| r.0 <= now) {
            let (_, c, r) = resizes.remove(0);
            j.record(journal::K_RESIZE, &session::resize_payload(c, r))?;
            term.resize(c as usize, r as usize);
        }
        while typed.first().is_some_and(|t| t.0 <= now) {
            let _ = h.cmds.send(SessionCmd::Input(typed.remove(0).1));
        }
        let next = [resizes.first().map(|r| r.0), typed.first().map(|t| t.0)].into_iter().flatten().fold(f64::MAX, f64::min);
        let until = deadline.min(start + Duration::from_secs_f64(next.min(1e9)));
        let ev = match held.take() {
            Some(e) => Ok(e),
            None => h.events.recv_deadline(until),
        };
        match ev {
            Ok(SessionEvent::Output(mut b)) => {
                while b.len() < 256 << 10 {
                    match h.events.try_recv() {
                        Ok(SessionEvent::Output(more)) => b.extend_from_slice(&more),
                        Ok(other) => {
                            held = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                bytes += b.len() as u64;
                j.record(journal::K_OUTPUT, &b)?;
                term.feed(&b);
            }
            Ok(SessionEvent::Exit(c)) => {
                code = c;
                j.record(journal::K_EXIT, &c.unwrap_or(u32::MAX).to_le_bytes())?;
                break;
            }
            Ok(_) | Err(_) => {}
        }
    }
    j.flush()?;
    Ok((bytes, code))
}

fn record(a: &Args) -> Res<()> {
    let out = PathBuf::from(a.get("--out").ok_or("give --out FILE.prtj")?);
    let cmd = a.get("--cmd").ok_or("give --cmd \"EXE ARGS\"")?;
    let mut parts = cmd.split_whitespace().map(String::from);
    let exe = parts.next().ok_or("empty --cmd")?;
    let args: Vec<String> = parts.collect();
    let grid = size(a.get("--size").unwrap_or("120x40"))?;
    let wait: f64 = a.get("--wait").map(str::parse).transpose()?.unwrap_or(30.0);
    let timed = |s: &str| -> Res<(f64, String)> {
        let (t, rest) = s.split_once(':').ok_or("expected SECS:VALUE")?;
        Ok((t.parse()?, rest.to_string()))
    };
    let mut resizes = Vec::new();
    for r in a.all("--resize") {
        let (t, v) = timed(r)?;
        let (c, r) = size(&v)?;
        resizes.push((t, c, r));
    }
    let mut typed = Vec::new();
    for t in a.all("--type") {
        let (t, v) = timed(t)?;
        typed.push((t, crate::unescape(&v)));
    }
    let t0 = Instant::now();
    let (bytes, code) = record_to(&out, &exe, &args, grid, Duration::from_secs_f64(wait), resizes, typed)?;
    println!(
        "{}",
        json!({ "journal": out.to_string_lossy(), "output_bytes": bytes, "exit_code": code, "seconds": secs(t0.elapsed().as_micros() as u64), "grid": [grid.0, grid.1] })
    );
    Ok(())
}
