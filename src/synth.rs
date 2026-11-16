//! The voice of the terminal: every sound is computed from an event that
//! really happened (a key sent, text arriving, a bell, a session ending).
//! Nothing loops, nothing plays by itself, and there are no sample files.
//!
//! This module is pure arithmetic with its own sample clock, so the same
//! code renders to the audio device (`sound.rs`) and to a file (`ears.rs`)
//! and can be tested without a sound card.

/// Voices that may sound at once. Past this, arriving text and key ticks are
/// dropped (they are plentiful); rarer events take the oldest voice instead.
pub const MAX_VOICES: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// A key was sent to the program.
    Key,
    /// Text landed in `cells` empty cells, `red` and `green` of them in those
    /// colours. `bulk` marks a screenful arriving at once; `pan` is where on
    /// the line it landed, -1 (left) to 1 (right).
    Text { cells: u32, red: u32, green: u32, bulk: bool, pan: f32 },
    Bell,
    /// The session ended by itself, with this exit code if known.
    Exit(Option<u32>),
    /// A session that had lost its window was picked up again.
    Attach,
    /// A command finished, by the shell's own report of its status. Told
    /// apart by contour, not by loudness: up for success, down for failure.
    Outcome { ok: bool },
    /// A program is blocked waiting for the person (permission, a question).
    /// Short-short-long, so it cannot be mistaken for an outcome.
    NeedsYou,
}

struct Voice {
    /// Samples still to wait before starting (second note of a pair).
    delay: u32,
    age: u32,
    dur: u32,
    phase: f32,
    f0: f32,
    f1: f32,
    amp: f32,
    attack: f32,
    env: f32,
    decay: f32,
    /// Share of filtered noise against the sine, 0 to 1.
    noise: f32,
    lp: f32,
    lp_k: f32,
    gl: f32,
    gr: f32,
}

/// One note, in the units a person thinks in.
struct Note {
    delay_ms: f32,
    dur_ms: f32,
    f0: f32,
    f1: f32,
    amp: f32,
    attack_ms: f32,
    tau_ms: f32,
    noise: f32,
    cutoff: f32,
    pan: f32,
}

impl Default for Note {
    fn default() -> Self {
        Self { delay_ms: 0.0, dur_ms: 100.0, f0: 440.0, f1: 440.0, amp: 0.1, attack_ms: 2.0, tau_ms: 30.0, noise: 0.0, cutoff: 4000.0, pan: 0.0 }
    }
}

pub struct Synth {
    rate: f32,
    volume: f32,
    voices: Vec<Voice>,
    rng: u32,
    clock: u64,
    last_key: Option<u64>,
    last_bell: Option<u64>,
    last_outcome: Option<u64>,
    // Arriving text is gathered and voiced at most once per `text_gap`.
    pending_cells: u32,
    pending_red: u32,
    pending_green: u32,
    pending_bulk: bool,
    pending_pan: f32,
    last_text: Option<u64>,
    text_gap: u64,
    /// Notes started and events dropped, for the measurements file.
    pub started: u64,
    pub dropped: u64,
}

impl Synth {
    pub fn new(rate: u32, volume: f32) -> Self {
        Self {
            rate: rate.max(8000) as f32,
            volume: volume.clamp(0.0, 1.0),
            voices: Vec::with_capacity(MAX_VOICES),
            rng: 0x9E37_79B9,
            clock: 0,
            last_key: None,
            last_bell: None,
            last_outcome: None,
            pending_cells: 0,
            pending_red: 0,
            pending_green: 0,
            pending_bulk: false,
            pending_pan: 0.0,
            last_text: None,
            text_gap: 0,
            started: 0,
            dropped: 0,
        }
    }

    fn ms(&self, ms: f32) -> u64 {
        (ms * 0.001 * self.rate) as u64
    }

