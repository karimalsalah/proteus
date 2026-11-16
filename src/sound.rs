//! The audio device. A running stream wakes the processor a hundred times a
//! second even for silence, so the stream runs only while something is
//! sounding. Between sounds it is paused (ready, but asleep: no callbacks, no
//! processor time; measured 0.0 ms over 50 s). While the window has focus the
//! paused device is kept; a minute after focus is lost it is let go.
//!
//! Opening a device cold takes long enough to hear (measured 20-210 ms), so
//! the window opens it on gaining focus: by the time a key is pressed the
//! stream only has to be resumed. When the default output changes, Windows
//! says so (endpoint.rs) and a held device is reopened on the new default at
//! once, paused; only if that registration failed does gaining focus re-open
//! a device held for more than a minute instead. A key or text event that
//! still arrives too late to be truthful is dropped rather than played out
//! of step.
//!
//! Everything here runs on its own thread; the window never waits for audio.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};

use crate::paths;
use crate::synth::{Event, Synth};

/// How long the stream keeps running after the last note, so that typing
/// does not stop and start it for every key.
const LINGER: Duration = Duration::from_millis(1500);
/// How long a paused stream is kept once the window has lost focus.
const KEEP_PAUSED: Duration = Duration::from_secs(60);
/// A key tick or text grain later than this is no longer about anything.
const STALE: Duration = Duration::from_millis(80);
/// Tests: make the next change look like a real move of the default.
#[cfg(test)]
static PRETEND_MOVED: AtomicBool = AtomicBool::new(false);
/// How long a default-output change waits for the rest of its burst.
const COALESCE: Duration = Duration::from_millis(10);

pub const CLOSED: u8 = 0;
pub const PAUSED: u8 = 1;
pub const RUNNING: u8 = 2;

/// What the device actually did, counted where it happens.
#[derive(Default)]
pub struct Meter {
    pub opens: AtomicU64,
    pub open_us_last: AtomicU64,
    pub open_us_max: AtomicU64,
    pub resumes: AtomicU64,
    /// From an event happening to the callback in which it began to sound.
    pub lag_us_sum: AtomicU64,
    pub lag_us_max: AtomicU64,
    pub lag_n: AtomicU64,
    /// Frames the device asked for and was given.
    pub frames: AtomicU64,
    /// Loudest sample handed to the device (f32 bits).
    pub peak: AtomicU32,
    pub events: AtomicU64,
    pub notes: AtomicU64,
    pub dropped: AtomicU64,
    /// Events dropped because the device was not ready in time.
    pub stale: AtomicU64,
    pub state: AtomicU8,
    quiet: AtomicBool,
    failed: AtomicBool,
    /// Following the default output (endpoint.rs): whether Windows will
    /// say when it changes, how often it did, and how often a held device
    /// was reopened on the new default, from the first callback to ready.
    pub watching: AtomicBool,
    pub device_callbacks: AtomicU64,
    pub follows: AtomicU64,
    pub follow_us_last: AtomicU64,
    pub follow_us_max: AtomicU64,
    /// Callbacks that arrived after their change was already followed.
    pub coalesced: AtomicU64,
    /// The sound thread, so its own processor time can be read.
    pub thread_id: std::sync::atomic::AtomicU32,
}

impl Meter {
    pub fn json(&self, device: &str) -> serde_json::Value {
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        serde_json::json!({
            "pid": std::process::id(),
            "device": device,
            "state": match self.state.load(Ordering::Relaxed) { RUNNING => "running", PAUSED => "paused", _ => "closed" },
            "opens": n(&self.opens),
            "open_ms_last": n(&self.open_us_last) as f64 / 1000.0,
            "open_ms_max": n(&self.open_us_max) as f64 / 1000.0,
            "resumes": n(&self.resumes),
            "event_to_callback_ms": {
                "n": n(&self.lag_n),
                "avg": n(&self.lag_us_sum) as f64 / 1000.0 / n(&self.lag_n).max(1) as f64,
                "max": n(&self.lag_us_max) as f64 / 1000.0,
            },
            "frames": n(&self.frames),
            "peak": f32::from_bits(self.peak.load(Ordering::Relaxed)),
            "events": n(&self.events),
            "notes": n(&self.notes),
            "dropped": n(&self.dropped),
            "stale": n(&self.stale),
            "default_device": {
                "watching": self.watching.load(Ordering::Relaxed),
                "callbacks": n(&self.device_callbacks),
                "follows": n(&self.follows),
                "coalesced": n(&self.coalesced),
                "callback_to_ready_ms_last": n(&self.follow_us_last) as f64 / 1000.0,
                "callback_to_ready_ms_max": n(&self.follow_us_max) as f64 / 1000.0,
            },
        })
    }

