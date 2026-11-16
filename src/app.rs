//! The window: input in, frames out. Frames are drawn only in response to an
//! event (new output, input, resize) or while an animation is still settling.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{viewport_to_point, TermMode};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::gpu::{self, Gpu};
use crate::input;
use crate::paths;
use crate::renderer::Renderer;
use crate::session::{self, SessionSpec};
use crate::sound::Sound;
use crate::synth::Event as Heard;
use crate::term::{Notify, Terminal, UiEvent};
use crate::theme::Theme;
use crate::Res;

/// Space between the window edge and the grid, in logical pixels.
const PAD: f32 = 12.0;

pub struct Options {
    exe: Option<String>,
    args: Vec<String>,
    /// The program was named with `-e` for this launch only.
    explicit: bool,
    cwd: Option<PathBuf>,
    font_pt: f32,
    journal: bool,
    reveal: bool,
    bench: bool,
    /// Run the session inside the window process instead of the daemon.
    local: bool,
    /// Watch one particular daemon session (one an agent drives, say)
    /// instead of starting or adopting one.
    attach: Option<u64>,
    /// Strength of the light arriving text casts (0 = off).
    light: f32,
    /// Loudness of event sounds, 0 to 1 (0 = off, and no audio device is ever opened).
    volume: f32,
    /// Command that opens a file link at its line (config `editor`).
    editor: String,
}

impl Options {
    /// Settings file first, then the command line on top of it.
    fn parse(args: &[String]) -> Self {
        let cfg = crate::config::Config::load();
        // At most once a day, on its own thread, never under test (update.rs).
        if !args.iter().any(|a| a == "--bench") {
            crate::update::background(cfg.update.clone());
        }
        let mut o = Options {
            exe: if cfg.shell.is_empty() { None } else { Some(cfg.shell.clone()) },
            args: cfg.shell_args.clone(),
            explicit: false,
            cwd: None,
            font_pt: cfg.font_pt,
            journal: true,
            reveal: cfg.reveal,
            bench: false,
            local: false,
            attach: None,
            light: cfg.light,
            volume: if cfg.sound { cfg.volume } else { 0.0 },
            editor: cfg.editor.clone(),
        };
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--cwd" => {
                    o.cwd = args.get(i + 1).map(PathBuf::from);
                    i += 1;
                }
                "--font" => {
                    o.font_pt = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(15.0);
                    i += 1;
                }
                // The daemon always journals, so no journal means no daemon.
                "--no-journal" => {
                    o.journal = false;
                    o.local = true;
                }
                "--no-reveal" => o.reveal = false,
                "--no-light" => o.light = 0.0,
                "--no-sound" => o.volume = 0.0,
                "--bench" => o.bench = true,
                "--local" => o.local = true,
                "--attach" => {
                    o.attach = args.get(i + 1).and_then(|s| s.parse().ok()).filter(|&id| id != 0);
                    i += 1;
                }
                "-e" => {
                    o.exe = args.get(i + 1).cloned();
                    o.args = args.iter().skip(i + 2).cloned().collect();
                    o.explicit = true;
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        // PROTEUS_SOUND=off or a volume (0 to 1) overrides, for tests.
        if let Ok(v) = std::env::var("PROTEUS_SOUND") {
            o.volume = v.parse::<f32>().ok().filter(|x| x.is_finite()).unwrap_or(0.0).clamp(0.0, 1.0);
        }
        // A benchmark measures; it does not perform.
        if o.bench {
            o.volume = 0.0;
        }
        o
    }
}

/// Live measurements, written to `stats.json` so performance claims are
/// numbers anyone can read rather than adjectives.
struct Stats {
    started: Instant,
    last_write: Instant,
    frames: u64,
    builds: u64,
    build_us_total: u64,
    build_us_max: u64,
    key_ms: Vec<f32>,
    backend: String,
    adapter: String,
    /// Pointer moves seen (links: moving over plain cells must draw nothing).
    moves: u64,
}

impl Stats {
    fn key_latency(&mut self, d: Duration) {
        if self.key_ms.len() >= 512 {
            self.key_ms.remove(0);
        }
        self.key_ms.push(d.as_secs_f32() * 1000.0);
    }

    fn write(&mut self, cols: usize, rows: usize) {
        self.last_write = Instant::now();
        let mut k = self.key_ms.clone();
        k.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f32| k.get(((k.len() as f32 - 1.0) * p).round().max(0.0) as usize).copied().unwrap_or(0.0);
        let v = serde_json::json!({
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_s": self.started.elapsed().as_secs_f32(),
            "frames": self.frames,
            "builds": self.builds,
            "build_us_avg": if self.builds > 0 { self.build_us_total / self.builds } else { 0 },
            "build_us_max": self.build_us_max,
            "key_to_present_ms": { "n": k.len(), "p50": pct(0.5), "p95": pct(0.95), "max": pct(1.0) },
            "backend": self.backend,
            "adapter": self.adapter,
            "grid": [cols, rows],
            "pointer_moves": self.moves,
        });
        // One file per window, so several windows do not overwrite each other.
        let name = format!("stats-{}.json", std::process::id());
        let _ = std::fs::write(paths::data_dir().join(name), serde_json::to_vec_pretty(&v).unwrap_or_default());
    }
}

/// `--bench`: a scripted run that types, then floods the terminal, and
/// writes what it measured to `bench.json`. Input goes down the same path as
/// real keystrokes, so the numbers are end to end.
struct Bench {
    phase: u8,
    t: Instant,
    keys: usize,
    file: PathBuf,
    file_bytes: u64,
    bytes0: u64,
    frames0: u64,
    builds0: u64,
    typing: serde_json::Value,
}

/// `session_home` of a window opened with `--attach`.
const WATCHING: &str = "daemon, watching";

const BENCH_TEXT: &[u8] = b"echo the quick brown fox jumps over the lazy dog 0123456789 proteus";

impl Bench {
    fn new() -> Self {
        // ~6 MB of coloured text, like a long build log.
        let file = std::env::temp_dir().join("proteus-bench.txt");
        let mut s = String::with_capacity(7 << 20);
        for i in 0..60_000 {
            s.push_str(&format!(
                "{i:06} \x1b[3{}mthe quick brown fox jumps over the lazy dog\x1b[0m 0123456789 abcdefghijklmnopqrstuvwxyz\r\n",
                1 + i % 6
            ));
        }
        let file_bytes = s.len() as u64;
        let _ = std::fs::write(&file, s);
        Self {
            phase: 0,
            t: Instant::now(),
            keys: 0,
            file,
            file_bytes,
            bytes0: 0,
            frames0: 0,
            builds0: 0,
            typing: serde_json::Value::Null,
        }
    }
}

fn percentiles(samples: &[f32]) -> serde_json::Value {
    let mut k = samples.to_vec();
    k.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |p: f32| k.get(((k.len() as f32 - 1.0) * p).round().max(0.0) as usize).copied().unwrap_or(0.0);
    serde_json::json!({ "n": k.len(), "p50": pct(0.5), "p95": pct(0.95), "max": pct(1.0) })
}

