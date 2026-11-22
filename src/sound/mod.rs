//! Floppy drive sounds (plan section 3.6, optional features O1 / O3).
//!
//! Receives USB reads/writes (cylinder moves), insertion, and ejection as events,
//! and [`synth::Synth`] synthesizes step sounds, motor sounds, etc. Audio output is done by the board (I2S).

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

pub mod synth;

/// Sampling rate [Hz].
pub const SAMPLE_RATE: u32 = 16_000;

/// Sound triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Move the head to `cylinder` and read/write.
    Access { cylinder: u16, write: bool },
    /// A disk was inserted.
    Insert,
    /// A disk was ejected.
    Eject,
}

static EVENTS: Channel<CriticalSectionRawMutex, Event, 16> = Channel::new();

/// Volume (0 = mute .. 3).
static VOLUME: AtomicU8 = AtomicU8::new(2);
/// Realistic speed mode (O3).
static SPEED_EMULATION: AtomicBool = AtomicBool::new(false);

pub const MAX_VOLUME: u8 = 3;

/// Send a sound event. Dropped on overflow (only a bit of sound is lost).
pub fn post(event: Event) {
    let _ = EVENTS.try_send(event);
}

/// Take one pending event.
pub fn take_event() -> Option<Event> {
    EVENTS.try_receive().ok()
}

pub fn volume() -> u8 {
    VOLUME.load(Ordering::Relaxed)
}

pub fn set_volume(volume: u8) {
    VOLUME.store(volume.min(MAX_VOLUME), Ordering::Relaxed);
}

pub fn speed_emulation() -> bool {
    SPEED_EMULATION.load(Ordering::Relaxed)
}

pub fn set_speed_emulation(on: bool) {
    SPEED_EMULATION.store(on, Ordering::Relaxed);
}
