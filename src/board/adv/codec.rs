//! Adv audio codec: ES8311 (I2C 0x18, CE tied low), feeding an NS4150B speaker amplifier.
//!
//! MCLK is not wired, so the codec derives its clock from BCLK. With 16-bit stereo I2S, BCLK is
//! 32 x fs, and the codec's pre-multiplier (x8) brings that to the 256 x fs it needs, whatever the
//! sample rate is.
//!
//! The amplifier needs no GPIO: its enable input is pulled up on the board and pulled low by the
//! headphone jack's detect switch, so plugging in headphones mutes the speaker.
//!
//! The register values are the ones the official M5Stack library (M5Unified, MIT) uses to enable the
//! speaker on the Cardputer-Adv. Not yet checked on real hardware.

use esp_hal::i2c::master::Error;

use super::Bus;

/// 7-bit I2C address.
pub const ADDR: u8 = 0x18;

/// Chip ID registers (expected 0x83, 0x11).
const REG_CHIP_ID1: u8 = 0xFD;
const REG_CHIP_ID2: u8 = 0xFE;

/// (register, value) pairs that enable DAC playback.
const SPEAKER_ON: [(u8, u8); 8] = [
    (0x00, 0x80), // RESET: CSM power on
    (0x01, 0xB5), // CLOCK_MANAGER: MCLK from BCLK
    (0x02, 0x18), // CLOCK_MANAGER: MULT_PRE = 3 (x8)
    (0x0D, 0x01), // SYSTEM: power up the analog circuitry
    (0x12, 0x00), // SYSTEM: power up the DAC
    (0x13, 0x10), // SYSTEM: enable the output to the HP drive
    (0x32, 0xBF), // DAC: volume 0dB
    (0x37, 0x08), // DAC: bypass the equalizer
];

/// Configure the codec for playback. Call once at startup, before handing the bus to the keyboard.
pub fn init(bus: &mut Bus) -> Result<(), Error> {
    let mut id = [0u8; 2];
    bus.write_read(ADDR, &[REG_CHIP_ID1], &mut id[..1])?;
    bus.write_read(ADDR, &[REG_CHIP_ID2], &mut id[1..])?;
    log::info!("ES8311 chip ID {:02x} {:02x}", id[0], id[1]);

    for (reg, value) in SPEAKER_ON {
        bus.write(ADDR, &[reg, value])?;
    }
    Ok(())
}