struct State {
    /// One display refresh. Frames are never drawn more often than this,
    /// except the frame that shows a keystroke's echo.
    interval: Duration,
    last_present: Instant,
    /// When the next paced frame is due, if one is.
    next_frame_at: Option<Instant>,
    /// Set once the window should go away: immediately (detach) or as soon
    /// as the session confirms it has ended (close).
    quit_at: Option<Instant>,
    /// Where the cursor was when the last key was sent. The echo is the
    /// frame in which it has moved; other output must not be mistaken for it.
    key_cursor: Option<alacritty_terminal::index::Point>,
    session_home: &'static str,
    bench: Option<Bench>,
    bytes: Arc<std::sync::atomic::AtomicU64>,
    last_wake: Instant,
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    gpu: Gpu,
    renderer: Renderer,
    terminal: Terminal,
    clipboard: Option<arboard::Clipboard>,
    mods: ModifiersState,
    /// Keys whose press went to the program and which have not come up yet.
    keys_down: Vec<winit::keyboard::PhysicalKey>,
    focused: bool,
    dirty: bool,
    mouse: (f32, f32),
    selecting: bool,
    held: Option<u8>,
    last_cell: (usize, usize),
    last_click: Option<(Instant, usize, usize)>,
    clicks: u8,
    wheel: f32,
    scale: f32,
    font_pt: f32,
    base_font_pt: f32,
    start: Instant,
    key_at: Option<Instant>,
    stats: Stats,
    sound: Sound,
    // Time travel (see the second `impl State` below, and scrub.rs).
    /// The session's journal, if this window can read it (time travel reads
    /// the past from it, the World view the times of replayed history).
    journal_path: Option<PathBuf>,
    notify: Notify,
    /// A trip into the past in progress; `None` is the live session.
    scrub: Option<crate::scrub::Scrub>,
    /// How many key repeats the current hold has reached.
    scrub_run: u32,
    scrub_drag: bool,
    /// A line shown in the bar's place until the given moment.
    notice: Option<(String, Instant)>,
    /// Every process the session has started, from the OS (`procs.rs`):
    /// the running set in full, and recent history. Drawn by the World view.
    procs: crate::procs::Ledger,
    /// The World view (`world.rs`), Ctrl+Shift+W; its hooks are together at
    /// the end of this file.
    world: crate::world::Live,
    /// The link under the pointer (`links.rs`), where plain `path:line`
    /// text is looked up when the shell has not said, and the editor
    /// command; wiring in the last `impl State` block.
    hover: crate::links::Hover,
    link_click: crate::links::Click,
    start_dir: PathBuf,
    editor: String,
}

impl State {
    fn new(window: Arc<Window>, opts: &Options, proxy: EventLoopProxy<UiEvent>) -> Res<Self> {
        let instance = gpu::instance();
        let surface = instance.create_surface(window.clone())?;
        let gpu = gpu::init(instance, Some(&surface))?;
        let caps = surface.get_capabilities(&gpu.adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        // Mailbox never blocks waiting for the display, so a keystroke's echo
        // can be drawn the moment it arrives; we pace frames ourselves.
        // `PROTEUS_PRESENT=vsync|immediate` overrides for comparison.
        let has = |m: wgpu::PresentMode| caps.present_modes.contains(&m);
        let present_mode = match std::env::var("PROTEUS_PRESENT").unwrap_or_default().as_str() {
            "vsync" => wgpu::PresentMode::AutoVsync,
            "immediate" if has(wgpu::PresentMode::Immediate) => wgpu::PresentMode::Immediate,
            _ if has(wgpu::PresentMode::Mailbox) => wgpu::PresentMode::Mailbox,
            _ => wgpu::PresentMode::AutoVsync,
        };
        let mhz = window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).unwrap_or(60_000).max(24_000);
        let interval = Duration::from_secs_f64(1000.0 / mhz as f64);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&gpu.device, &config);

        let scale = window.scale_factor() as f32;
        let theme = Theme::void_light();
        let mut renderer = Renderer::new(&gpu.device, format, opts.font_pt * scale, theme.clone());
        renderer.reveal_enabled = opts.reveal;
        renderer.light = opts.light;
        // Reduced motion: read now, and followed while the window lives (motion.rs).
        {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(RawWindowHandle::Win32(h)) = window.window_handle().map(|h| h.as_raw()) {
                if !crate::motion::watch(h.hwnd.get()) {
                    paths::log("motion: the window could not be watched for setting changes");
                }
            }
            renderer.set_still(crate::motion::still());
        }
        renderer.resize(&gpu.device, config.width, config.height, PAD * scale);
        let (cols, rows) = renderer.grid();

