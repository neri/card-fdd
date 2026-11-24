//! Hardware layer.
//!
//! Supports the Cardputer v1.1 ([`v1`]) and the Cardputer-Adv ([`adv`]). The model is detected at
//! startup ([`detect`]); the cargo features `board-v1` / `board-adv` force one of them.
//! Parts that differ per model (the keyboard, and the codec setup before audio starts) live in the
//! per-model modules. The LCD, SD, LED, battery, I2S, and USB are the same on both.
//!
//! Pin assignment:
//!
//! | Function   | v1.1                                                 | Adv                                       |
//! |------------|------------------------------------------------------|-------------------------------------------|
//! | LCD SPI2   | MOSI G35, SCK G36, CS G37, DC G34, RST G33, BL G38   | same                                      |
//! | SD SPI3    | SCK G40, MISO G39, MOSI G14, CS G12                  | same                                      |
//! | Keyboard   | Rows G8, G9, G11 / Cols G13, G15, G3, G4, G5, G6, G7 | TCA8418 (I2C 0x34): SDA G8, SCL G9, INT G11 |
//! | I2S        | NS4168: BCLK G41, LRCK G43, DATA G42                 | ES8311 (I2C 0x18): same pins, no MCLK     |
//! | RGB LED    | G21 (power from G38)                                 | same                                      |
//! | Battery    | G10 (1/2 voltage divider)                            | same                                      |
//! | Grove UART | TX G2, RX G1 (log output)                            | same                                      |
//! | USB OTG    | D- G19, D+ G20                                       | same                                      |

use core::sync::atomic::{AtomicU8, Ordering};

use esp_hal::delay::Delay;
use esp_hal::gpio::{DriveMode, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::peripherals::{GPIO8, GPIO9, I2C0};
use esp_hal::time::Rate;

use crate::input::KeyEvent;

pub mod adv;
pub mod audio;
pub mod battery;
pub mod display;
pub mod led;
pub mod sdcard;
pub mod system;
pub mod v1;

pub use battery::AdcBattery;

#[cfg(all(feature = "board-v1", feature = "board-adv"))]
compile_error!("Enable at most one of the features `board-v1` and `board-adv`");

/// Hardware model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// Cardputer v1.0 / v1.1.
    V1,
    /// Cardputer-Adv.
    Adv,
}

impl Model {
    pub const fn name(self) -> &'static str {
        match self {
            Model::V1 => "Cardputer v1.1",
            Model::Adv => "Cardputer-Adv",
        }
    }
}

static MODEL: AtomicU8 = AtomicU8::new(0);

/// The model detected at startup (v1.1 before [`set_model`] is called).
pub fn model() -> Model {
    match MODEL.load(Ordering::Relaxed) {
        1 => Model::Adv,
        _ => Model::V1,
    }
}

pub fn set_model(model: Model) {
    MODEL.store(model as u8, Ordering::Relaxed);
}

/// Detect the model by checking whether the Adv keyboard controller (TCA8418) answers on G8 / G9.
///
/// Call this before initializing the keyboard. On v1.1, G8 / G9 only drive the 74HC138 address
/// inputs, so I2C traffic there is harmless. G11 is not touched: it is an output on v1.1 but the
/// TCA8418 interrupt output (open drain) on Adv, so it must not be driven until the model is known.
pub fn detect(mut i2c: I2C0<'_>, mut sda: GPIO8<'_>, mut scl: GPIO9<'_>) -> Model {
    if cfg!(feature = "board-v1") {
        return Model::V1;
    }
    if cfg!(feature = "board-adv") {
        return Model::Adv;
    }

    recover_bus(scl.reborrow());

    let config = I2cConfig::default().with_frequency(Rate::from_khz(100));
    let Ok(bus) = I2c::new(i2c.reborrow(), config) else {
        log::warn!("Cannot configure I2C for model detection; assuming v1.1");
        return Model::V1;
    };
    let mut bus = bus.with_sda(sda.reborrow()).with_scl(scl.reborrow());
    let delay = Delay::new();
    for _ in 0..3 {
        if adv::keyboard::probe(&mut bus) {
            return Model::Adv;
        }
        delay.delay_millis(1);
    }
    Model::V1
}

/// Clock SCL 9 times so that a device left in the middle of a transfer (e.g. by a reset during
/// I2C traffic) releases SDA.
fn recover_bus(scl: GPIO9<'_>) {
    let config = OutputConfig::default()
        .with_drive_mode(DriveMode::OpenDrain)
        .with_pull(Pull::Up);
    let mut scl = Output::new(scl, Level::High, config);
    let delay = Delay::new();
    for _ in 0..9 {
        scl.set_low();
        delay.delay_micros(5);
        scl.set_high();
        delay.delay_micros(5);
    }
}

/// Keyboard.
#[allow(
    async_fn_in_trait,
    reason = "Only used for static dispatch on a single-threaded executor, so no Send bound is needed"
)]
pub trait Keyboard {
    /// Wait for the next key event (press or release).
    async fn next_event(&mut self) -> KeyEvent;
}

/// The keyboard of the detected model.
pub enum AnyKeyboard {
    V1(v1::MatrixKeyboard),
    Adv(adv::Tca8418Keyboard),
}

impl Keyboard for AnyKeyboard {
    async fn next_event(&mut self) -> KeyEvent {
        match self {
            AnyKeyboard::V1(k) => k.next_event().await,
            AnyKeyboard::Adv(k) => k.next_event().await,
        }
    }
}

/// Battery.
pub trait Battery {
    /// Battery voltage [mV].
    fn millivolts(&mut self) -> u16;

    /// Approximate battery level [%].
    fn percent(&mut self) -> u8 {
        percent_from_millivolts(self.millivolts())
    }
}

/// Estimate the remaining charge from a single Li-ion cell voltage (linear approximation: 3.30V = 0%, 4.15V = 100%).
pub fn percent_from_millivolts(mv: u16) -> u8 {
    const EMPTY: u32 = 3300;
    const FULL: u32 = 4150;
    let mv = u32::from(mv).clamp(EMPTY, FULL);
    ((mv - EMPTY) * 100 / (FULL - EMPTY)) as u8
}
