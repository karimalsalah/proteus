//! `proteus shot`: render terminal content to a PNG with no window. This is
//! how the engine is verified, and how an agent can see its own output.
//!
//!   proteus shot --vt FILE | --journal FILE | --cmd "EXE ARGS..."
//!                [--out PNG] [--size WxH] [--scale S] [--font PT]
//!                [--wait SECS] [--reveal-ms N] [--until SECS] [--time-build N] [--scrub]
//!                [--cwd DIR]
//!   proteus shot --world ...   the World view instead (see world_shot.rs)
//!
//! `--until` shows a journal as it stood that many seconds in. It is a seek
//! (recall.rs): the nearest keyframe and the records after it, at the sizes
//! the session really had, then fitted to this picture's grid.
//! `--scrub` draws it the way the window does while scrubbing, with the bar.
//! `--scrub --indexing 43` draws the moment before a first seek has arrived,
//! and `--notice "WORDS"` the line shown when there is no past to show.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::gpu;
use crate::recall::{self, Target, Timeline};
use crate::renderer::Renderer;
use crate::scrub::BAR_ROWS;
use crate::session::{self, SessionEvent, SessionSpec};
use crate::term::Terminal;
use crate::theme::Theme;
use crate::Res;

pub fn write_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> Res<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header()?;
    wr.write_image_data(rgba)?;
    Ok(())
}