        let (exe, args) = match &opts.exe {
            Some(e) => (e.clone(), opts.args.clone()),
            // The benchmark skips the profile so runs are comparable.
            None if opts.bench => (session::default_shell().0, vec!["-NoLogo".into(), "-NoProfile".into()]),
            None => session::default_shell(),
        };
        if opts.bench {
            let tick = proxy.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(5));
                if tick.send_event(UiEvent::Tick).is_err() {
                    break;
                }
            });
        }
        let spec = SessionSpec {
            exe: exe.clone(),
            args,
            cwd: opts.cwd.clone(),
            cols: cols as u16,
            rows: rows as u16,
            journal: opts.journal,
            env: None,
        };
        // Normally the session lives in the daemon so it outlives this
        // window. The benchmark and `--local` keep it in-process.
        let (session, home) = if let Some(id) = opts.attach {
            // A session asked for by number that has gone is an error, not a
            // reason to start another one.
            let stream = crate::remote::connect(false)?;
            (crate::remote::open_with(stream, &spec, Some(id))?.0, WATCHING)
        } else if opts.local {
            (session::spawn_local(spec)?, "in-process")
        } else {
            // Only a plain launch may adopt a session that lost its window;
            // an explicit command or a benchmark always starts fresh.
            let reattach = !opts.explicit && !opts.bench;
            match crate::remote::connect(true).and_then(|s| crate::remote::open(s, &spec, reattach)) {
                Ok((h, true)) => (h, "daemon, reattached"),
                Ok((h, false)) => (h, "daemon"),
                Err(e) => {
                    paths::log(&format!("daemon unavailable ({e}); running the session in-process"));
                    (session::spawn_local(spec)?, "in-process")
                }
            }
        };
        let proxy = Mutex::new(proxy);
        let notify: Notify = Arc::new(move |e| {
            if let Ok(p) = proxy.lock() {
                let _ = p.send_event(e);
            }
        });
        let terminal = Terminal::new(cols, rows, theme, notify.clone(), Some(session.cmds.clone()));
        terminal.attach(session.events);

        let info = gpu.adapter.get_info();
        paths::log(&format!(
            "start pid={} shell={exe} session={home} grid={cols}x{rows} backend={:?} adapter={} present={:?} refresh={}mHz journal={:?}",
            std::process::id(),
            info.backend,
            info.name,
            present_mode,
            mhz,
            session.journal_path
        ));
        let world_name = session.journal_path.as_deref().and_then(|p| p.file_stem()).map_or(home.to_string(), |s| s.to_string_lossy().into_owned());
        let now = Instant::now();
        let mut st = Self {
            interval,
            last_present: now - interval,
            next_frame_at: None,
            quit_at: None,
            key_cursor: None,
            session_home: home,
            bench: if opts.bench { Some(Bench::new()) } else { None },
            bytes: session.bytes.clone(),
            last_wake: now,
            window,
            surface,
            config,
            gpu,
            renderer,
            terminal,
            clipboard: arboard::Clipboard::new().ok(),
            mods: ModifiersState::empty(),
            keys_down: Vec::new(),
            focused: true,
            dirty: true,
            mouse: (0.0, 0.0),
            selecting: false,
            held: None,
            last_cell: (0, 0),
            last_click: None,
            clicks: 0,
            wheel: 0.0,
            scale,
            font_pt: opts.font_pt,
            base_font_pt: opts.font_pt,
            start: now,
            key_at: None,
            stats: Stats {
                started: now,
                last_write: now,
                frames: 0,
                builds: 0,
                build_us_total: 0,
                build_us_max: 0,
                key_ms: Vec::new(),
                backend: format!("{:?} {:?}", info.backend, present_mode),
                adapter: info.name.clone(),
                moves: 0,
            },
            sound: Sound::start(opts.volume),
            journal_path: session.journal_path.clone(),
            notify,
            scrub: None,
            scrub_run: 0,
            scrub_drag: false,
            notice: None,
            procs: crate::procs::Ledger::bounded(20_000),
            world: crate::world::Live::new(&world_name),
            hover: crate::links::Hover::default(),
            link_click: crate::links::Click::default(),
            start_dir: crate::links::start_dir(session.journal_path.as_deref(), opts.cwd.as_deref()),
            editor: opts.editor.clone(),
        };
        st.sound.warm();
        st.publish_metrics();
        Ok(st)
    }

    fn mode(&self) -> TermMode {
        self.terminal.term.lock().map(|t| *t.mode()).unwrap_or(TermMode::NONE)
    }

    fn publish_metrics(&mut self) {
        use std::sync::atomic::Ordering;
        let m = self.renderer.fonts.metrics;
        self.terminal.shared.cell_w.store(m.cell_w as u32, Ordering::Relaxed);
        self.terminal.shared.cell_h.store(m.cell_h as u32, Ordering::Relaxed);
    }

    /// Fit the grid to the window and tell the program its new size.
    fn regrid(&mut self) {
        let (cols, rows) = self.renderer.grid();
        self.terminal.resize(cols, rows);
        self.publish_metrics();
        self.scrub_refit();
        // The text under the pointer has moved; the next move looks again.
        self.link_unhover();
        self.dirty = true;
        self.window.request_redraw();
    }

    fn cell(&self) -> (usize, usize, bool) {
        self.renderer.cell_at(self.mouse.0, self.mouse.1, self.terminal.cols, self.terminal.rows)
    }

    /// A program asked to write the clipboard (OSC 52). Any printed text can
    /// ask (a fetched page, a hostile git log), so only a window the person is
    /// looking at may, and only a sensible amount; each write is logged (audit T1).
    fn program_copy(&mut self, text: String) {
        if !self.focused || text.len() > 8 << 10 {
            paths::log(&format!("clipboard: refused a program's copy of {} bytes (focused: {})", text.len(), self.focused));
            return;
        }
        paths::log(&format!("clipboard: a program copied {} characters", text.chars().count()));
        self.set_clipboard(text);
    }

    fn set_clipboard(&mut self, text: String) {
        if let Some(c) = self.clipboard.as_mut() {
            let _ = c.set_text(text);
        }
    }

    fn copy_selection(&mut self) -> bool {
        let text = self.terminal.term.lock().ok().and_then(|t| t.selection_to_string());
        match text {
            Some(t) if !t.is_empty() => {
                self.set_clipboard(t);
                true
            }
            _ => false,
        }
    }

    fn clear_selection(&mut self) {
        if let Ok(mut t) = self.terminal.term.lock() {
            if t.selection.take().is_some() {
                self.dirty = true;
            }
        }
    }

    fn paste(&mut self) {
        let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) else { return };
        if text.is_empty() {
            return;
        }
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        // A program that does not mark pastes (cmd.exe) runs a pasted line the
        // moment its newline arrives; a paste never presses Enter by itself (audit T1).
        let text = if bracketed { text } else { text.trim_end_matches(['\r', '\n']).to_string() };
        if text.is_empty() {
            return;
        }
        self.send(input::encode_paste(&text, bracketed));
    }

    /// Send input to the program, snapping back to live output first.
    fn send(&mut self, bytes: Vec<u8>) {
        self.leave_scrub();
        if let Ok(mut t) = self.terminal.term.lock() {
            if t.grid().display_offset() != 0 {
                t.scroll_display(Scroll::Bottom);
                self.dirty = true;
            }
        }
        self.terminal.input(bytes);
        if self.dirty {
            self.window.request_redraw();
        }
    }

    /// Ask for a paced frame no later than `at`.
    fn frame_by(&mut self, at: Instant) {
        self.next_frame_at = Some(self.next_frame_at.map_or(at, |t| t.min(at)));
    }

    /// A key went to the program. Its echo will trigger the frame; drawing
    /// one now would only make the echo wait behind it. The fallback frame
    /// covers keys that produce no echo, so the cursor still responds.
    fn key_sent(&mut self) {
        self.key_at = Some(Instant::now());
        self.key_cursor = self.terminal.term.lock().ok().map(|t| t.grid().cursor.point);
        self.renderer.pulse();
        self.frame_by(Instant::now() + Duration::from_millis(12));
    }

    fn scroll(&mut self, s: Scroll) {
        // In the past, it is the past screen's own scrollback that scrolls.
        if let Some(past) = self.scrub.as_mut() {
            if let Some(v) = past.view.as_mut() {
                v.term.scroll_display(s);
            }
        } else if let Ok(mut t) = self.terminal.term.lock() {
            t.scroll_display(s);
        }
        self.dirty = true;
        self.window.request_redraw();
    }

    fn set_font(&mut self, pt: f32) {
        self.font_pt = pt.clamp(6.0, 72.0);
        self.renderer.set_font_px(&self.gpu.queue, self.font_pt * self.scale);
        self.regrid();
    }

    /// Keys coming up, and modifiers pressed by themselves. They matter only
    /// to a program that asked to hear about them (kitty keyboard protocol),
    /// and they are not typing: nothing flashes, sounds or scrolls.
    fn on_quiet_key(&mut self, ev: &KeyEvent) {
        if ev.state == ElementState::Released && !input::is_modifier(ev) {
            // Only a key whose press the program saw is reported as released
            // (not one a terminal shortcut consumed).
            let Some(i) = self.keys_down.iter().position(|k| *k == ev.physical_key) else { return };
            self.keys_down.swap_remove(i);
        }
        if let Some(bytes) = input::encode_key(ev, self.mods, self.mode()) {
            self.terminal.input(bytes);
        }
    }

    fn on_key(&mut self, ev: KeyEvent) {
        if ev.state != ElementState::Pressed || input::is_modifier(&ev) {
            return self.on_quiet_key(&ev);
        }
        let (ctrl, shift) = (self.mods.control_key(), self.mods.shift_key());
        let ch = match &ev.logical_key {
            Key::Character(s) => s.to_lowercase(),
            _ => String::new(),
        };
        // The World view: Ctrl+Shift+W shows it, and while it is shown keys
        // are its own and do not reach the program.
        if self.world_key(&ev, ctrl && shift && ch == "w") {
            return;
        }
        // Terminal-level shortcuts come first.
        if self.scrub_key(&ev, ctrl, shift) {
            return;
        }
        if ctrl && shift && ch == "d" {
            // Detach: the window goes, the session stays in the daemon and
            // the next window picks it up.
            self.quit_at = Some(Instant::now());
            return;
        }
        if ctrl && shift && ch == "c" {
            self.copy_selection();
            return;
        }
        if ctrl && shift && ev.logical_key == Key::Named(NamedKey::Delete) {
            // A program that died with the kitty keyboard protocol switched
            // on never switched it off: back to plain keys.
            self.terminal.feed(input::DROP_KEYBOARD_FLAGS);
            return;
        }
        if (ctrl && shift && ch == "v") || (shift && ev.logical_key == Key::Named(NamedKey::Insert)) {
            self.paste();
            return;
        }
        if ctrl && !shift && ch == "c" && self.copy_selection() {
            self.clear_selection();
            self.window.request_redraw();
            return;
        }
        if shift && ev.logical_key == Key::Named(NamedKey::PageUp) {
            return self.scroll(Scroll::PageUp);
        }
        if shift && ev.logical_key == Key::Named(NamedKey::PageDown) {
            return self.scroll(Scroll::PageDown);
        }
        if ctrl && (ch == "=" || ch == "+") {
            return self.set_font(self.font_pt + 1.0);
        }
        if ctrl && !shift && ch == "-" {
            return self.set_font(self.font_pt - 1.0);
        }
        if ctrl && ch == "0" {
            return self.set_font(self.base_font_pt);
        }

        if let Some(bytes) = input::encode_key(&ev, self.mods, self.mode()) {
            if !self.keys_down.contains(&ev.physical_key) {
                self.keys_down.push(ev.physical_key);
            }
            self.clear_selection();
            self.send(bytes);
            self.key_sent();
            self.sound.play(Heard::Key);
        }
    }

    fn on_mouse_button(&mut self, button: MouseButton, state: ElementState) {
        if self.world.shown {
            return; // clicks are not the program's while the World view is up
        }
        if self.scrub.is_some() {
            // The past is looked at, not clicked into: the only thing the
            // mouse does here is drag the playhead.
            self.scrub_drag = button == MouseButton::Left && state == ElementState::Pressed && self.scrub_to_mouse();
            return;
        }
        let pressed = state == ElementState::Pressed;
        // Ctrl+click on a link opens it; its press and release go nowhere else.
        if button == MouseButton::Left {
            let (mut click, ctrl) = (std::mem::take(&mut self.link_click), self.mods.control_key());
            let taken = click.route(pressed, ctrl, || self.link_open());
            self.link_click = click;
            if taken {
                return;
            }
        }
        let mode = self.mode();
        let (col, row, right) = self.cell();
        // Programs that asked for the mouse get it; Shift overrides for selection.
        if mode.intersects(TermMode::MOUSE_MODE) && !self.mods.shift_key() {
            let b = match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                _ => return,
            };
            self.held = if pressed { Some(b) } else { None };
            if let Some(bytes) = input::encode_mouse(b, col, row, pressed, mode.contains(TermMode::SGR_MOUSE)) {
                self.terminal.input(bytes);
            }
            return;
        }
        match (button, pressed) {
            (MouseButton::Left, true) => {
                let now = Instant::now();
                let again = self
                    .last_click
                    .is_some_and(|(t, c, r)| now.duration_since(t) < Duration::from_millis(400) && c == col && r == row);
                self.clicks = if again { self.clicks % 3 + 1 } else { 1 };
                self.last_click = Some((now, col, row));
                let ty = match self.clicks {
                    1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                if let Ok(mut t) = self.terminal.term.lock() {
                    let point = viewport_to_point(t.grid().display_offset(), Point::new(row, Column(col)));
                    t.selection = Some(Selection::new(ty, point, if right { Side::Right } else { Side::Left }));
                }
                self.selecting = true;
                self.dirty = true;
                self.window.request_redraw();
            }
            (MouseButton::Left, false) => {
                if self.selecting {
                    self.selecting = false;
                    self.copy_selection();
                }
            }
            (MouseButton::Right, true) | (MouseButton::Middle, true) => self.paste(),
            _ => {}
        }
    }

    fn on_mouse_move(&mut self, x: f32, y: f32) {
        self.mouse = (x, y);
        self.stats.moves += 1;
        if self.stats.last_write.elapsed() > Duration::from_secs(2) {
            self.stats.write(self.terminal.cols, self.terminal.rows);
        }
        self.link_hover(false);
        if self.world_mouse(x, y) {
            return;
        }
        if self.scrub.is_some() {
            if self.scrub_drag {
                self.scrub_to_mouse();
            }
            return;
        }
        let (col, row, right) = self.cell();
        if self.selecting {
            if let Ok(mut t) = self.terminal.term.lock() {
                let point = viewport_to_point(t.grid().display_offset(), Point::new(row, Column(col)));
                if let Some(sel) = t.selection.as_mut() {
                    sel.update(point, if right { Side::Right } else { Side::Left });
                }
            }
            self.dirty = true;
            self.window.request_redraw();
        } else if let Some(b) = self.held {
            let mode = self.mode();
            if (col, row) != self.last_cell && mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
                if let Some(bytes) = input::encode_mouse(b + 32, col, row, true, mode.contains(TermMode::SGR_MOUSE)) {
                    self.terminal.input(bytes);
                }
            }
        }
        self.last_cell = (col, row);
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        if self.world_wheel(&delta) {
            return;
        }
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => y * 3.0,
            MouseScrollDelta::PixelDelta(p) => p.y as f32 / self.renderer.fonts.metrics.cell_h,
        };
        self.wheel += lines;
        let whole = self.wheel.trunc() as i32;
        if whole == 0 {
            return;
        }
        self.wheel -= whole as f32;
        if self.scrub.is_some() {
            // While in the past the wheel moves through time, not through the program.
            return self.scrub_step(-(whole as i64));
        }
        let mode = self.mode();
        let (col, row, _) = self.cell();
        if mode.intersects(TermMode::MOUSE_MODE) && !self.mods.shift_key() {
            let b = if whole > 0 { 64 } else { 65 };
            for _ in 0..whole.unsigned_abs() {
                if let Some(bytes) = input::encode_mouse(b, col, row, true, mode.contains(TermMode::SGR_MOUSE)) {
                    self.terminal.input(bytes);
                }
            }
        } else if mode.contains(TermMode::ALT_SCREEN) {
            // Full-screen programs have no scrollback: turn the wheel into arrow keys.
            let key: &[u8] = match (whole > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let mut bytes = Vec::new();
            for _ in 0..whole.unsigned_abs() {
                bytes.extend_from_slice(key);
            }
            self.terminal.input(bytes);
        } else {
            self.scroll(Scroll::Delta(whole));
        }
    }

    /// Advance the benchmark script. Returns true when it has finished.
    fn bench_step(&mut self) -> bool {
        use std::sync::atomic::Ordering;
        let Some(mut b) = self.bench.take() else { return false };
        let now = Instant::now();
        let quiet = now.duration_since(self.last_wake);
        let mut done = false;
        match b.phase {
            // Wait for the shell prompt to settle.
            0 => {
                if b.t.elapsed() > Duration::from_millis(2500) && quiet > Duration::from_millis(800) {
                    self.stats.key_ms.clear();
                    b.phase = 1;
                    b.t = now;
                } else if b.t.elapsed() > Duration::from_secs(30) {
                    done = true;
                }
            }
            // Type one character at a time, each after the last was drawn.
            1 => {
                let waited = b.t.elapsed();
                if b.keys < BENCH_TEXT.len() {
                    if waited > Duration::from_millis(45) && (self.key_at.is_none() || waited > Duration::from_millis(400)) {
                        self.terminal.input(vec![BENCH_TEXT[b.keys]]);
                        self.key_sent();
                        b.keys += 1;
                        b.t = now;
                    }
                } else if waited > Duration::from_millis(500) {
                    b.typing = percentiles(&self.stats.key_ms);
                    self.terminal.input(vec![0x1b]); // clear the typed line
                    b.phase = 2;
                    b.t = now;
                }
            }
            // Flood: print the file and time it.
            2 => {
                if b.t.elapsed() > Duration::from_millis(500) {
                    let cmd = format!("cmd /c type \"{}\"\r", b.file.display());
                    b.bytes0 = self.bytes.load(Ordering::Relaxed);
                    b.frames0 = self.stats.frames;
                    b.builds0 = self.stats.builds;
                    self.terminal.input(cmd.into_bytes());
                    b.phase = 3;
                    b.t = now;
                }
            }
            _ => {
                let got = self.bytes.load(Ordering::Relaxed) - b.bytes0;
                let finished = got >= b.file_bytes && quiet > Duration::from_millis(600);
                if finished || b.t.elapsed() > Duration::from_secs(180) {
                    let secs = self.last_wake.duration_since(b.t).as_secs_f64().max(0.001);
                    let frames = self.stats.frames - b.frames0;
                    let v = serde_json::json!({
                        "version": env!("CARGO_PKG_VERSION"),
                        "session": self.session_home,
                        "backend": self.stats.backend,
                        "adapter": self.stats.adapter,
                        "grid": [self.terminal.cols, self.terminal.rows],
                        "typing_key_to_present_ms": b.typing,
                        "flood": {
                            "completed": finished,
                            "bytes": got,
                            "seconds": secs,
                            "mb_per_s": got as f64 / 1e6 / secs,
                            "frames": frames,
                            "fps": frames as f64 / secs,
                            "grid_builds": self.stats.builds - b.builds0,
                        },
                        "grid_build_us": { "avg": if self.stats.builds > 0 { self.stats.build_us_total / self.stats.builds } else { 0 }, "max": self.stats.build_us_max },
                    });
                    let _ = std::fs::write(paths::data_dir().join("bench.json"), serde_json::to_vec_pretty(&v).unwrap_or_default());
                    let _ = std::fs::remove_file(&b.file);
                    done = true;
                }
            }
        }
        if !done {
            self.bench = Some(b);
        }
        done
    }

    fn redraw(&mut self) -> Res<()> {
        // A key that produced no echo stops counting as pending, so later
        // unrelated output is neither rushed nor logged as its latency.
        if self.key_at.is_some_and(|k| k.elapsed() > Duration::from_millis(250)) {
            self.key_at = None;
        }
        // Pace to the display, but let a keystroke's echo through at once.
        let since = self.last_present.elapsed();
        let urgent = self.key_at.is_some() && self.terminal.has_wake() && since > Duration::from_millis(2);
        if since + Duration::from_micros(400) < self.interval && !urgent {
            self.frame_by(self.last_present + self.interval);
            return Ok(());
        }
        self.next_frame_at = None;
        // Animations switched off or on in Windows since the last frame.
        if crate::motion::still() != self.renderer.still {
            self.renderer.set_still(crate::motion::still());
            self.dirty = true;
        }
        let now = self.start.elapsed().as_secs_f64();
        // While the past is on screen the live grid is left alone: its
        // pending output stays pending and is drawn on the way back.
        let scrubbed = self.scrub_frame(now);
        let woke = !scrubbed && self.terminal.take_wake();
        // The screen changed under a pointer resting on a link: look again.
        if (woke || self.dirty) && self.hover.link.is_some() {
            self.link_hover(true);
        }
        if woke {
            self.last_wake = Instant::now();
        }
        let mut echoed = false;
        let grid_built = (woke || self.dirty) && !scrubbed;
        if grid_built {
            let t0 = Instant::now();
            if let Ok(term) = self.terminal.term.lock() {
                self.renderer.build(&self.gpu.device, &self.gpu.queue, &term, now, self.focused);
                echoed = self.key_cursor.is_some_and(|p| p != term.grid().cursor.point);
            }
            // Text that arrived is heard; what the person typed already was
            // (the key itself), and changes in place are not arrivals at all.
            let a = self.renderer.arrivals;
            if a.cells > a.typed {
                self.sound.play(Heard::Text { cells: a.cells - a.typed, red: a.red, green: a.green, bulk: a.bulk, pan: a.pan });
            }
            let us = t0.elapsed().as_micros() as u64;
            self.stats.builds += 1;
            self.stats.build_us_total += us;
            self.stats.build_us_max = self.stats.build_us_max.max(us);
            self.dirty = false;
        }
        self.world_draw(woke, grid_built, now);
        match self.surface.get_current_texture() {
            Ok(frame) => {
                let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.renderer.render(&self.gpu.device, &self.gpu.queue, &view, now);
                frame.present();
                self.last_present = Instant::now();
                self.stats.frames += 1;
                if echoed {
                    self.key_cursor = None;
                    if let Some(k) = self.key_at.take() {
                        self.stats.key_latency(k.elapsed());
                    }
                }
            }
            Err(wgpu::SurfaceError::Lost) | Err(wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.gpu.device, &self.config);
                self.dirty = true;
                self.window.request_redraw();
            }
            Err(wgpu::SurfaceError::Timeout) => self.window.request_redraw(),
            Err(e) => return Err(format!("surface: {e}").into()),
        }
        if self.renderer.animating(self.start.elapsed().as_secs_f64()) {
            self.frame_by(self.last_present + self.interval);
        }
        if self.stats.last_write.elapsed() > Duration::from_secs(2) {
            self.stats.write(self.terminal.cols, self.terminal.rows);
        }
        Ok(())
    }
}

