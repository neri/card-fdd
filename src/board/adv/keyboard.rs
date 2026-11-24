//! Adv keyboard: TCA8418 keypad scan controller (I2C 0x34, interrupt on G11).
//!
//! The chip scans the 7 x 8 matrix (ROW0..6 x COL0..7), debounces it, and queues press / release
//! events in a 10-entry FIFO. The interrupt line stays low while there are unread events.
//!
//! The mapping from the electrical 7x8 to the physical 4x14 follows the official M5Stack library
//! (M5Cardputer, `TCA8418KeyboardReader::remap`):
//!
//! - Physical row = COL % 4
//! - Physical column = 2 * ROW, plus 1 if COL >= 4
//!
//! Not yet checked on real hardware.

use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::i2c::master::{Error, I2c};
use esp_hal::peripherals::GPIO11;

use super::Bus;
use crate::board::Keyboard;
use crate::input::{self, KeyEvent, KeyPos};

/// 7-bit I2C address.
pub const ADDR: u8 = 0x34;

// Registers
const REG_CFG: u8 = 0x01;
const REG_INT_STAT: u8 = 0x02;
const REG_KEY_EVENT_A: u8 = 0x04;
const REG_KP_GPIO1: u8 = 0x1D;
const REG_KP_GPIO2: u8 = 0x1E;
const REG_KP_GPIO3: u8 = 0x1F;

// CFG bits
const CFG_KE_IEN: u8 = 1 << 0;
const CFG_OVR_FLOW_IEN: u8 = 1 << 3;

// INT_STAT bits (write 1 to clear)
const INT_OVR_FLOW: u8 = 1 << 3;
const INT_ALL: u8 = 0x1F;

const ROWS: u8 = 7;
const COLS: u8 = 8;

/// The FIFO holds at most 10 events.
const FIFO_DEPTH: usize = 10;

/// Check the FIFO at least this often even without an interrupt (in case an edge is missed).
const POLL_PERIOD: Duration = Duration::from_millis(100);

/// Pause after an empty read that followed another empty read, so that an interrupt line stuck low
/// does not turn into a busy loop.
const IDLE_BACKOFF: Duration = Duration::from_millis(5);

/// Pause after an I2C error.
const ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Whether a TCA8418 answers at [`ADDR`] (used for model detection).
pub fn probe<Dm: esp_hal::DriverMode>(bus: &mut I2c<'_, Dm>) -> bool {
    let mut buf = [0u8];
    bus.write_read(ADDR, &[REG_CFG], &mut buf).is_ok()
}

/// Decode a FIFO entry into the physical key position and whether the key was pressed.
///
/// Bit 7 is set for a press. The low 7 bits are the key number `ROW * 10 + COL + 1` (keypad keys
/// 1..=80; 97.. are GPI events, which are not used).
pub const fn decode_event(raw: u8) -> Option<(KeyPos, bool)> {
    let code = raw & 0x7F;
    if code == 0 || code > 80 {
        return None;
    }
    let n = code - 1;
    let (row, col) = (n / 10, n % 10);
    if row >= ROWS || col >= COLS {
        return None;
    }
    let pos = KeyPos {
        row: col % 4,
        col: 2 * row + if col >= 4 { 1 } else { 0 },
    };
    Some((pos, raw & 0x80 != 0))
}

pub struct Tca8418Keyboard {
    bus: Bus,
    int: Input<'static>,
    /// Pressed state.
    stable: u64,
    /// Keys to report as released (after a FIFO overflow).
    release: u64,
    /// The previous FIFO read was empty.
    was_empty: bool,
}

impl Tca8418Keyboard {
    pub fn new(bus: Bus, int: GPIO11<'static>) -> Self {
        // INT has a 3.3k pull-up on the board. The internal one does no harm
        let int = Input::new(int, InputConfig::default().with_pull(Pull::Up));
        let mut kb = Self {
            bus,
            int,
            stable: 0,
            release: 0,
            was_empty: false,
        };
        if let Err(e) = kb.init() {
            log::warn!("TCA8418 init failed: {:?}", e);
        }
        kb
    }

    fn init(&mut self) -> Result<(), Error> {
        self.write_reg(REG_CFG, 0)?;
        // ROW0..6 and COL0..7 as keypad, COL8..9 unused
        self.write_reg(REG_KP_GPIO1, 0x7F)?;
        self.write_reg(REG_KP_GPIO2, 0xFF)?;
        self.write_reg(REG_KP_GPIO3, 0x00)?;
        // Drop anything queued before startup
        for _ in 0..FIFO_DEPTH {
            if self.read_reg(REG_KEY_EVENT_A)? == 0 {
                break;
            }
        }
        self.write_reg(REG_INT_STAT, INT_ALL)?;
        self.write_reg(REG_CFG, CFG_KE_IEN | CFG_OVR_FLOW_IEN)
    }

    fn read_reg(&mut self, reg: u8) -> Result<u8, Error> {
        let mut buf = [0u8];
        self.bus.write_read(ADDR, &[reg], &mut buf)?;
        Ok(buf[0])
    }

    fn write_reg(&mut self, reg: u8, value: u8) -> Result<(), Error> {
        self.bus.write(ADDR, &[reg, value])
    }

    fn event(&self, pos: KeyPos, pressed: bool) -> KeyEvent {
        let mods = input::modifiers_from_bitmap(self.stable);
        KeyEvent {
            pos,
            key: input::resolve(pos, mods),
            pressed,
            mods,
        }
    }
}

impl Keyboard for Tca8418Keyboard {
    async fn next_event(&mut self) -> KeyEvent {
        loop {
            if self.release != 0 {
                let index = self.release.trailing_zeros() as u8;
                self.release &= self.release - 1;
                self.stable &= !(1 << index);
                return self.event(KeyPos::from_index(index), false);
            }

            match self.read_reg(REG_KEY_EVENT_A) {
                Ok(0) => {
                    // FIFO empty: clear the interrupt and wait for the next one
                    let status = self.read_reg(REG_INT_STAT).unwrap_or(0);
                    if status & INT_OVR_FLOW != 0 {
                        // Events were lost, so the pressed state is unknown. Release everything
                        // so that no modifier stays stuck
                        log::warn!("TCA8418 FIFO overflow");
                        self.release = self.stable;
                    }
                    let _ = self.write_reg(REG_INT_STAT, INT_ALL);
                    if self.release != 0 {
                        continue;
                    }
                    if self.was_empty {
                        Timer::after(IDLE_BACKOFF).await;
                    }
                    self.was_empty = true;
                    let _ = with_timeout(POLL_PERIOD, self.int.wait_for_low()).await;
                }
                Ok(raw) => {
                    self.was_empty = false;
                    log::debug!("TCA8418 event {:#04x}", raw);
                    let Some((pos, pressed)) = decode_event(raw) else {
                        continue;
                    };
                    let bit = 1u64 << pos.index();
                    let was_pressed = self.stable & bit != 0;
                    if was_pressed == pressed {
                        continue;
                    }
                    if pressed {
                        self.stable |= bit;
                    } else {
                        self.stable &= !bit;
                    }
                    return self.event(pos, pressed);
                }
                Err(e) => {
                    log::warn!("TCA8418 read failed: {:?}", e);
                    self.was_empty = false;
                    Timer::after(ERROR_BACKOFF).await;
                }
            }
        }
    }
}
