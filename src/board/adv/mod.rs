//! Implementation specific to Cardputer-Adv.
//!
//! The keyboard controller (TCA8418), the audio codec (ES8311), and the IMU (BMI270, unused) share
//! one I2C bus on G8 (SDA) / G9 (SCL). The codec is configured once at startup ([`codec::init`]),
//! after which the bus is handed to the keyboard driver for good.
//!
//! Reference: Cardputer-Adv schematic v1.0 (2025-06-20).

use esp_hal::Blocking;
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::peripherals::{GPIO8, GPIO9, I2C0};
use esp_hal::time::Rate;

pub mod codec;
pub mod keyboard;

pub use keyboard::Tca8418Keyboard;

pub type Bus = I2c<'static, Blocking>;

/// Bring up the shared I2C bus (400kHz, the same as the official M5Stack library).
pub fn i2c_bus(i2c: I2C0<'static>, sda: GPIO8<'static>, scl: GPIO9<'static>) -> Bus {
    let config = I2cConfig::default().with_frequency(Rate::from_khz(400));
    I2c::new(i2c, config)
        .expect("Failed to configure I2C0")
        .with_sda(sda)
        .with_scl(scl)
}