/// Time travel: Ctrl+Shift+Left and Right step through the session's past by
/// output record (holding accelerates), Ctrl+Shift+Home and End jump to its
/// start and back to now, the wheel steps too, and the bar can be dragged.
/// Typing or Esc returns to the live session. Nothing here is ever sent to
/// the program, and the live session keeps running underneath.
impl State {
    /// The grid the past screen gets: the window's, less the bar.
    fn scrub_view(&self) -> (usize, usize) {
        let (cols, rows) = self.renderer.grid();
        (cols, rows.saturating_sub(crate::scrub::BAR_ROWS).max(1))
    }

    fn say(&mut self, words: String) {
        self.notice = Some((words, Instant::now() + Duration::from_millis(2600)));
        self.dirty = true;
        self.window.request_redraw();
    }

    /// Handle a key if it is one of the time-travel keys.
    fn scrub_key(&mut self, ev: &KeyEvent, ctrl: bool, shift: bool) -> bool {
        let named = match &ev.logical_key {
            Key::Named(n) => Some(*n),
            _ => None,
        };
        let way = match named {
            Some(NamedKey::ArrowLeft) if ctrl && shift => -1,
            Some(NamedKey::ArrowRight) if ctrl && shift => 1,
            Some(NamedKey::Home) if ctrl && shift => i64::MIN,
            Some(NamedKey::End) if ctrl && shift => i64::MAX,
            Some(NamedKey::Escape) if self.scrub.is_some() => i64::MAX,
            _ => return false,
        };
        // Held down, a key steps 1, 1, 1... then 2, 4, 8 records at a time.
        self.scrub_run = if ev.repeat { self.scrub_run + 1 } else { 0 };
        let stride = 1i64 << (self.scrub_run / 8).min(14);
        self.scrub_step(if way.unsigned_abs() == 1 { way * stride } else { way });
        true
    }

