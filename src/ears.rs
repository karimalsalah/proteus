//! `proteus ears`: the sound of every event, rendered to a WAV file with no
//! sound card involved, plus measurements. The twin of `proteus shot`: this
//! is how the engine's voice is checked, and how an agent can hear it.
//!
//!   proteus ears [--out WAV] [--volume V] [--play]
//!
//! `--play` also sends the same score through the real audio device and
//! reports what the device did (opened, frames delivered, peak, closed).
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::sound::Sound;
use crate::synth::{level, Event, Synth};
use crate::Res;

const RATE: u32 = 48_000;

/// (name, start, end) in seconds, and the events inside each stretch.
fn score() -> (Vec<(&'static str, f32, f32)>, Vec<(f32, Event)>) {
    let mut ev: Vec<(f32, Event)> = Vec::new();
    for i in 0..6 {
        ev.push((0.10 + i as f32 * 0.11, Event::Key));
    }
    ev.push((1.00, Event::Text { cells: 6, red: 0, green: 0, bulk: false, pan: -0.8 }));
    ev.push((1.30, Event::Text { cells: 80, red: 0, green: 0, bulk: false, pan: 0.0 }));
    ev.push((1.60, Event::Text { cells: 900, red: 0, green: 0, bulk: false, pan: 0.8 }));
    // A flood: a screenful every 8 ms for a second and a half.
    for i in 0..188 {
        ev.push((2.00 + i as f32 * 0.008, Event::Text { cells: 4000, red: 0, green: 0, bulk: true, pan: 0.0 }));
    }
    ev.push((4.20, Event::Bell));
    ev.push((5.00, Event::Attach));
    ev.push((5.80, Event::Exit(Some(0))));
    ev.push((6.60, Event::Exit(Some(1))));
    // The same line of text three ways: plain, red, green.
    ev.push((7.50, Event::Text { cells: 80, red: 0, green: 0, bulk: false, pan: 0.0 }));
    ev.push((8.00, Event::Text { cells: 80, red: 80, green: 0, bulk: false, pan: 0.0 }));
    ev.push((8.50, Event::Text { cells: 80, red: 0, green: 80, bulk: false, pan: 0.0 }));
    ev.push((9.60, Event::Outcome { ok: true }));
    ev.push((10.20, Event::Outcome { ok: false }));
    ev.push((10.80, Event::NeedsYou));
    let parts = vec![
        ("keys", 0.0, 0.9),
        ("text", 0.9, 2.0),
        ("flood", 2.0, 4.1),
        ("bell", 4.1, 4.9),
        ("attach", 4.9, 5.7),
        ("exit_ok", 5.7, 6.5),
        ("exit_fail", 6.5, 7.3),
        ("text_plain", 7.45, 7.95),
        ("text_red", 7.95, 8.45),
        ("text_green", 8.45, 8.95),
        ("outcome_ok", 9.55, 10.15),
        ("outcome_fail", 10.15, 10.75),
        ("needs_you", 10.75, 11.35),
        ("after", 11.5, 12.0),
    ];
    (parts, ev)
}

fn write_wav(path: &std::path::Path, samples: &[f32]) -> Res<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let bytes = (samples.len() * 2) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + bytes).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&2u16.to_le_bytes())?; // stereo
    f.write_all(&RATE.to_le_bytes())?;
    f.write_all(&(RATE * 4).to_le_bytes())?;
    f.write_all(&4u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&bytes.to_le_bytes())?;
    for s in samples {
        f.write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
    }
    Ok(())
}

pub fn run(args: &[String]) -> Res<()> {
    let mut out = PathBuf::from("proteus-ears.wav");
    let mut volume = 0.5f32;
    let mut play = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                out = PathBuf::from(args.get(i + 1).ok_or("--out needs a file")?);
                i += 1;
            }
            "--volume" => {
                volume = args.get(i + 1).ok_or("--volume needs a number")?.parse::<f32>()?.clamp(0.0, 1.0);
                i += 1;
            }
            "--play" => play = true,
            other => return Err(format!("unknown option {other}").into()),
        }
        i += 1;
    }

    let (parts, events) = score();
    let total = 12.0f32;
    let mut synth = Synth::new(RATE, volume);
    let mut samples = vec![0.0f32; (total * RATE as f32) as usize * 2];
    let mut next = 0;
    for (n, chunk) in samples.chunks_mut(240 * 2).enumerate() {
        let t = n as f32 * 240.0 / RATE as f32;
        while next < events.len() && events[next].0 <= t {
            synth.trigger(events[next].1);
            next += 1;
        }
        synth.render(chunk, 2);
    }
    write_wav(&out, &samples)?;

    let at = |s: f32| ((s * RATE as f32) as usize * 2).min(samples.len());
    let mut report = serde_json::Map::new();
    for (name, a, b) in &parts {
        let (rms, peak) = level(&samples[at(*a)..at(*b)]);
        report.insert((*name).into(), serde_json::json!({ "rms": rms, "peak": peak }));
    }
    let (rms, peak) = level(&samples);
    let mut v = serde_json::json!({
        "wav": out.display().to_string(),
        "volume": volume,
        "seconds": total,
        "rms": rms,
        "peak": peak,
        "notes": synth.started,
        "dropped": synth.dropped,
        "parts": report,
    });

    if play {
        let sound = Sound::start(volume);
        // As the window does on gaining focus: have the device ready first.
        sound.warm();
        std::thread::sleep(Duration::from_millis(500));
        let t0 = Instant::now();
        for (t, e) in &events {
            let due = Duration::from_secs_f32(*t);
            if let Some(wait) = due.checked_sub(t0.elapsed()) {
                std::thread::sleep(wait);
            }
            sound.play(*e);
        }
        // Wait for the stream to stop by itself, as it must when idle.
        let deadline = Instant::now() + Duration::from_secs(6);
        std::thread::sleep(Duration::from_millis(600));
        while sound.meter.state.load(std::sync::atomic::Ordering::Acquire) == crate::sound::RUNNING && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(100));
        let file = crate::paths::data_dir().join(format!("sound-{}.json", std::process::id()));
        v["device"] = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| sound.meter.json("not opened"));
        let _ = std::fs::remove_file(file);
    }
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    Ok(())
}
