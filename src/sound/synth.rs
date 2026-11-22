//! Floppy drive sound synthesis (hardware independent).
//!
//! Uses no recorded samples; sounds are made by passing impulses and noise through resonators (2nd-order IIR) (plan Q10).
//! Prioritizes a 3.5-inch FDD feel (avoids HDD-like high-pitched ticks and motor hum).
//!
//! - Step sound: a low "thock". Layers a short ring around 450Hz with a body resonance around 100Hz.
//!   During a seek they repeat at 3ms intervals and sound like a "brrr"
//! - Motor sound: no hum; plays a very quiet, low (around 180Hz) disk rubbing sound.
//!   1 Its level is modulated once per revolution (5Hz). Stops 2 seconds after the last access
//! - Insert / eject: instead of reproducing mechanical sounds, use beeps (two rising notes on insert, two falling on eject)
//!
//! Adjust pitch and loudness with the constants below.

use super::{Event, SAMPLE_RATE};

// ---- Tuning constants ----

/// Pitch [Hz] and decay (closer to 1 rings longer) of the step sound's mid-range ring.
const CLICK_FREQ: f32 = 450.0;
const CLICK_DAMP: f32 = 0.93;
const CLICK_LEVEL: f32 = 0.10;
/// Body resonance of the step sound (the low "thock").
const BODY_FREQ: f32 = 100.0;
const BODY_DAMP: f32 = 0.985;
const BODY_LEVEL: f32 = 0.022;
/// Amount of noise mixed into the step sound.
const CLICK_NOISE: f32 = 0.3;

/// Center pitch [Hz], decay, and level of the disk rubbing sound.
const RUSTLE_FREQ: f32 = 180.0;
const RUSTLE_DAMP: f32 = 0.92;
const RUSTLE_LEVEL: f32 = 0.03;

/// Level of the beeps (insert / eject) and the length of each note.
const BEEP_LEVEL: f32 = 0.25;
const BEEP_NOTE: u32 = SAMPLE_RATE * 60 / 1000;
/// Pitches [Hz] of the insert / eject beeps (C6, G6).
const BEEP_INSERT: [f32; 2] = [1047.0, 1568.0];
const BEEP_EJECT: [f32; 2] = [1568.0, 1047.0];

/// Step interval (3ms).
const STEP_INTERVAL: u32 = SAMPLE_RATE * 3 / 1000;
/// Settling time of the head after a seek (15ms). No new step starts during this time.
const SETTLE: u32 = SAMPLE_RATE * 15 / 1000;
/// Time from the last access until the motor stops (2 seconds).
const MOTOR_OFF_AFTER: u32 = SAMPLE_RATE * 2;
/// Maximum number of steps moved at once.
const MAX_STEPS: u16 = 84;

/// Final amplitude for each volume level (relative to i16 full scale).
const GAIN: [f32; 4] = [0.0, 2500.0, 6000.0, 14000.0];

/// 2nd-order resonator: y[n] = a1 y[n-1] + a2 y[n-2] + x[n].
struct Resonator {
    a1: f32,
    a2: f32,
    y1: f32,
    y2: f32,
}