    /// Move through time by `n` output records: `i64::MIN` is the start of
    /// the session, `i64::MAX` is now.
    fn scrub_step(&mut self, n: i64) {
        if n == i64::MAX {
            return self.leave_scrub();
        }
        let entering = self.scrub.is_none();
        if entering {
            if n > 0 {
                return; // already at the present
            }
            let opened = match &self.journal_path {
                Some(p) => crate::scrub::Scrub::open(p, self.notify.clone()).map_err(|e| format!("its journal cannot be read ({e})")),
                None => Err("it has no journal this window can read".to_string()),
            };
            match opened {
                Ok(s) => self.scrub = Some(s),
                Err(why) => return self.say(format!("time travel is unavailable for this session: {why}")),
            }
        }
        let view = self.scrub_view();
        let Some(s) = self.scrub.as_mut() else { return };
        let _ = s.tl.refresh(&s.journal);
        match s.tl.step(s.pos, n.max(-(1 << 40))) {
            Some(pos) => {
                s.go(pos, view);
                self.dirty = true;
                self.window.request_redraw();
            }
            None if entering => {
                // Nothing behind the present yet: there is no past to enter.
                self.scrub = None;
                self.say("this session has no past yet".to_string());
            }
            // Stepped forward onto the newest output: that is the present.
            None => self.leave_scrub(),
        }
    }

