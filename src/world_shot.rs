//! `proteus shot --world`: the World view rendered to a PNG with no window,
//! through the same scene builder and the same GPU path the window uses.
//!
//!   proteus shot --world --journal FILE [--until SECS] [--out PNG] [--size WxH]
//!   proteus shot --world --cmd "EXE ARGS..." [--cwd DIR] [--wait SECS]
//!                [--reveal-ms N] [--hover X,Y] [--zoom FROM,TO] [--time N]
//!
//! `--journal` shows a recorded session (with `--until`, as it stood that
//! many seconds in). `--cmd` runs a real session now, journaled, feeding the
//! view the way a window is fed (events as they arrive), and says where the
//! journal is. `--reveal-ms N` captures N ms after the latest event, while
//! what has just arrived is still lit; without it the picture is settled.
//! `--hover X,Y` puts the mouse at a pixel; `--zoom FROM,TO` shows that
//! range of seconds; `--time N` times N more builds of the scene.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::gpu;
use crate::procs::Ledger;
use crate::procs_win::now_us;
use crate::renderer::Renderer;
use crate::session::{self, SessionCmd, SessionEvent, SessionSpec};
use crate::term::Terminal;
use crate::theme::Theme;
use crate::world::{self, Live};
use crate::Res;

pub struct Opts {
    pub journal: Option<PathBuf>,
    pub cmd: Option<String>,
    pub cwd: Option<PathBuf>,
    pub out: PathBuf,
    pub size: (u32, u32),
    pub scale: f32,
    pub font_pt: f32,
    pub wait: f32,
    pub until: Option<f64>,
    pub reveal_ms: Option<f32>,
    pub hover: Option<(f32, f32)>,
    pub zoom: Option<(f64, f64)>,
    pub time: usize,
}