    fn save(&self, device: &str) {
        let file = paths::data_dir().join(format!("sound-{}.json", std::process::id()));
        let _ = std::fs::write(file, serde_json::to_vec_pretty(&self.json(device)).unwrap_or_default());
    }
}

enum Msg {
    Play(Event, Instant),
    /// Get the device ready without making a sound.
    Warm,
    /// The window gained or lost focus.
    Focus(bool),
    /// Windows said the default output changed (endpoint.rs).
    Device,
}

/// Handle held by the window. Cheap to call; never blocks. The sender is
/// shared only weakly with the device callback, so dropping this handle
/// still ends the sound thread.
pub struct Sound {
    tx: Option<Arc<Sender<Msg>>>,
    pub meter: Arc<Meter>,
}

impl Sound {
    pub fn off() -> Self {
        Self { tx: None, meter: Arc::new(Meter::default()) }
    }

    /// `volume` is 0 to 1; 0 means no thread and no device, ever.
    pub fn start(volume: f32) -> Self {
        if !(volume > 0.0) {
            return Self::off();
        }
        let (tx, rx) = unbounded::<Msg>();
        let tx = Arc::new(tx);
        let nudge = Arc::downgrade(&tx);
        let meter = Arc::new(Meter::default());
        let m = meter.clone();
        let spawned = std::thread::Builder::new().name("sound".into()).spawn(move || run(rx, nudge, volume.min(1.0), m));
        Self { tx: spawned.ok().map(|_| tx), meter }
    }

    pub fn on(&self) -> bool {
        self.tx.is_some()
    }

    pub fn play(&self, e: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Play(e, Instant::now()));
        }
    }

    /// Have the device ready without making a sound.
    pub fn warm(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Warm);
        }
    }

    /// The person turned to this window, or away from it.
    pub fn focus(&self, on: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Focus(on));
        }
    }
}

/// Keys and text are about one instant; the rest still mean something late.
fn perishable(e: &Event) -> bool {
    matches!(e, Event::Key | Event::Text { .. })
}

/// The part that runs inside the device's callback.
struct Feed {
    synth: Synth,
    events: Receiver<(Event, Instant)>,
    meter: Arc<Meter>,
    channels: usize,
    scratch: Vec<f32>,
}