    /// Put the playhead where the mouse is, if the mouse is on the bar.
    fn scrub_to_mouse(&mut self) -> bool {
        let (m, pad, screen) = (self.renderer.fonts.metrics, self.renderer.pad, self.renderer.screen);
        let on_bar = self.mouse.1 >= crate::scrub::band_top(pad[1], m.cell_h, self.renderer.grid().1);
        let view = self.scrub_view();
        let Some(s) = self.scrub.as_mut().filter(|_| on_bar || self.scrub_drag) else { return false };
        let t = crate::scrub::time_at_x(self.mouse.0, screen[0], pad[0], m.cell_w, s.tl.duration_us());
        // The last output record at or before that moment (never the present itself).
        let at = s.tl.outputs_at(s.tl.pos_at(t)).clamp(1, s.tl.outs.len().saturating_sub(1).max(1));
        let pos = s.tl.outs.get(at - 1).map_or(0, |&p| p as usize);
        if pos != s.pos {
            s.go(pos, view);
            self.dirty = true;
            self.window.request_redraw();
        }
        true
    }

    /// The window changed size: the past screen is fitted again.
    fn scrub_refit(&mut self) {
        let view = self.scrub_view();
        if let Some(s) = self.scrub.as_mut() {
            s.go(s.pos, view);
        }
    }

    /// Back to the live session.
    fn leave_scrub(&mut self) {
        if let Some(s) = self.scrub.take() {
            paths::log(&s.summary());
            self.scrub_drag = false;
            self.renderer.set_overlay(&self.gpu.device, &self.gpu.queue, None);
            // What the session printed meanwhile simply is there; it did not just arrive.
            self.renderer.suppress_reveal = true;
            self.dirty = true;
            self.window.request_redraw();
        }
    }

    /// Draw the past, if that is where the window is. Returns whether it is.
    fn scrub_frame(&mut self, now: f64) -> bool {
        let (device, queue) = (&self.gpu.device, &self.gpu.queue);
        let (_, rows) = self.renderer.grid();
        let Some(s) = self.scrub.as_mut() else {
            // Not in the past: at most a message in the bar's place.
            match &self.notice {
                Some((_, until)) if Instant::now() >= *until => {
                    self.notice = None;
                    self.renderer.set_overlay(device, queue, None);
                }
                Some((words, until)) => {
                    let r = &self.renderer;
                    let o = crate::scrub::notice(r.screen, r.pad, &r.fonts.metrics, &r.theme, rows, words);
                    let wiped = self.renderer.set_overlay(device, queue, Some(&o));
                    self.dirty |= wiped;
                    self.next_frame_at = Some(self.next_frame_at.map_or(*until, |t| t.min(*until)));
                }
                None => {}
            }
            return false;
        };
        if s.poll() || self.dirty {
            match &s.view {
                Some(v) => {
                    self.renderer.suppress_reveal = true;
                    self.renderer.build(device, queue, &v.term, now, self.focused);
                }
                // The first past screen is still on its way: the live one stays under the bar.
                None => {
                    if let Ok(t) = self.terminal.term.lock() {
                        self.renderer.suppress_reveal = true;
                        self.renderer.build(device, queue, &t, now, self.focused);
                    }
                }
            }
            let r = &self.renderer;
            let o = s.overlay(r.screen, r.pad, &r.fonts.metrics, &r.theme, rows);
            // If the bar's glyphs pushed others out of the atlas, the grid is built again next frame.
            self.dirty = self.renderer.set_overlay(device, queue, Some(&o));
            if self.dirty {
                self.window.request_redraw();
            }
        }
        true
    }
}

/// The World view (`world.rs`) in the window. What the view is, and how it is
/// laid out, is all in that file; this is only the wiring.
impl State {
    fn world_toggle(&mut self) {
        if self.world.toggle() {
            // The view is of the live session and takes the whole window:
            // a trip into the past ends here, and its bar (or the line shown
            // in the bar's place) goes, since both are drawn over everything.
            self.leave_scrub();
            self.notice = None;
            self.renderer.set_overlay(&self.gpu.device, &self.gpu.queue, None);
            // History that was replayed to this window gets its times from the journal.
            self.world.load_history(self.journal_path.as_deref());
            self.renderer.enter_scene();
        } else {
            self.renderer.leave_scene();
            self.dirty = true;
        }
        self.window.request_redraw();
    }

    /// True when the key was the view's and must not reach the program:
    /// the toggle itself, and every key while the view is shown.
    fn world_key(&mut self, ev: &KeyEvent, toggle: bool) -> bool {
        use crate::world::Key as K;
        // A held key repeats: the press that showed or hid the view does it
        // once, and its repeats go nowhere (not into the program either).
        if self.world.repeat_of_toggle(ev.repeat) {
            return true;
        }
        if toggle {
            self.world_toggle();
            return true;
        }
        if !self.world.shown {
            return false;
        }
        let key = match &ev.logical_key {
            Key::Named(NamedKey::Escape) => Some(K::Close),
            Key::Named(NamedKey::ArrowLeft) => Some(K::Left),
            Key::Named(NamedKey::ArrowRight) => Some(K::Right),
            Key::Named(NamedKey::ArrowUp) => Some(K::In),
            Key::Named(NamedKey::ArrowDown) => Some(K::Out),
            Key::Named(NamedKey::Home) => Some(K::Fit),
            Key::Named(NamedKey::End) => Some(K::Latest),
            Key::Character(s) => match s.as_str() {
                "+" | "=" => Some(K::In),
                "-" => Some(K::Out),
                "0" => Some(K::Fit),
                _ => None,
            },
            _ => None,
        };
        match key {
            Some(K::Close) => self.world_toggle(),
            Some(k) => {
                if self.world.key(k) {
                    self.window.request_redraw();
                }
            }
            None => {}
        }
        true
    }

