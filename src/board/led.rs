//! Built-in RGB LED (SK6812, G21). Used as an access lamp, etc.

use esp_hal::Blocking;
use esp_hal::peripherals::{GPIO21, RMT};
use esp_hal::rmt::Rmt;
use esp_hal::time::Rate;
use esp_hal_smartled::{RmtSmartLeds, SK68XX_TIMING, buffer_size, color_order};
use smart_leds_trait::{RGB8, SmartLedsWrite};

/// Brightness (0..=255). The LED is quite bright, so keep it low.
const BRIGHTNESS: u8 = 32;

type Driver = RmtSmartLeds<'static, { buffer_size::<RGB8>(1) }, Blocking, RGB8, color_order::Grb>;

pub struct StatusLed {
    driver: Driver,
}

impl StatusLed {
    pub fn new(rmt: RMT<'static>, pin: GPIO21<'static>) -> Self {
        let rmt = Rmt::new(rmt, Rate::from_mhz(80)).expect("Failed to configure RMT");
        let driver = RmtSmartLeds::new(SK68XX_TIMING, rmt.channel0, pin, Rate::from_mhz(80))
            .expect("Failed to configure RGB LED");
        let mut led = Self { driver };
        led.off();
        led
    }

    /// Set the color (dimmed by [`BRIGHTNESS`]).
    pub fn set(&mut self, color: RGB8) {
        let scale = |c: u8| (u16::from(c) * u16::from(BRIGHTNESS) / 255) as u8;
        let color = RGB8::new(scale(color.r), scale(color.g), scale(color.b));
        // A failed write does not affect operation, so ignore it.
        let _ = self.driver.write([color]);
    }

    pub fn off(&mut self) {
        self.set(RGB8::new(0, 0, 0));
    }
}