impl Feed {
    fn fill(&mut self, out: &mut [f32]) {
        for (e, at) in self.events.try_iter() {
            let lag = at.elapsed();
            if perishable(&e) && lag > STALE {
                self.meter.stale.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let us = lag.as_micros() as u64;
            self.meter.lag_us_sum.fetch_add(us, Ordering::Relaxed);
            self.meter.lag_us_max.fetch_max(us, Ordering::Relaxed);
            self.meter.lag_n.fetch_add(1, Ordering::Relaxed);
            self.synth.trigger(e);
        }
        self.synth.render(out, self.channels);
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let m = &self.meter;
        if peak > f32::from_bits(m.peak.load(Ordering::Relaxed)) {
            m.peak.store(peak.to_bits(), Ordering::Relaxed);
        }
        m.frames.fetch_add((out.len() / self.channels) as u64, Ordering::Relaxed);
        m.notes.store(self.synth.started, Ordering::Relaxed);
        m.dropped.store(self.synth.dropped, Ordering::Relaxed);
        m.quiet.store(self.synth.silent(), Ordering::Release);
    }

    fn fill_i16(&mut self, out: &mut [i16]) {
        let mut buf = std::mem::take(&mut self.scratch);
        buf.resize(out.len(), 0.0);
        self.fill(&mut buf);
        for (o, s) in out.iter_mut().zip(&buf) {
            *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        self.scratch = buf;
    }
}

struct Device {
    stream: cpal::Stream,
    to_feed: Sender<(Event, Instant)>,
    name: String,
}

/// Open the default output, paused.
fn open(volume: f32, meter: &Arc<Meter>) -> Result<Device, String> {
    let t0 = Instant::now();
    let device = cpal::default_host().default_output_device().ok_or("no output device")?;
    let name = device.name().unwrap_or_else(|_| "unknown".into());
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let config = supported.config();
    let (to_feed, events) = unbounded::<(Event, Instant)>();
    let mut feed = Feed {
        synth: Synth::new(config.sample_rate.0, volume),
        events,
        meter: meter.clone(),
        channels: config.channels.max(1) as usize,
        scratch: Vec::new(),
    };
    meter.failed.store(false, Ordering::Release);
    let failed = meter.clone();
    // A device that goes away (headphones unplugged) is reopened on the next event.
    let on_error = move |e: cpal::StreamError| {
        paths::log(&format!("sound: stream error: {e}"));
        failed.failed.store(true, Ordering::Release);
    };
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(&config, move |d: &mut [f32], _: &cpal::OutputCallbackInfo| feed.fill(d), on_error, None),
        cpal::SampleFormat::I16 => device.build_output_stream(&config, move |d: &mut [i16], _: &cpal::OutputCallbackInfo| feed.fill_i16(d), on_error, None),
        other => return Err(format!("unsupported sample format {other:?}")),
    }
    .map_err(|e| e.to_string())?;
    stream.pause().map_err(|e| e.to_string())?;
    let us = t0.elapsed().as_micros() as u64;
    meter.open_us_last.store(us, Ordering::Relaxed);
    meter.open_us_max.fetch_max(us, Ordering::Relaxed);
    meter.opens.fetch_add(1, Ordering::Relaxed);
    meter.state.store(PAUSED, Ordering::Release);
    Ok(Device { stream, to_feed, name: format!("{name} ({} Hz, {} ch, {:?})", config.sample_rate.0, config.channels, supported.sample_format()) })
}

fn run(rx: Receiver<Msg>, nudge: std::sync::Weak<Sender<Msg>>, volume: f32, meter: Arc<Meter>) {
    meter.thread_id.store(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }, Ordering::Relaxed);
    // Before cpal touches COM on this thread (it accepts the mode set here).
    let watch = match crate::endpoint::Watch::start(move || {
        if let Some(tx) = nudge.upgrade() {
            let _ = tx.try_send(Msg::Device);
        }
    }) {
        Ok(w) => {
            meter.watching.store(true, Ordering::Release);
            Some(w)
        }
        Err(e) => {
            paths::log(&format!("sound: the default output will be followed only on focus ({e})"));
            None
        }
    };
    let mut refused_until: Option<Instant> = None;
    let mut dev: Option<Device> = None;
    let mut last_name = String::from("not opened");
    // When the stream went quiet (running) or was paused.
    let mut since = Instant::now();
    let mut opened_at = Instant::now();
    let mut focused = false;
    // The default output when the held device was opened (endpoint.rs).
    let mut on_id: Option<String> = None;
    loop {
        let state = meter.state.load(Ordering::Acquire);
        // Closed: sleep until something happens. Paused: sleep until
        // something happens or the device has been held long enough.
        // Running: look in now and then to see whether the last note is over.
        let msg = match state {
            RUNNING => rx.recv_timeout(Duration::from_millis(250)),
            PAUSED if focused => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            PAUSED => rx.recv_timeout(KEEP_PAUSED.saturating_sub(since.elapsed()).max(Duration::from_millis(1))),
            _ => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let mut leaving = false;
        match msg {
            Ok(m) => {
                // Without word from Windows, returning to the window is the
                // moment to follow a default output that changed meanwhile.
                if watch.is_none() && matches!(m, Msg::Focus(true)) && state == PAUSED && opened_at.elapsed() >= KEEP_PAUSED {
                    dev = None;
                    meter.state.store(CLOSED, Ordering::Release);
                }
                // The default output changed: a held device moves to the new
                // one now, paused (a closed one opens there when next wanted).
                if matches!(m, Msg::Device) {
                    // Windows reports one change once per role, back to back:
                    // let the burst finish so it costs one reopen.
                    std::thread::sleep(COALESCE);
                }
                if let Some(at) = matches!(m, Msg::Device).then(|| watch.as_ref().and_then(|w| w.take())).flatten() {
                    if let Some(w) = &watch {
                        meter.device_callbacks.store(w.changed.callbacks.load(Ordering::Relaxed), Ordering::Relaxed);
                    }
                    refused_until = None;
                    #[cfg(test)]
                    if PRETEND_MOVED.swap(false, Ordering::AcqRel) {
                        on_id = Some("a device that is no longer the default".into());
                    }
                    // A callback whose change is already followed (the rest
                    // of a burst, however spread out) costs nothing more.
                    let now_id = watch.as_ref().and_then(|w| w.default_id());
                    if dev.is_some() && now_id.is_some() && now_id == on_id {
                        meter.coalesced.fetch_add(1, Ordering::Relaxed);
                    } else if dev.is_some() {
                        on_id = now_id;
                        dev = None;
                        meter.state.store(CLOSED, Ordering::Release);
                        match open(volume, &meter) {
                            Ok(d) => {
                                let us = crate::endpoint::now_us().saturating_sub(at);
                                meter.follows.fetch_add(1, Ordering::Relaxed);
                                meter.follow_us_last.store(us, Ordering::Relaxed);
                                meter.follow_us_max.fetch_max(us, Ordering::Relaxed);
                                paths::log(&format!("sound: default output changed; now {} (ready {:.1} ms after Windows said so)", d.name, us as f64 / 1000.0));
                                last_name = d.name.clone();
                                since = Instant::now();
                                opened_at = since;
                                dev = Some(d);
                            }
                            Err(e) => paths::log(&format!("sound: default output changed; it cannot be opened ({e})")),
                        }
                        meter.save(&last_name);
                    }
                }
                let wanted = matches!(m, Msg::Play(..) | Msg::Warm | Msg::Focus(true));
                if wanted && dev.is_none() && !refused_until.is_some_and(|t| Instant::now() < t) {
                    on_id = watch.as_ref().and_then(|w| w.default_id());
                    match open(volume, &meter) {
                        Ok(d) => {
                            last_name = d.name.clone();
                            since = Instant::now();
                            opened_at = since;
                            dev = Some(d);
                        }
                        Err(e) => {
                            // Say so once, then stop trying for a while: no sound
                            // card must not mean a failed open on every keystroke.
                            paths::log(&format!("sound: {e}; staying silent for 30 s"));
                            refused_until = Some(Instant::now() + Duration::from_secs(30));
                        }
                    }
                }
                match m {
                    Msg::Play(e, at) => {
                        meter.events.fetch_add(1, Ordering::Relaxed);
                        if let Some(d) = &dev {
                            let _ = d.to_feed.send((e, at));
                            meter.quiet.store(false, Ordering::Release);
                            if meter.state.load(Ordering::Acquire) != RUNNING {
                                if d.stream.play().is_err() {
                                    meter.failed.store(true, Ordering::Release);
                                }
                                meter.resumes.fetch_add(1, Ordering::Relaxed);
                                meter.state.store(RUNNING, Ordering::Release);
                            }
                            since = Instant::now();
                        }
                    }
                    // Being asked again while paused keeps the device a while longer.
                    Msg::Warm => {
                        if meter.state.load(Ordering::Acquire) == PAUSED {
                            since = Instant::now();
                        }
                    }
                    // Losing focus starts the countdown to letting the device go.
                    Msg::Focus(on) => {
                        focused = on;
                        if meter.state.load(Ordering::Acquire) == PAUSED {
                            since = Instant::now();
                        }
                    }
                    // Handled above.
                    Msg::Device => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The window is going: let the last note finish, then leave.
            Err(RecvTimeoutError::Disconnected) => {
                leaving = true;
                let end = Instant::now() + Duration::from_millis(700);
                while meter.state.load(Ordering::Acquire) == RUNNING && !meter.quiet.load(Ordering::Acquire) && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }

        let state = meter.state.load(Ordering::Acquire);
        let failed = meter.failed.load(Ordering::Acquire);
        if state == RUNNING && !failed && !leaving {
            if !meter.quiet.load(Ordering::Acquire) {
                since = Instant::now();
            } else if since.elapsed() >= LINGER {
                // The last note is over: stop the callbacks, keep the device.
                if let Some(d) = &dev {
                    let _ = d.stream.pause();
                }
                meter.state.store(PAUSED, Ordering::Release);
                since = Instant::now();
                meter.save(&last_name);
            }
        }
        if dev.is_some() && (failed || leaving || (state == PAUSED && !focused && since.elapsed() >= KEEP_PAUSED)) {
            dev = None;
            meter.state.store(CLOSED, Ordering::Release);
            meter.save(&last_name);
        }
        if leaving {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::Audio::{eCapture, eCommunications, eConsole, eMultimedia, eRender, IMMNotificationClient};
    use windows_core::{Interface, PCWSTR};

    fn wait(what: impl Fn() -> bool, ms: u64) -> bool {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            if what() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        what()
    }

    /// The real sound thread, holding the real default output paused. Never played.
    fn held() -> Option<Sound> {
        // Tests share one process: never repoint a data folder another test uses.
        if std::env::var_os("PROTEUS_DATA").is_none() {
            let dir = std::env::temp_dir().join(format!("proteus-sound-test-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("PROTEUS_DATA", &dir);
        }
        let s = Sound::start(0.5);
        s.warm();
        let m = s.meter.clone();
        if wait(|| m.state.load(Ordering::Acquire) == PAUSED, 5000) {
            Some(s)
        } else {
            eprintln!("no output device could be opened here: the check was not run");
            None
        }
    }

    /// The sound tests share the one registered client (`endpoint::LAST`).
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn client() -> IMMNotificationClient {
        let raw = *crate::endpoint::LAST.lock().unwrap() as *mut core::ffi::c_void;
        unsafe { IMMNotificationClient::from_raw_borrowed(&raw) }.expect("a registered client").clone()
    }

    /// A changed default output moves the held device to the new default at
    /// once, paused (silent), and once per change however many callbacks
    /// Windows sends: one per role, plus ones that are not about output.
    /// The callbacks are made by this test through the registered object's
    /// own COM interface, as the audio service makes them. The default device
    /// itself is never changed (a person may be listening): the sound thread
    /// is told to forget which device it holds, so it must reopen, and the
    /// reopen of the real default is what is timed.
    #[test]
    fn a_changed_default_output_is_followed_at_once_and_once() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = held() else { return };
        let m = s.meter.clone();
        assert!(m.watching.load(Ordering::Acquire), "the default output is not being watched");
        let client = client();
        let mut times = Vec::new();
        for round in 1..=5u64 {
            let opens = m.opens.load(Ordering::Relaxed);
            PRETEND_MOVED.store(true, Ordering::Release);
            unsafe {
                client.OnDefaultDeviceChanged(eRender, eConsole, PCWSTR::null()).unwrap();
                client.OnDefaultDeviceChanged(eRender, eMultimedia, PCWSTR::null()).unwrap();
                client.OnDefaultDeviceChanged(eRender, eCommunications, PCWSTR::null()).unwrap();
                client.OnDefaultDeviceChanged(eCapture, eConsole, PCWSTR::null()).unwrap();
            }
            assert!(wait(|| m.follows.load(Ordering::Relaxed) == round, 3000), "round {round}: the change was not followed");
            std::thread::sleep(Duration::from_millis(60));
            assert_eq!(m.follows.load(Ordering::Relaxed), round, "one change must cost one reopen");
            assert_eq!(m.opens.load(Ordering::Relaxed), opens + 1);
            assert_eq!(m.state.load(Ordering::Acquire), PAUSED, "the new device must be held paused");
            times.push(m.follow_us_last.load(Ordering::Relaxed) as f64 / 1000.0);
        }
        assert_eq!(m.device_callbacks.load(Ordering::Relaxed), 10, "console and multimedia count; communications and capture do not");
        println!("default output followed {} times, callback to ready (ms): {times:.1?}", times.len());
        assert!(times.iter().all(|&t| t < 250.0), "{times:?}");
        drop(s);
    }

    /// The callbacks of one change spread out, the later ones landing while
    /// the new device is still being opened and after it is ready: still one
    /// reopen. A callback whose change is already followed costs nothing.
    #[test]
    fn a_spread_out_burst_still_costs_one_reopen() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = held() else { return };
        let m = s.meter.clone();
        let client = client();
        let (opens, follows, coalesced) = (m.opens.load(Ordering::Relaxed), m.follows.load(Ordering::Relaxed), m.coalesced.load(Ordering::Relaxed));
        PRETEND_MOVED.store(true, Ordering::Release);
        for (k, role) in [eConsole, eMultimedia, eConsole, eMultimedia, eConsole, eMultimedia].into_iter().enumerate() {
            unsafe { client.OnDefaultDeviceChanged(eRender, role, PCWSTR::null()).unwrap() };
            if k + 1 < 6 {
                std::thread::sleep(Duration::from_millis(40));
            }
        }
        assert!(wait(|| m.follows.load(Ordering::Relaxed) > follows, 3000), "the change was not followed");
        std::thread::sleep(Duration::from_millis(400));
        let (o, f, c) = (m.opens.load(Ordering::Relaxed) - opens, m.follows.load(Ordering::Relaxed) - follows, m.coalesced.load(Ordering::Relaxed) - coalesced);
        println!("six callbacks 40 ms apart: {f} reopen(s), {o} open(s), {c} coalesced");
        assert_eq!((f, o), (1, 1), "a spread-out burst reopened the device more than once");
        assert!(c >= 1, "the later callbacks were not seen as the same change");
        assert_eq!(m.state.load(Ordering::Acquire), PAUSED);
        drop(s);
    }

    /// Holding the device paused, and being registered for device changes,
    /// costs the sound thread no processor time while nothing sounds.
    #[test]
    fn a_held_device_and_the_watch_cost_nothing_while_silent() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION};
        let Some(s) = held() else { return };
        let m = s.meter.clone();
        let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let ms = |k: &FILETIME, u: &FILETIME| ((k.dwHighDateTime as u64) << 32 | k.dwLowDateTime as u64) as f64 / 1e4 + ((u.dwHighDateTime as u64) << 32 | u.dwLowDateTime as u64) as f64 / 1e4;
        let h = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, m.thread_id.load(Ordering::Relaxed)) };
        assert!(!h.is_null(), "the sound thread could not be opened");
        let sample = || unsafe {
            let (mut c, mut e, mut k, mut u) = (z(), z(), z(), z());
            GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u);
            let t = ms(&k, &u);
            GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
            (t, ms(&k, &u))
        };
        std::thread::sleep(Duration::from_millis(500));
        let (t0, p0) = sample();
        std::thread::sleep(Duration::from_secs(10));
        let (t1, p1) = sample();
        unsafe { CloseHandle(h) };
        println!("held paused and watched for 10 s: sound thread {:.1} ms of processor time; whole test process {:.1} ms (other tests may be running)", t1 - t0, p1 - p0);
        assert_eq!(m.state.load(Ordering::Acquire), PAUSED);
        assert!(t1 - t0 < 1.0, "the sound thread ran while silent: {:.1} ms", t1 - t0);
        drop(s);
    }
}