    /// True while the view is shown. A frame is asked for only when the
    /// mouse is now over something else.
    fn world_mouse(&mut self, x: f32, y: f32) -> bool {
        if self.world.shown && self.world.mouse(x, y) {
            self.window.request_redraw();
        }
        self.world.shown
    }

    /// The wheel zooms the timeline about the mouse.
    fn world_wheel(&mut self, delta: &MouseScrollDelta) -> bool {
        if !self.world.shown {
            return false;
        }
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => *y,
            MouseScrollDelta::PixelDelta(p) => p.y as f32 / (3.0 * self.renderer.fonts.metrics.cell_h),
        };
        if self.world.wheel(lines, self.mouse.0) {
            self.window.request_redraw();
        }
        true
    }

    /// Called every frame, after the grid's own build. Output is placed on
    /// the timeline as it arrives whether or not the view is up; the scene is
    /// built only when the view is up and something it shows has changed (or
    /// the grid has just reused the instance buffers).
    fn world_draw(&mut self, woke: bool, grid_built: bool, now: f64) {
        if woke {
            self.world.on_output(self.bytes.load(std::sync::atomic::Ordering::Relaxed), crate::procs_win::now_us());
        }
        if !self.world.shown || !(grid_built || self.world.dirty) {
            return;
        }
        let ty = self.renderer.scene_type();
        let scene = self.world.scene(&self.procs, ty, self.renderer.screen, self.renderer.pad[0], &self.renderer.theme, now);
        self.renderer.build_scene(&self.gpu.device, &self.gpu.queue, &scene);
    }
}

/// Links (`links.rs`) in the window: only the wiring. A frame is asked for
/// only when the link under the pointer changes.
impl State {
    /// Where plain `path:line` text is looked up: the directory the shell
    /// last reported, else where the session started.
    fn link_dir(&self) -> PathBuf {
        let said = self.terminal.cwd.lock().ok().and_then(|c| c.clone()).map(PathBuf::from);
        said.filter(|p| p.is_absolute()).unwrap_or_else(|| self.start_dir.clone())
    }

    /// The viewport cell under the pointer, or None when it is in the margin.
    fn pointer_cell(&self) -> Option<(usize, usize)> {
        let (m, pad) = (self.renderer.fonts.metrics, self.renderer.pad);
        let (x, y) = ((self.mouse.0 - pad[0]) / m.cell_w, (self.mouse.1 - pad[1]) / m.cell_h);
        (x >= 0.0 && y >= 0.0 && (x as usize) < self.terminal.cols && (y as usize) < self.terminal.rows).then_some((y as usize, x as usize))
    }

    /// The pointer moved (`again` false), or the screen changed under it
    /// during a frame (`again` true).
    fn link_hover(&mut self, again: bool) {
        if self.world.shown || self.scrub.is_some() {
            return self.link_unhover();
        }
        if self.selecting {
            return;
        }
        let (cell, dir, term) = (self.pointer_cell(), self.link_dir(), self.terminal.term.clone());
        let changed = match term.lock() {
            Ok(t) if again => self.hover.recheck(&t, &dir),
            Ok(t) => self.hover.point(&t, cell, &dir),
            Err(_) => false,
        };
        if changed {
            self.link_show(!again);
        }
    }

    fn link_unhover(&mut self) {
        if self.hover.clear() {
            self.link_show(true);
        }
    }

    /// Draw what is hovered now: underline, strip and pointer shape.
    fn link_show(&mut self, ask_frame: bool) {
        let strip = self.hover.link.as_ref().map(|l| {
            let (target, hint, warn) = crate::links::describe(l);
            crate::renderer::Strip { target, hint, warn, pointer_x: self.mouse.0 }
        });
        let on = strip.is_some();
        self.renderer.hover = self.hover.link.as_ref().map(|l| l.spans.clone()).unwrap_or_default();
        self.renderer.set_strip(&self.gpu.device, &self.gpu.queue, strip);
        self.window.set_cursor(if on { winit::window::CursorIcon::Pointer } else { winit::window::CursorIcon::Default });
        self.dirty = true;
        if ask_frame {
            self.window.request_redraw();
        }
    }

    /// Ctrl+click: open the link under the pointer. True if there was one.
    fn link_open(&mut self) -> bool {
        self.link_hover(false);
        let Some(l) = self.hover.link.as_ref() else { return false };
        match crate::links::action(&l.target) {
            crate::links::Action::Refuse(why) => self.say(format!("link not opened: {why}")),
            a => crate::links::open(a, self.editor.clone()),
        }
        true
    }
}

