//! v1.1 keyboard: a matrix that selects one of 8 row lines with a 74HC138 and reads 7 column lines.
//!
//! Selecting one of 0..8 with the 3 address lines (G8, G9, G11) pulls low the column lines
//! (G13, G15, G3..G7, pulled up) of the pressed keys in that row. The mapping from the electrical 8x7 to the physical 4x14
//! follows the official M5Stack library (M5Cardputer):
//!
//! - Physical row = 3 - (address % 4)
//! - Physical column = 2 * input index if address >= 4, otherwise 2 * input index + 1

use embassy_time::{Duration, Ticker};
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::peripherals::{GPIO3, GPIO4, GPIO5, GPIO6, GPIO7, GPIO8, GPIO9, GPIO11, GPIO13, GPIO15};

use crate::board::Keyboard;
use crate::input::{self, KeyEvent, KeyPos};

/// Scan period. A state is accepted after two identical consecutive scans (debouncing), so
/// the response delay is at most about 2 periods.
const SCAN_PERIOD: Duration = Duration::from_millis(5);

/// Wait time between switching the address lines and reading the column lines.
const SETTLE_US: u32 = 2;

pub struct KeyboardPins {
    pub a0: GPIO8<'static>,
    pub a1: GPIO9<'static>,
    pub a2: GPIO11<'static>,
    pub in0: GPIO13<'static>,
    pub in1: GPIO15<'static>,
    pub in2: GPIO3<'static>,
    pub in3: GPIO4<'static>,
    pub in4: GPIO5<'static>,
    pub in5: GPIO6<'static>,
    pub in6: GPIO7<'static>,
}

pub struct MatrixKeyboard {
    addr: [Output<'static>; 3],
    inputs: [Input<'static>; 7],
    delay: Delay,
    ticker: Ticker,
    /// Previous scan result (unconfirmed).
    last_raw: u64,
    /// Confirmed pressed state.
    stable: u64,
    /// Changed bits not yet returned as events.
    pending: u64,
}

impl MatrixKeyboard {
    pub fn new(pins: KeyboardPins) -> Self {
        let out_cfg = OutputConfig::default();
        let cfg = InputConfig::default().with_pull(Pull::Up);
        Self {
            addr: [
                Output::new(pins.a0, Level::Low, out_cfg),
                Output::new(pins.a1, Level::Low, out_cfg),
                Output::new(pins.a2, Level::Low, out_cfg),
            ],
            inputs: [
                Input::new(pins.in0, cfg),
                Input::new(pins.in1, cfg),
                Input::new(pins.in2, cfg),
                Input::new(pins.in3, cfg),
                Input::new(pins.in4, cfg),
                Input::new(pins.in5, cfg),
                Input::new(pins.in6, cfg),
            ],
            delay: Delay::new(),
            ticker: Ticker::every(SCAN_PERIOD),
            last_raw: 0,
            stable: 0,
            pending: 0,
        }
    }

    /// Scan all keys once and return a bitmap of pressed keys.
    fn scan(&mut self) -> u64 {
        let mut bits = 0u64;
        for addr in 0..8u8 {
            for (i, pin) in self.addr.iter_mut().enumerate() {
                pin.set_level(Level::from(addr & (1 << i) != 0));
            }
            self.delay.delay_micros(SETTLE_US);
            for (j, pin) in self.inputs.iter().enumerate() {
                if pin.is_low() {
                    let row = 3 - (addr % 4);
                    let col = if addr >= 4 { 2 * j } else { 2 * j + 1 } as u8;
                    bits |= 1 << KeyPos { row, col }.index();
                }
            }
        }
        bits
    }
}

impl Keyboard for MatrixKeyboard {
    async fn next_event(&mut self) -> KeyEvent {
        loop {
            if self.pending != 0 {
                let index = self.pending.trailing_zeros() as u8;
                self.pending &= self.pending - 1;
                let pos = KeyPos::from_index(index);
                let mods = input::modifiers_from_bitmap(self.stable);
                return KeyEvent {
                    pos,
                    key: input::resolve(pos, mods),
                    pressed: self.stable & (1 << index) != 0,
                    mods,
                };
            }

            self.ticker.next().await;
            let raw = self.scan();
            if raw == self.last_raw {
                self.pending = raw ^ self.stable;
                self.stable = raw;
            }
            self.last_raw = raw;
        }
    }
}
