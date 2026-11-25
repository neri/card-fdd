//! SD reader mode (controls and screen updates).
//!
//! Entered with Ctrl+Alt+S from the normal mode, which reboots with a flag set (`board::system`).
//! The whole card is exposed over USB; the device itself never touches the file system meanwhile.
//! Esc reboots back to the normal mode. If the PC has not ejected the card yet, Esc warns first and a
//! second press within [`FORCE_EXIT_WINDOW`] leaves anyway.

use core::cell::RefCell;

use embassy_time::{Duration, Instant};
use esp_hal::gpio::Output;
use log::info;
use smart_leds_trait::RGB8;

use crate::board::display::Display;
use crate::board::led::StatusLed;
use crate::board::sdcard;
use crate::board::system;
use crate::input::{Key, KeyEvent};
use crate::sound;
use crate::storage::reader::{ReaderDisk, ReaderState};
use crate::ui::color;
use crate::ui::reader::{FOOTER_HINT, ReaderView};
use crate::usb;
use crate::usb::command::LinkState;

/// The whole SD card presented over USB.
pub type SdReader = ReaderDisk<sdcard::Card>;

/// LED color when accessed over USB (same as the floppy access lamp).
const USB_ACCESS_LED: RGB8 = RGB8::new(255, 160, 0);
/// Time after the last access before the LED turns off.
const ACCESS_LED_HOLD: Duration = Duration::from_millis(80);
/// Time after the "not ejected" warning during which pressing Esc again leaves anyway.
const FORCE_EXIT_WINDOW: Duration = Duration::from_secs(3);

pub struct Reader {
    lcd: Display,
    led: StatusLed,
    /// Backlight pin (kept because dropping it turns the backlight off).
    _backlight: Output<'static>,
    disk: &'static RefCell<SdReader>,
    view: ReaderView,
    state: ReaderState,
    link: LinkState,
    battery_percent: Option<u8>,
    /// Time the "not ejected" warning was shown.
    exit_armed: Option<Instant>,
    /// Leaving was requested; reboot once no transfer is running.
    leaving: bool,
    last_access: Instant,
    led_on: bool,
}

impl Reader {
    pub fn new(lcd: Display, backlight: Output<'static>, led: StatusLed, disk: &'static RefCell<SdReader>) -> Self {
        let state = disk.borrow().state();
        let mut reader = Self {
            lcd,
            led,
            _backlight: backlight,
            disk,
            view: ReaderView,
            state,
            link: LinkState::Detached,
            battery_percent: None,
            exit_armed: None,
            leaving: false,
            last_access: Instant::MIN,
            led_on: false,
        };
        reader.redraw();
        reader
    }

    pub fn set_battery(&mut self, percent: u8) {
        self.battery_percent = Some(percent);
        self.view.draw_battery(&mut self.lcd, self.battery_percent);
    }

    fn redraw(&mut self) {
        let bytes = self.disk.borrow().total_bytes();
        self.view
            .draw(&mut self.lcd, self.state, bytes, self.link, self.battery_percent);
    }

    // ---------------------------------------------------------------- Periodic processing

    /// Called periodically from the main loop.
    pub fn tick(&mut self) {
        self.poll_disk();
        self.poll_activity();
        self.poll_link();
        if self
            .exit_armed
            .is_some_and(|t| t.elapsed() >= FORCE_EXIT_WINDOW)
        {
            self.exit_armed = None;
            self.view.draw_footer(&mut self.lcd, FOOTER_HINT, color::DIM);
        }
    }

    /// Host eject requests, card errors, and a pending exit. The disk is only changed outside a transfer.
    fn poll_disk(&mut self) {
        if usb::HOST.is_busy() {
            return;
        }
        if usb::HOST.take_eject_request() && self.disk.borrow().state() == ReaderState::Ready {
            info!("Card ejected at host request");
            self.disk.borrow_mut().eject();
            sound::post(sound::Event::Eject);
        }
        let state = self.disk.borrow().state();
        if state != self.state {
            info!("Reader state: {:?}", state);
            self.state = state;
            self.view.draw_state(&mut self.lcd, state);
        }
        if self.leaving {
            self.leave();
        }
    }

    /// USB access lamp.
    fn poll_activity(&mut self) {
        if usb::ACTIVITY.try_take().is_some() {
            self.last_access = Instant::now();
            if !self.led_on {
                self.led.set(USB_ACCESS_LED);
                self.led_on = true;
            }
        }
        if self.led_on && self.last_access.elapsed() > ACCESS_LED_HOLD {
            self.led.off();
            self.led_on = false;
        }
    }

    /// USB connection state display.
    fn poll_link(&mut self) {
        let link = usb::HOST.link_state();
        if link != self.link {
            info!("USB: {:?}", link);
            self.link = link;
            self.view.draw_link(&mut self.lcd, link);
        }
    }

    // ---------------------------------------------------------------- Key handling

    pub fn on_key(&mut self, event: &KeyEvent) {
        if event.mods.ctrl && event.mods.alt {
            match event.key {
                Key::Char('d') => {
                    info!("Rebooting into download mode");
                    self.view
                        .draw_footer(&mut self.lcd, "Rebooting to download mode...", color::ACCENT);
                    system::reboot_to_download_mode();
                }
                Key::Char('r') => {
                    info!("Rebooting");
                    system::reboot();
                }
                _ => {}
            }
            return;
        }
        // Esc is Fn+`; the bare ` key (the "go up" key in the selector) works too
        if matches!(event.key, Key::Esc | Key::Char('`')) {
            self.request_exit();
        }
    }

    /// Development command (CDC-ACM): `exit` (same as Esc), `perf` / `perf reset` (transfer timing).
    pub fn dev_command(&mut self, line: &str) {
        match line {
            "exit" => self.request_exit(),
            "perf" => usb::perf::PERF.log(),
            "perf reset" => usb::perf::PERF.reset(),
            _ => info!("Reader mode: unknown command {}", line),
        }
    }

    /// Esc: leave right away when it is safe, otherwise warn first (pressing again leaves anyway).
    fn request_exit(&mut self) {
        if self.leaving {
            return;
        }
        // Safe when the PC ejected the card, the card is unusable anyway, or no PC is connected
        let safe = self.state != ReaderState::Ready || usb::HOST.link_state() == LinkState::Detached;
        let armed = self
            .exit_armed
            .take()
            .is_some_and(|t| t.elapsed() < FORCE_EXIT_WINDOW);
        if !safe && !armed {
            info!("Card not ejected by the host yet");
            self.exit_armed = Some(Instant::now());
            self.view
                .draw_footer(&mut self.lcd, "Not ejected on PC! Again to leave", color::ERROR);
            return;
        }
        self.leaving = true;
        self.view
            .draw_footer(&mut self.lcd, "Back to floppy mode...", color::ACCENT);
        // A read/write in progress finishes first (poll_disk calls leave once it is done)
        if !usb::HOST.is_busy() {
            self.leave();
        }
    }

    fn leave(&mut self) -> ! {
        info!("Leaving SD reader mode");
        self.led.off();
        system::reboot()
    }
}