struct App {
    proxy: EventLoopProxy<UiEvent>,
    opts: Options,
    state: Option<State>,
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        use winit::platform::windows::WindowAttributesExtWindows;
        let icon = |n: u32| winit::window::Icon::from_rgba(crate::icon::rgba(n), n, n).ok();
        let attrs = Window::default_attributes()
            .with_title("PROTEUS")
            .with_inner_size(LogicalSize::new(1180.0, 760.0))
            .with_min_inner_size(LogicalSize::new(320.0, 200.0))
            .with_theme(Some(winit::window::Theme::Dark))
            // Test runs set this so their windows never take the keyboard
            // from whoever is typing.
            .with_active(std::env::var_os("PROTEUS_NO_FOCUS").is_none())
            .with_window_icon(icon(32))
            .with_taskbar_icon(icon(256));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                paths::log(&format!("window: {e}"));
                return el.exit();
            }
        };
        window.set_ime_allowed(true);
        match State::new(window, &self.opts, self.proxy.clone()) {
            Ok(s) => self.state = Some(s),
            Err(e) => {
                paths::log(&format!("startup: {e}"));
                el.exit();
            }
        }
    }

    /// Sleep until the next paced frame is due, or indefinitely if none is.
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(st) = self.state.as_mut() else { return };
        if let Some(q) = st.quit_at {
            if Instant::now() >= q {
                st.stats.write(st.terminal.cols, st.terminal.rows);
                return el.exit();
            }
            let wake = st.next_frame_at.map_or(q, |t| t.min(q));
            return el.set_control_flow(ControlFlow::WaitUntil(wake));
        }
        match st.next_frame_at {
            Some(t) if Instant::now() >= t => {
                st.next_frame_at = None;
                st.window.request_redraw();
                el.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UiEvent) {
        let Some(st) = self.state.as_mut() else { return };
        match ev {
            UiEvent::Wake => {
                // The moment output arrived is when it goes on the World view's timeline.
                st.world.on_output(st.bytes.load(std::sync::atomic::Ordering::Relaxed), crate::procs_win::now_us());
                // In the past, only news from the past is worth a frame; what
                // the live session prints meanwhile waits.
                if crate::scrub::wake_draws(st.scrub.as_ref()) {
                    st.window.request_redraw();
                }
            }
            UiEvent::Tick => {
                if st.quit_at.is_none() && st.bench_step() {
                    // End the session and wait for it to confirm, so nothing
                    // is left behind in the daemon.
                    st.terminal.kill();
                    st.quit_at = Some(Instant::now() + Duration::from_millis(700));
                }
            }
            UiEvent::Replayed => {
                // History is back on the grid at its old size: show it without
                // animating it, then fit the session to this window.
                st.renderer.suppress_reveal = true;
                st.world.on_replayed(st.bytes.load(std::sync::atomic::Ordering::Relaxed), crate::procs_win::now_us());
                let (cols, rows) = st.renderer.grid();
                st.terminal.force_resize(cols, rows);
                st.dirty = true;
                st.window.request_redraw();
                st.sound.play(Heard::Attach);
            }
            UiEvent::Title(t) => st.window.set_title(&format!("{t} - PROTEUS")),
            UiEvent::Bell => st.sound.play(Heard::Bell),
            UiEvent::Clipboard(s) => st.program_copy(s),
            // A frame is asked for only while the World view is up: on the
            // grid nothing changes because a process started.
            UiEvent::Proc { event, live } => {
                let id = st.procs.apply(&event);
                st.world.on_proc(&st.procs, id, &event, live, st.start.elapsed().as_secs_f64());
                if st.world.shown {
                    st.window.request_redraw();
                }
            }
            UiEvent::Exit(code) => {
                paths::log(&format!("session processes: {} seen, {} still running", st.procs.total, st.procs.live_count()));
                if st.world.builds > 0 {
                    let w = &st.world;
                    paths::log(&format!("world view: {} scene builds, avg {} us, max {} us", w.builds, w.build_us_total / w.builds, w.build_us_max));
                }
                st.world.on_exit(code, crate::procs_win::now_us());
                paths::log(&format!("session ended (exit code {code:?}); closing window pid={}", std::process::id()));
                st.stats.write(st.terminal.cols, st.terminal.rows);
                // A session that ended by itself is heard, and the window
                // stays just long enough for that. One the person closed is not.
                if st.quit_at.is_none() && st.sound.on() {
                    st.sound.play(Heard::Exit(code));
                    st.quit_at = Some(Instant::now() + Duration::from_millis(450));
                } else {
                    el.exit();
                }
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(st) = self.state.as_mut() else { return };
        match event {
            // Closing a window that only watches a session leaves it running.
            WindowEvent::CloseRequested if st.session_home == WATCHING => st.quit_at = Some(Instant::now()),
            WindowEvent::CloseRequested => {
                // Closing ends the session. Wait briefly for it to confirm so
                // the request is not lost with the process.
                st.terminal.kill();
                st.quit_at = Some(Instant::now() + Duration::from_millis(700));
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = st.redraw() {
                    paths::log(&format!("render: {e}"));
                    el.exit();
                }
            }
            WindowEvent::Resized(size) => {
                if size.width == 0 || size.height == 0 {
                    return;
                }
                st.config.width = size.width;
                st.config.height = size.height;
                st.surface.configure(&st.gpu.device, &st.config);
                st.renderer.resize(&st.gpu.device, size.width, size.height, PAD * st.scale);
                st.regrid();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                st.scale = scale_factor as f32;
                st.renderer.set_font_px(&st.gpu.queue, st.font_pt * st.scale);
                st.renderer.resize(&st.gpu.device, st.config.width, st.config.height, PAD * st.scale);
                st.regrid();
            }
            WindowEvent::Focused(f) => {
                st.focused = f;
                if !f {
                    // Keys released while another window has focus are never seen here.
                    st.keys_down.clear();
                }
                // Opening the audio device takes long enough to hear; it is
                // opened now and kept while the window has focus, so the
                // first key after a pause is on time.
                st.sound.focus(f);
                st.dirty = true;
                if st.mode().contains(TermMode::FOCUS_IN_OUT) {
                    st.terminal.input(if f { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
                }
                st.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => st.mods = m.state(),
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if reaches_program(event.state, is_synthetic) {
                    st.on_key(event)
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                if !text.is_empty() && !st.world.shown {
                    let bytes = input::encode_text(&text, st.mode());
                    st.send(bytes);
                }
            }
            WindowEvent::CursorMoved { position, .. } => st.on_mouse_move(position.x as f32, position.y as f32),
            WindowEvent::CursorLeft { .. } => st.link_unhover(),
            WindowEvent::MouseInput { state, button, .. } => st.on_mouse_button(button, state),
            WindowEvent::MouseWheel { delta, .. } => st.on_wheel(delta),
            _ => {}
        }
    }
}

/// Is this key event typing? When the window gains focus the window library
/// invents a press for every key that is already down (the Enter still held
/// from launching it, say); those are not the person typing into this
/// window, so they never reach the program. Invented releases (focus lost
/// with a key down) still go through: they only end a press the program saw.
fn reaches_program(state: ElementState, is_synthetic: bool) -> bool {
    !(is_synthetic && state == ElementState::Pressed)
}

/// Remove measurement files left by windows that closed more than a day ago.
fn sweep_stats() {
    let Ok(entries) = std::fs::read_dir(paths::data_dir()) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let stale = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d.as_secs() > 86_400);
        let ours = name.starts_with("stats-") || name.starts_with("sound-");
        if name == "stats.json" || (ours && name.ends_with(".json") && stale) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The session `--attach` names, if the command line asks for one. A missing
/// or unreadable number is an error: an ordinary launch in its place would
/// adopt, or start, some other session.
fn attach_target(args: &[String]) -> Result<Option<u64>, String> {
    // After `-e` the words are the program's own.
    let Some(i) = args.iter().take_while(|a| *a != "-e").position(|a| a == "--attach") else { return Ok(None) };
    match args.get(i + 1).and_then(|s| s.parse::<u64>().ok()).filter(|&id| id != 0) {
        Some(id) => Ok(Some(id)),
        None => Err(format!("--attach needs a session number (`proteus sessions` lists them); got {}", args.get(i + 1).map_or("nothing".to_string(), |s| format!("{s:?}")))),
    }
}

pub fn run(args: &[String]) -> Res<()> {
    attach_target(args)?; // refused before any window exists
    sweep_stats();
    let event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App { proxy: event_loop.create_proxy(), opts: Options::parse(args), state: None };
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn attach_names_a_session_or_is_refused() {
        let a = |words: &[&str]| super::attach_target(&words.iter().map(|w| w.to_string()).collect::<Vec<_>>());
        assert_eq!(a(&[]), Ok(None));
        assert_eq!(a(&["--no-sound"]), Ok(None));
        assert_eq!(a(&["--attach", "7"]), Ok(Some(7)));
        assert_eq!(a(&["--font", "14", "--attach", "12", "--no-light"]), Ok(Some(12)));
        for bad in [&["--attach"][..], &["--attach", "seven"], &["--attach", "0"], &["--attach", "-3"], &["--attach", "--local"], &["--attach", "3.5"]] {
            let why = a(bad).expect_err("an ordinary window must not open in its place");
            assert!(why.contains("--attach needs a session number"), "{why}");
        }
        // After -e the words are the program's own.
        assert_eq!(a(&["-e", "tool.exe", "--attach"]), Ok(None));
    }

    /// The Enter that launched the window, still held as it gains focus,
    /// must not run an empty line: invented presses are dropped, and
    /// everything else (real presses, real and invented releases) passes.
    #[test]
    fn keys_held_while_focus_arrives_are_not_typed() {
        use winit::event::ElementState::{Pressed, Released};
        assert!(!super::reaches_program(Pressed, true), "an invented press reached the program");
        assert!(super::reaches_program(Pressed, false));
        assert!(super::reaches_program(Released, false));
        assert!(super::reaches_program(Released, true), "an invented release must still end the press");
    }
}
