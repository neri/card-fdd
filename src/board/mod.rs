//! Hardware layer.
//!
//! Parts that differ per model (keyboard, battery, and audio in the future) are abstracted with traits,
//! with implementations in per-model modules (currently only [`v1`]). The LCD and LED are shared.
//!
//! Pin assignment (v1.1):
//!
//! | Function   | Pins                                                   |
//! |------------|--------------------------------------------------------|
//! | LCD SPI2   | MOSI G35, SCK G36, CS G37, DC G34, RST G33, BL G38     |
//! | SD SPI3    | SCK G40, MISO G39, MOSI G14, CS G12                    |
//! | Keyboard   | Rows G8, G9, G11 / Cols G13, G15, G3, G4, G5, G6, G7   |
//! | RGB LED    | G21                                                    |
//! | Battery    | G10 (1/2 voltage divider)                              |
//! | Grove UART | TX G2, RX G1 (log output)                              |
//! | USB OTG    | D- G19, D+ G20                                         |

use crate::input::KeyEvent;

pub mod display;
pub mod led;
pub mod sdcard;
pub mod system;
pub mod v1;

/// Keyboard.
#[allow(
    async_fn_in_trait,
    reason = "Only used for static dispatch on a single-threaded executor, so no Send bound is needed"
)]
pub trait Keyboard {
    /// Wait for the next key event (press or release).
    async fn next_event(&mut self) -> KeyEvent;
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