impl Resonator {
    fn new(freq: f32, damp: f32) -> Self {
        let w = 2.0 * core::f32::consts::PI * freq / SAMPLE_RATE as f32;
        Self {
            a1: 2.0 * damp * cos_approx(w),
            a2: -damp * damp,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.a1 * self.y1 + self.a2 * self.y2 + x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

pub struct Synth {
    rng: u32,
    /// Head position and target.
    head: u16,
    target: u16,
    step_countdown: u32,
    settle: u32,
    /// Step sound: impulse to apply on the next sample, and the noise envelope.
    impulse: f32,
    click_env: f32,
    click: Resonator,
    body: Resonator,
    /// Motor (disk rubbing sound).
    motor: f32,
    motor_idle: u32,
    rotation_phase: f32,
    rustle: Resonator,
    /// Beeps: sequence of notes, current note index, remaining samples, phase.
    beep_notes: [f32; 2],
    beep_index: usize,
    beep_left: u32,
    beep_phase: f32,
}

impl Default for Synth {
    fn default() -> Self {
        Self::new()
    }
}

impl Synth {
    pub fn new() -> Self {
        Self {
            rng: 0x1234_5678,
            head: 0,
            target: 0,
            step_countdown: 0,
            settle: 0,
            impulse: 0.0,
            click_env: 0.0,
            click: Resonator::new(CLICK_FREQ, CLICK_DAMP),
            body: Resonator::new(BODY_FREQ, BODY_DAMP),
            motor: 0.0,
            motor_idle: MOTOR_OFF_AFTER,
            rotation_phase: 0.0,
            rustle: Resonator::new(RUSTLE_FREQ, RUSTLE_DAMP),
            beep_notes: BEEP_INSERT,
            beep_index: 2,
            beep_left: 0,
            beep_phase: 0.0,
        }
    }

    pub fn event(&mut self, event: Event) {
        match event {
            Event::Access { cylinder, .. } => {
                self.motor_idle = 0;
                let limit = self.head.saturating_add(MAX_STEPS);
                self.target = cylinder.min(limit);
            }
            Event::Insert => self.start_beep(BEEP_INSERT),
            Event::Eject => {
                self.start_beep(BEEP_EJECT);
                self.motor_idle = MOTOR_OFF_AFTER;
            }
        }
    }

    fn start_beep(&mut self, notes: [f32; 2]) {
        self.beep_notes = notes;
        self.beep_index = 0;
        self.beep_left = BEEP_NOTE;
        self.beep_phase = 0.0;
    }

    /// Play one step sound.
    fn step(&mut self) {
        self.impulse = 1.0;
        self.click_env = 1.0;
    }

    fn noise(&mut self) -> f32 {
        // xorshift32
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x as i32) as f32 / i32::MAX as f32
    }

    /// Fill `out` with mono samples. `volume` is 0 (mute) to 3.
    pub fn fill(&mut self, out: &mut [i16], volume: u8) {
        let gain = GAIN[usize::from(volume.min(3))];
        for s in out.iter_mut() {
            let v = self.next_sample();
            *s = (v * gain).clamp(-32767.0, 32767.0) as i16;
        }
    }

    fn next_sample(&mut self) -> f32 {
        // --- Step (seek)
        if self.settle > 0 {
            self.settle -= 1;
        } else if self.head != self.target {
            if self.step_countdown == 0 {
                if self.head < self.target {
                    self.head += 1;
                } else {
                    self.head -= 1;
                }
                self.step();
                self.step_countdown = STEP_INTERVAL;
                if self.head == self.target {
                    self.settle = SETTLE;
                }
            } else {
                self.step_countdown -= 1;
            }
        }

        let excite = self.impulse + CLICK_NOISE * self.noise() * self.click_env;
        self.impulse = 0.0;
        self.click_env *= 0.7;
        let click = self.click.process(excite) * CLICK_LEVEL;
        let body = self.body.process(excite) * BODY_LEVEL;

        // --- Motor (disk rubbing sound)
        self.motor_idle = self.motor_idle.saturating_add(1);
        let motor_target = if self.motor_idle < MOTOR_OFF_AFTER { 1.0 } else { 0.0 };
        // Ramps up in about 0.2 s and stops in about 0.4 s
        let rate = if motor_target > self.motor { 0.0003 } else { 0.00015 };
        self.motor += (motor_target - self.motor) * rate;
        let rustle = if self.motor > 0.001 {
            // Level modulation per revolution (300rpm = 5Hz)
            self.rotation_phase += 5.0 / SAMPLE_RATE as f32;
            if self.rotation_phase >= 1.0 {
                self.rotation_phase -= 1.0;
            }
            let swell = 0.6 + 0.4 * (4.0 * (self.rotation_phase - 0.5).abs() - 1.0);
            let n = self.noise() * (1.0 - RUSTLE_DAMP);
            self.motor * swell * self.rustle.process(n) * RUSTLE_LEVEL
        } else {
            0.0
        };

        // --- Beeps (triangle wave; short fades at note start and end prevent clicks)
        let mut beep = 0.0;
        if self.beep_index < self.beep_notes.len() {
            self.beep_phase += self.beep_notes[self.beep_index] / SAMPLE_RATE as f32;
            if self.beep_phase >= 1.0 {
                self.beep_phase -= 1.0;
            }
            let wave = 4.0 * (self.beep_phase - 0.5).abs() - 1.0;
            let pos = BEEP_NOTE - self.beep_left;
            let fade = (pos.min(self.beep_left) as f32 / 80.0).min(1.0);
            beep = wave * fade * BEEP_LEVEL;
            self.beep_left -= 1;
            if self.beep_left == 0 {
                self.beep_index += 1;
                self.beep_left = BEEP_NOTE;
            }
        }

        click + body + rustle + beep
    }
}

/// Approximation of cos via Taylor expansion (accurate enough for |x| <= π). Needed because no_std has no cos.
fn cos_approx(x: f32) -> f32 {
    let x2 = x * x;
    1.0 - x2 / 2.0 + x2 * x2 / 24.0 - x2 * x2 * x2 / 720.0 + x2 * x2 * x2 * x2 / 40320.0
}