/// A session's file name without its extension: what the header calls it.
fn session_name(path: &std::path::Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Run a real session to its end (or to the deadline), feeding the view as a
/// window would. Returns the ledger, the view, and the frame-clock time of
/// the last event.
fn run_live(o: &Opts, cmd: &str) -> Res<(Ledger, Live, f64)> {
    let mut parts = cmd.split_whitespace().map(String::from);
    let exe = parts.next().ok_or("empty --cmd")?;
    let (cols, rows) = (120u16, 30u16);
    let started = Instant::now();
    let h = session::spawn_local(SessionSpec { exe, args: parts.collect(), cwd: o.cwd.clone(), cols, rows, journal: true, env: None })?;
    // A terminal answers the console host's opening questions, as a window's would.
    let term = Terminal::new(cols as usize, rows as usize, Theme::void_light(), Arc::new(|_| {}), Some(h.cmds.clone()));
    let name = h.journal_path.as_deref().map(session_name).unwrap_or_else(|| "not journaled".into());
    let mut live = Live::new(&name);
    let mut ledger = Ledger::new();
    let deadline = Instant::now() + Duration::from_secs_f32(o.wait);
    let (mut total, mut last) = (0u64, 0.0f64);
    let mut ended = false;
    loop {
        let ev = h.events.recv_deadline(deadline);
        let now = started.elapsed().as_secs_f64();
        match ev {
            Ok(SessionEvent::Output(b)) => {
                total += b.len() as u64;
                term.feed(&b);
                live.on_output(total, now_us());
                last = now;
            }
            Ok(SessionEvent::Proc(ev)) => {
                let id = ledger.apply(&ev);
                live.on_proc(&ledger, id, &ev, true, now);
                last = now;
            }
            Ok(SessionEvent::Exit(code)) => {
                live.on_exit(code, now_us());
                last = now;
                ended = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break, // still going at the deadline: shown as it stands
        }
    }
    let _ = h.cmds.send(SessionCmd::Kill);
    println!("session: {} processes, {} bytes printed, {}", ledger.total, total, if ended { "ended" } else { "still running at the deadline (now ended by this command)" });
    if let Some(p) = &h.journal_path {
        println!("journal: {}", p.display());
    }
    Ok((ledger, live, last))
}

/// p50 / p95 / max of a set of microsecond values.
fn spread(mut v: Vec<u64>) -> String {
    v.sort_unstable();
    let at = |p: f64| v.get(((v.len() as f64 - 1.0) * p).round().max(0.0) as usize).copied().unwrap_or(0);
    format!("p50 {} us, p95 {} us, max {} us (n={})", at(0.5), at(0.95), v.last().copied().unwrap_or(0), v.len())
}

pub fn run(o: &Opts) -> Res<()> {
    let theme = Theme::void_light();
    let (ledger, mut live, last) = match (&o.cmd, &o.journal) {
        (Some(c), _) => run_live(o, c)?,
        (None, Some(p)) => {
            let t = Instant::now();
            let scan = world::scan_journal(p, o.until.map(|s| (s * 1e6) as u64)).map_err(|e| format!("{}: {e}", p.display()))?;
            let read_us = t.elapsed().as_micros();
            let name = scan.meta["session"].as_str().map(String::from).unwrap_or_else(|| session_name(p));
            let live = Live::from_scan(&scan, &name);
            let last = scan.last_us.saturating_sub(scan.epoch_us) as f64 / 1e6;
            println!(
                "journal: read in {read_us} us, {} records{}, {} processes, {} bytes printed, clock {}",
                scan.records,
                o.until.map_or(String::new(), |s| format!(" in the first {s} s")),
                scan.ledger.total,
                scan.activity.total,
                if scan.epoch_exact { "from its process records" } else { "from its header (whole seconds; no process records)" }
            );
            (scan.ledger, live, last)
        }
        _ => return Err("--world needs --journal FILE or --cmd \"EXE ARGS\"".into()),
    };

    let gpu = gpu::init(gpu::instance(), None)?;
    let mut r = Renderer::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb, o.font_pt * o.scale, theme.clone());
    r.resize(&gpu.device, o.size.0, o.size.1, 12.0 * o.scale);
    let (size, pad, ty) = (r.screen, r.pad[0], r.scene_type());
    live.shown = true;
    if let Some((a, b)) = o.zoom {
        live.zoom_to(a, b);
    }
    // Settled, or caught while the latest arrivals are still lit.
    let at = last + o.reveal_ms.map_or(30.0, |ms| ms as f64 / 1000.0);
    let mut scene = live.scene(&ledger, ty, size, pad, &theme, at);
    if let Some((x, y)) = o.hover {
        if live.mouse(x, y) {
            scene = live.scene(&ledger, ty, size, pad, &theme, at);
        }
        println!("hover {x},{y}: {:?}", live.hover());
    }
    r.enter_scene();
    r.build_scene(&gpu.device, &gpu.queue, &scene);
    let earliest = scene.emits.iter().map(|e| e.born).chain(scene.texts.iter().filter_map(|t| t.born)).fold(f64::INFINITY, f64::min);
    if earliest.is_finite() {
        r.simulate_field(&gpu.device, &gpu.queue, earliest, at);
    }
    let (pw, ph, rgba) = r.capture(&gpu.device, &gpu.queue, at);
    crate::shot::write_png(&o.out, pw, ph, &rgba)?;

    let c = scene.counts;
    let (rects, glyphs) = r.instance_counts();
    let info = gpu.adapter.get_info();
    println!("{} {}x{} px, {:?} on {}", o.out.display(), pw, ph, info.backend, info.name);
    println!(
        "world: {} processes ({} running, {} failed) as {} nodes and {} groups in {} rows (detail level {}, {} folded, {} rows cut); {} spans; {} rectangles, {} glyphs",
        c.procs, c.running, c.failed, c.nodes, c.groups, c.rows, c.level, c.folded, c.rows_cut, c.spans, rects, glyphs
    );
    // The frame loop's own question, asked at each millisecond after the
    // latest event: is anything still moving?
    let still = (0..5000).map(|ms| last + ms as f64 / 1000.0).take_while(|&t| r.animating(t)).count();
    println!("frames: owed for {still} ms after the latest event, none after that (animating = {} six seconds on)", r.animating(last + 6.0));
    println!("scene build: {} us", live.build_us_max);
    if o.time > 0 {
        let mut us = Vec::with_capacity(o.time);
        for _ in 0..o.time {
            let t = Instant::now();
            live.dirty = true;
            let s = live.scene(&ledger, ty, size, pad, &theme, at);
            us.push(t.elapsed().as_micros() as u64);
            std::hint::black_box(s);
        }
        println!("scene build, {} more: {}", o.time, spread(us));
    }
    Ok(())
}