pub fn run(args: &[String]) -> Res<()> {
    let mut vt: Option<PathBuf> = None;
    let mut jr: Option<PathBuf> = None;
    let mut cmd: Option<String> = None;
    let mut out = PathBuf::from("proteus-shot.png");
    let (mut w, mut h) = (2200u32, 1300u32);
    let mut scale = 2.0f32;
    let mut font_pt = 15.0f32;
    let mut wait = 2.0f32;
    let mut reveal_ms: Option<f32> = None;
    // Time travel: show a journal as it stood this many seconds in.
    let mut until: Option<f64> = None;
    // Measure the grid build over this many repeats (0 = do not measure).
    let mut time_build = 0usize;
    let mut scrub = false;
    // The journal and the position shown, when the scrub bar is wanted.
    let mut past: Option<(PathBuf, usize)> = None;
    // Two states of the window that last a moment and so are drawn on
    // request: the bar while a first seek is still indexing (given a
    // percentage), and the line shown when there is no past to show.
    let mut indexing: Option<u32> = None;
    let mut notice: Option<String> = None;
    // The World view instead of the grid (world_shot.rs), and its options.
    let (mut world, mut cwd, mut hover, mut zoom, mut time) = (false, None::<PathBuf>, None, None, 0usize);
    let pair = |v: &str| -> Option<(f64, f64)> {
        let (a, b) = v.split_once(',')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    };

    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--scrub" => {
                scrub = true;
                i += 1;
                continue;
            }
            "--vt" => vt = Some(PathBuf::from(&val)),
            "--journal" => jr = Some(PathBuf::from(&val)),
            "--cmd" => cmd = Some(val.clone()),
            "--out" => out = PathBuf::from(&val),
            "--size" => {
                let (a, b) = val.split_once('x').ok_or("size must be WxH")?;
                w = a.parse()?;
                h = b.parse()?;
            }
            "--scale" => scale = val.parse()?,
            "--font" => font_pt = val.parse()?,
            "--wait" => wait = val.parse()?,
            "--reveal-ms" => reveal_ms = Some(val.parse()?),
            "--until" => until = Some(val.parse()?),
            "--time-build" => time_build = val.parse()?,
            "--indexing" => indexing = Some(val.parse()?),
            "--notice" => notice = Some(val.clone()),
            "--world" => {
                world = true;
                i += 1;
                continue;
            }
            "--cwd" => cwd = Some(PathBuf::from(&val)),
            "--hover" => hover = Some(pair(&val).map(|(x, y)| (x as f32, y as f32)).ok_or("hover must be X,Y")?),
            "--zoom" => zoom = Some(pair(&val).ok_or("zoom must be FROM,TO in seconds")?),
            "--time" => time = val.parse()?,
            other => return Err(format!("unknown option {other}").into()),
        }
        i += 2;
    }

    if world {
        let o = crate::world_shot::Opts { journal: jr, cmd, cwd, out, size: (w, h), scale, font_pt, wait, until, reveal_ms, hover, zoom, time };
        return crate::world_shot::run(&o);
    }

    let gpu = gpu::init(gpu::instance(), None)?;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut r = Renderer::new(&gpu.device, format, font_pt * scale, Theme::void_light());
    r.resize(&gpu.device, w, h, 12.0 * scale);
    let (cols, rows) = r.grid();

    let notify: crate::term::Notify = Arc::new(|_| {});
    let started = Instant::now();
    let term = if let Some(c) = cmd {
        let mut parts = c.split_whitespace().map(String::from);
        let exe = parts.next().ok_or("empty --cmd")?;
        let h = session::spawn_local(SessionSpec {
            exe,
            args: parts.collect(),
            cwd,
            cols: cols as u16,
            rows: rows as u16,
            journal: false,
            env: None,
        })?;
        let term = Terminal::new(cols, rows, Theme::void_light(), notify, Some(h.cmds.clone()));
        let deadline = Instant::now() + Duration::from_secs_f32(wait);
        let (mut first, mut last, mut total) = (None, Instant::now(), 0usize);
        loop {
            match h.events.recv_deadline(deadline) {
                Ok(SessionEvent::Output(b)) => {
                    first.get_or_insert_with(Instant::now);
                    total += b.len();
                    term.feed(&b);
                    last = Instant::now();
                }
                Ok(SessionEvent::Exit(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        // Parse-only rate: what the pseudo-terminal delivers with no drawing.
        if let Some(f) = first {
            let s = last.duration_since(f).as_secs_f64().max(1e-6);
            println!("session: {total} bytes in {s:.3}s = {:.2} MB/s (no rendering)", total as f64 / 1e6 / s);
        }
        let _ = h.cmds.send(session::SessionCmd::Kill);
        term
    } else {
        let term = Terminal::new(cols, rows, Theme::void_light(), notify, None);
        if let Some(p) = jr {
            // While a first seek is on its way the window still shows the
            // present, full height, with the bar over its last rows.
            let waiting = scrub && indexing.is_some();
            let wanted = until.map_or(Target::End, |s| Target::Time(recall::micros(s)));
            let target = if waiting { Target::End } else { wanted };
            let view_rows = if scrub && !waiting { rows.saturating_sub(BAR_ROWS).max(1) } else { rows };
            let t0 = Instant::now();
            let s = recall::Index::open(&p)?.seek(target, Some((cols, view_rows)))?;
            println!(
                "recall: {} records, t={:.3}s, via {}, {} journal bytes replayed, {:.2} ms",
                s.pos,
                s.t_us as f64 / 1e6,
                s.how,
                s.replayed,
                t0.elapsed().as_secs_f64() * 1e3
            );
            past = scrub.then(|| (p, if waiting { usize::MAX } else { s.pos as usize }));
            *term.term.lock().map_err(|_| "terminal lock")? = s.replay.term;
        } else if let Some(p) = vt {
            term.feed(&std::fs::read(&p)?);
        } else {
            return Err("give one of --vt, --journal or --cmd".into());
        }
        term
    };
    term.settle();

    let now = started.elapsed().as_secs_f64();
    let t = term.term.lock().map_err(|_| "terminal lock")?;
    let at = match reveal_ms {
        // Two builds: an empty baseline, then the content, so every glyph is
        // "born" at `now` and the capture lands mid-reveal.
        Some(ms) => {
            let blank = Terminal::new(cols, rows, Theme::void_light(), Arc::new(|_| {}), None);
            let b = blank.term.lock().map_err(|_| "terminal lock")?;
            r.build(&gpu.device, &gpu.queue, &b, now, true);
            drop(b);
            r.build(&gpu.device, &gpu.queue, &t, now, true);
            let at = now + ms as f64 / 1000.0;
            r.simulate_field(&gpu.device, &gpu.queue, now, at);
            at
        }
        None => {
            r.build(&gpu.device, &gpu.queue, &t, now, true);
            now + 30.0
        }
    };
    // `--time-build N`: the renderer's own cost of turning this grid into GPU
    // instances, measured over N more builds of the same content.
    if time_build > 0 {
        let mut us: Vec<f64> = Vec::with_capacity(time_build);
        for i in 0..time_build {
            let t0 = Instant::now();
            r.build(&gpu.device, &gpu.queue, &t, now + 31.0 + i as f64 * 0.001, true);
            us.push(t0.elapsed().as_secs_f64() * 1e6);
            // A window submits a frame after every build; without this the
            // staged buffer writes pile up and the timing measures that.
            gpu.queue.submit(std::iter::empty());
            let _ = gpu.device.poll(wgpu::PollType::Poll);
        }
        let avg = us.iter().sum::<f64>() / us.len() as f64;
        us.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f64| us[((us.len() as f64 - 1.0) * p).round() as usize];
        println!(
            "{{\"build_us\": {{\"n\": {}, \"avg\": {:.1}, \"p50\": {:.1}, \"p95\": {:.1}, \"max\": {:.1}}}, \"grid\": [{}, {}]}}",
            us.len(),
            avg,
            pct(0.5),
            pct(0.95),
            pct(1.0),
            cols,
            rows
        );
    }
    if let Some((p, pos)) = &past {
        // The same bar the window draws while scrubbing, from the same data.
        let tl = Timeline::scan(p)?;
        let (_, _, n, _) = crate::scrub::track(r.screen[0], r.pad[0], r.fonts.metrics.cell_w);
        let hist = tl.histogram(n);
        let pos = if *pos == usize::MAX { tl.pos_at(recall::micros(until.unwrap_or(0.0))) } else { *pos };
        let note = indexing.map(|p| format!("indexing {p}%"));
        let data = crate::scrub::BarData {
            hist: &hist,
            duration_us: tl.duration_us(),
            t_us: tl.time_at(pos),
            started_unix: tl.head.meta["started_unix"].as_u64(),
            output: tl.outputs_at(pos),
            outputs: tl.outs.len(),
            exit: tl.exit,
            note: note.as_deref(),
            waiting: indexing.is_some(),
        };
        let bar = crate::scrub::bar(r.screen, r.pad, &r.fonts.metrics, &r.theme, rows, &data);
        if r.set_overlay(&gpu.device, &gpu.queue, Some(&bar)) {
            r.suppress_reveal = true;
            r.build(&gpu.device, &gpu.queue, &t, now, true);
        }
    } else if let Some(words) = &notice {
        let line = crate::scrub::notice(r.screen, r.pad, &r.fonts.metrics, &r.theme, rows, words);
        if r.set_overlay(&gpu.device, &gpu.queue, Some(&line)) {
            r.suppress_reveal = true;
            r.build(&gpu.device, &gpu.queue, &t, now, true);
        }
    }
    drop(t);
    let (pw, ph, rgba) = r.capture(&gpu.device, &gpu.queue, at);
    write_png(&out, pw, ph, &rgba)?;
    let info = gpu.adapter.get_info();
    println!("{} {}x{} px, grid {}x{}, {:?} on {}", out.display(), pw, ph, cols, rows, info.backend, info.name);
    Ok(())
}