    fn noise(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// True when nothing is sounding and nothing is waiting to: the device
    /// can be closed.
    pub fn silent(&self) -> bool {
        self.voices.is_empty() && self.pending_cells == 0
    }

    fn push(&mut self, n: Note, droppable: bool) {
        if self.voices.len() >= MAX_VOICES {
            if droppable {
                self.dropped += 1;
                return;
            }
            self.voices.remove(0);
        }
        let angle = (n.pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
        let samples = |ms: f32| (ms * 0.001 * self.rate).max(1.0);
        self.voices.push(Voice {
            delay: samples(n.delay_ms) as u32 * (n.delay_ms > 0.0) as u32,
            age: 0,
            dur: samples(n.dur_ms) as u32,
            phase: 0.0,
            f0: n.f0,
            f1: n.f1,
            amp: n.amp,
            attack: samples(n.attack_ms),
            env: 1.0,
            decay: (-1.0 / samples(n.tau_ms)).exp(),
            noise: n.noise.clamp(0.0, 1.0),
            lp: 0.0,
            lp_k: 1.0 - (-std::f32::consts::TAU * n.cutoff / self.rate).exp(),
            gl: angle.cos(),
            gr: angle.sin(),
        });
        self.started += 1;
    }

    pub fn trigger(&mut self, e: Event) {
        match e {
            Event::Key => {
                // Held keys repeat faster than a tick can be told apart.
                if self.last_key.is_some_and(|t| self.clock < t + self.ms(25.0)) {
                    self.dropped += 1;
                    return;
                }
                self.last_key = Some(self.clock);
                // No two ticks are identical, as no two key strikes are.
                let f = 1750.0 + 350.0 * (self.noise() * 0.5 + 0.5);
                self.push(Note { dur_ms: 24.0, f0: f, f1: f * 0.8, amp: 0.20, attack_ms: 0.4, tau_ms: 5.0, noise: 0.8, cutoff: 3200.0, ..Note::default() }, true);
            }
            Event::Text { cells, red, green, bulk, pan } => {
                if cells == 0 {
                    return;
                }
                self.pending_cells = self.pending_cells.saturating_add(cells);
                self.pending_red = self.pending_red.saturating_add(red.min(cells));
                self.pending_green = self.pending_green.saturating_add(green.min(cells));
                self.pending_bulk |= bulk;
                self.pending_pan = pan;
                self.flush_text();
            }
            Event::Bell => {
                if self.last_bell.is_some_and(|t| self.clock < t + self.ms(150.0)) {
                    self.dropped += 1;
                    return;
                }
                self.last_bell = Some(self.clock);
                self.push(Note { dur_ms: 420.0, f0: 880.0, f1: 880.0, amp: 0.20, tau_ms: 90.0, ..Note::default() }, false);
                self.push(Note { dur_ms: 420.0, f0: 1318.5, f1: 1318.5, amp: 0.09, tau_ms: 70.0, ..Note::default() }, false);
            }
            Event::Exit(Some(0)) => self.pair(659.3, 440.0),
            Event::Exit(_) => {
                // A failure: two low notes a semitone apart, which beat.
                for f in [196.0, 207.7] {
                    self.push(Note { dur_ms: 420.0, f0: f, f1: f * 0.97, amp: 0.20, attack_ms: 4.0, tau_ms: 120.0, noise: 0.12, cutoff: 900.0, ..Note::default() }, false);
                }
            }
            Event::Attach => self.pair(440.0, 659.3),
            Event::Outcome { ok } => {
                // A script running fifty commands is one event to the ear, not fifty.
                if self.last_outcome.is_some_and(|t| self.clock < t + self.ms(400.0)) {
                    self.dropped += 1;
                    return;
                }
                self.last_outcome = Some(self.clock);
                let (a, b, rough) = if ok { (523.3, 784.0, 0.0) } else { (392.0, 277.2, 0.18) };
                self.push(Note { dur_ms: 90.0, f0: a, f1: a, amp: 0.13, attack_ms: 2.0, tau_ms: 28.0, noise: rough, cutoff: 1400.0, ..Note::default() }, false);
                self.push(Note { delay_ms: 70.0, dur_ms: 170.0, f0: b, f1: b, amp: 0.13, attack_ms: 2.0, tau_ms: 50.0, noise: rough, cutoff: 1400.0, ..Note::default() }, false);
            }
            Event::NeedsYou => {
                for (delay, dur, tau) in [(0.0, 70.0, 22.0), (95.0, 70.0, 22.0), (190.0, 200.0, 70.0)] {
                    self.push(Note { delay_ms: delay, dur_ms: dur, f0: 698.5, f1: 698.5, amp: 0.15, attack_ms: 2.0, tau_ms: tau, ..Note::default() }, false);
                }
            }        }
    }

    /// Two soft notes, the second after the first.
    fn pair(&mut self, a: f32, b: f32) {
        self.push(Note { dur_ms: 260.0, f0: a, f1: a, amp: 0.16, attack_ms: 3.0, tau_ms: 70.0, ..Note::default() }, false);
        self.push(Note { delay_ms: 95.0, dur_ms: 320.0, f0: b, f1: b, amp: 0.16, attack_ms: 3.0, tau_ms: 90.0, ..Note::default() }, false);
    }

    /// Voice the text gathered so far, unless a grain sounded too recently.
    /// The gap widens while output keeps coming, so a long build log settles
    /// into a slow pulse instead of a buzz.
    fn flush_text(&mut self) {
        if self.pending_cells == 0 {
            return;
        }
        if self.last_text.is_some_and(|t| self.clock < t + self.text_gap) {
            return;
        }
        let (base, widest) = (self.ms(45.0), self.ms(400.0));
        self.text_gap = match self.last_text {
            Some(t) if self.clock - t < self.ms(600.0) => (self.text_gap * 5 / 4).clamp(base, widest),
            _ => base,
        };
        self.last_text = Some(self.clock);
        // More text is louder and lower: a word is a light tap, a page a thud.
        let size = ((1.0 + self.pending_cells as f32).ln() / 401f32.ln()).min(1.0);
        let bulk = self.pending_bulk;
        let f = if bulk { 250.0 } else { 880.0 - 520.0 * size };
        let amp = (0.05 + 0.10 * size) * if bulk { 0.6 } else { 1.0 };
        let pan = self.pending_pan * 0.6;
        // Colour: -1 when all of it was red, 1 when all of it was green.
        let tone = (self.pending_green as f32 - self.pending_red as f32) / self.pending_cells as f32;
        self.pending_cells = 0;
        self.pending_red = 0;
        self.pending_green = 0;
        self.pending_bulk = false;
        self.push(
            Note { dur_ms: 70.0 + 80.0 * size, f0: f, f1: f * 0.92, amp, attack_ms: 1.5, tau_ms: 22.0 + 30.0 * size, noise: 0.25, cutoff: 2400.0, pan, ..Note::default() },
            true,
        );
        if tone <= -0.25 {
            // Red text gets a rough partner an octave down and a semitone
            // off: it beats against the grain, the way a failure should.
            let (lo, weight) = (f * 0.5 * 1.06, (-tone).min(1.0));
            self.push(
                Note { dur_ms: 190.0, f0: lo, f1: lo * 0.94, amp: amp * weight * 1.1, attack_ms: 2.0, tau_ms: 60.0, noise: 0.3, cutoff: 1200.0, pan, ..Note::default() },
                true,
            );
        } else if tone >= 0.25 {
            // Green text gets a clean fifth above.
            self.push(
                Note { dur_ms: 150.0, f0: f * 1.5, f1: f * 1.5, amp: amp * tone.min(1.0) * 0.6, attack_ms: 2.0, tau_ms: 45.0, pan, ..Note::default() },
                true,
            );
        }
    }

    /// Fill `out` (interleaved, `channels` per frame) with the next samples.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        out.fill(0.0);
        let channels = channels.max(1);
        let frames = out.len() / channels;
        let mut done = 0;
        while done < frames {
            self.flush_text();
            let n = (frames - done).min(256);
            if !self.voices.is_empty() {
                self.block(&mut out[done * channels..(done + n) * channels], channels, n);
            }
            done += n;
            self.clock += n as u64;
        }
    }

    fn block(&mut self, out: &mut [f32], channels: usize, n: usize) {
        let mut mix = [0.0f32; 512];
        let tau = std::f32::consts::TAU;
        let fade = (0.005 * self.rate).max(1.0);
        let mut voices = std::mem::take(&mut self.voices);
        for v in voices.iter_mut() {
            for i in 0..n {
                if v.delay > 0 {
                    v.delay -= 1;
                    continue;
                }
                if v.age >= v.dur {
                    break;
                }
                let a = v.age as f32;
                let f = v.f0 + (v.f1 - v.f0) * (a / v.dur as f32);
                v.phase = (v.phase + tau * f / self.rate) % tau;
                v.lp += v.lp_k * (self.noise() - v.lp);
                // Rise, decay, and a short fade at the very end: no clicks.
                let shape = (a / v.attack).min(1.0) * v.env * ((v.dur - v.age) as f32 / fade).min(1.0);
                let s = v.amp * shape * ((1.0 - v.noise) * v.phase.sin() + v.noise * v.lp * 3.0);
                v.env *= v.decay;
                v.age += 1;
                mix[i * 2] += s * v.gl;
                mix[i * 2 + 1] += s * v.gr;
            }
        }
        voices.retain(|v| v.age < v.dur);
        self.voices = voices;
        for i in 0..n {
            // tanh keeps any pile-up of voices below full scale.
            let l = (mix[i * 2] * self.volume * 1.6).tanh();
            let r = (mix[i * 2 + 1] * self.volume * 1.6).tanh();
            if channels == 1 {
                out[i] = (l + r) * 0.5;
            } else {
                out[i * channels] = l;
                out[i * channels + 1] = r;
            }
        }
    }
}

/// Loudness of a stretch of samples: (root mean square, peak).
pub fn level(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    ((sum / samples.len() as f32).sqrt(), peak)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn run(s: &mut Synth, secs: f32) -> Vec<f32> {
        let mut out = vec![0.0f32; (secs * RATE as f32) as usize * 2];
        for chunk in out.chunks_mut(480 * 2) {
            s.render(chunk, 2);
        }
        out
    }

    #[test]
    fn nothing_happening_is_exact_silence() {
        let mut s = Synth::new(RATE, 1.0);
        assert!(s.silent());
        assert_eq!(level(&run(&mut s, 0.5)), (0.0, 0.0));
        // A change in place (a spinner turning) lands in no empty cell.
        s.trigger(Event::Text { cells: 0, red: 0, green: 0, bulk: false, pan: 0.0 });
        assert!(s.silent());
        assert_eq!(level(&run(&mut s, 0.2)).1, 0.0);
    }

    #[test]
    fn every_event_is_heard_then_ends() {
        for e in [
            Event::Key,
            Event::Text { cells: 12, red: 0, green: 0, bulk: false, pan: -0.5 },
            Event::Text { cells: 4000, red: 4000, green: 0, bulk: true, pan: 0.0 },
            Event::Bell,
            Event::Exit(Some(0)),
            Event::Exit(Some(1)),
            Event::Exit(None),
            Event::Attach,
            Event::Outcome { ok: true },
            Event::Outcome { ok: false },
            Event::NeedsYou,
        ] {
            let mut s = Synth::new(RATE, 0.5);
            s.trigger(e);
            let (rms, peak) = level(&run(&mut s, 0.02));
            assert!(rms > 0.0005 && peak > 0.004, "{e:?} too quiet: rms {rms} peak {peak}");
            let (_, peak) = level(&run(&mut s, 1.0));
            assert!(peak < 0.9, "{e:?} clips: {peak}");
            assert!(s.silent(), "{e:?} never ends");
            assert_eq!(level(&run(&mut s, 0.1)).1, 0.0, "{e:?} leaves a residue");
        }
    }

    #[test]
    fn a_flood_is_a_slow_pulse_and_never_clips() {
        let mut s = Synth::new(RATE, 1.0);
        let mut peak = 0.0f32;
        // Three seconds of a screenful every 5 ms, with keys and bells on top.
        for i in 0..600 {
            s.trigger(Event::Text { cells: 3000, red: 0, green: 0, bulk: true, pan: 0.0 });
            s.trigger(Event::Key);
            if i % 10 == 0 {
                s.trigger(Event::Bell);
            }
            assert!(s.voices.len() <= MAX_VOICES);
            let mut buf = vec![0.0f32; 240 * 2];
            s.render(&mut buf, 2);
            peak = peak.max(level(&buf).1);
        }
        assert!(peak < 1.0, "peak {peak}");
        assert!(s.dropped > 0);
        // 600 arrivals became a few grains: the gap widened to 400 ms.
        let mut t = Synth::new(RATE, 1.0);
        for _ in 0..600 {
            t.trigger(Event::Text { cells: 3000, red: 0, green: 0, bulk: true, pan: 0.0 });
            t.render(&mut vec![0.0f32; 240 * 2], 2);
        }
        assert!((8..=20).contains(&t.started), "{} grains in 3 s", t.started);
    }

    /// Energy near one frequency (Goertzel), to tell low sound from high.
    fn energy_at(samples: &[f32], hz: f32) -> f32 {
        let w = std::f32::consts::TAU * hz / RATE as f32;
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for x in samples.iter().step_by(2) {
            let s0 = x + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).sqrt() / samples.len() as f32
    }

    #[test]
    fn red_text_sounds_lower_and_green_higher_than_plain() {
        let hear = |red: u32, green: u32| {
            let mut s = Synth::new(RATE, 0.5);
            s.trigger(Event::Text { cells: 80, red, green, bulk: false, pan: 0.0 });
            (run(&mut s, 0.3), s.started)
        };
        let (plain, n_plain) = hear(0, 0);
        let (red, n_red) = hear(80, 0);
        let (green, n_green) = hear(0, 80);
        // 80 cells: the grain starts at 499 Hz; red adds a note gliding down
        // from 264 Hz, green one at 748 Hz.
        assert_eq!((n_plain, n_red, n_green), (1, 2, 2));
        assert!(energy_at(&red, 256.0) > 4.0 * energy_at(&plain, 256.0));
        assert!(energy_at(&green, 748.0) > 4.0 * energy_at(&plain, 748.0));
        // Half red, half green is neither; a few coloured cells in plenty are not enough.
        assert_eq!(hear(40, 40).1, 1);
        assert_eq!(hear(10, 0).1, 1);
    }

    /// Success rises and failure falls; every outcome and attention sound is
    /// over within 400 ms; a burst of outcomes is one sound.
    #[test]
    fn outcomes_differ_by_contour_are_short_and_do_not_pile_up() {
        let hear = |e: Event| {
            let mut s = Synth::new(RATE, 0.5);
            s.trigger(e);
            let first = run(&mut s, 0.07);
            let rest = run(&mut s, 0.33);
            assert!(s.silent(), "{e:?} lasts longer than 400 ms");
            (first, rest)
        };
        let (ok_a, ok_b) = hear(Event::Outcome { ok: true });
        let (bad_a, bad_b) = hear(Event::Outcome { ok: false });
        assert!(energy_at(&ok_a, 523.3) > 3.0 * energy_at(&ok_a, 784.0) && energy_at(&ok_b, 784.0) > 3.0 * energy_at(&ok_b, 523.3), "success rises");
        assert!(energy_at(&bad_a, 392.0) > 3.0 * energy_at(&bad_a, 277.2) && energy_at(&bad_b, 277.2) > 3.0 * energy_at(&bad_b, 392.0), "failure falls");
        hear(Event::NeedsYou);
        let mut s = Synth::new(RATE, 0.5);
        for _ in 0..50 {
            s.trigger(Event::Outcome { ok: true });
        }
        assert_eq!(s.started, 2);
    }

    #[test]
    fn held_key_does_not_buzz() {
        let mut s = Synth::new(RATE, 1.0);
        for _ in 0..50 {
            s.trigger(Event::Key);
        }
        assert_eq!(s.started, 1);
    }

    #[test]
    fn volume_scales_and_zero_is_silent() {
        let loud = |v: f32| {
            let mut s = Synth::new(RATE, v);
            s.trigger(Event::Bell);
            level(&run(&mut s, 0.2)).0
        };
        assert_eq!(loud(0.0), 0.0);
        assert!(loud(0.2) < loud(0.6));
    }

    #[test]
    fn mono_and_surround_devices_get_sound_in_the_right_channels() {
        let mut s = Synth::new(RATE, 0.5);
        s.trigger(Event::Bell);
        let mut mono = vec![0.0f32; 480];
        s.render(&mut mono, 1);
        assert!(level(&mono).1 > 0.0);
        let mut s = Synth::new(RATE, 0.5);
        s.trigger(Event::Bell);
        let mut six = vec![0.0f32; 480 * 6];
        s.render(&mut six, 6);
        for f in six.chunks(6) {
            assert_eq!(&f[2..], &[0.0; 4]);
        }
        assert!(six.chunks(6).any(|f| f[0] != 0.0 && f[1] != 0.0));
    }
}
