//! SD reader mode screen.
//!
//! ```text
//!  SD READER       USB:PC               87%   ← Header (USB state and battery)
//!  ────────────────────────────────────────
//!
//!  Card: 31.9 GB                              ← Card size
//!
//!  Ready. The PC can use the whole card.      ← State
//!  Eject it on the PC before leaving.
//!
//!  Keep FAT16/FAT32 + MBR for floppies.
//!  ────────────────────────────────────────
//!  Esc or `: back to floppy mode              ← Footer (also warnings)
//! ```

use core::fmt::Write;

use embedded_graphics::pixelcolor::Rgb565;
use heapless::String;

use super::{LINE_H, color, fill, hline, text_line};
use crate::board::display::{self, Display};
use crate::storage::reader::ReaderState;
use crate::usb::command::LinkState;

const HEADER_Y: i32 = 1;
const BODY_Y: i32 = 16;
const FOOTER_Y: i32 = BODY_Y + 8 * LINE_H + 4;

const CARD_Y: i32 = BODY_Y + LINE_H;
const STATE_Y: i32 = BODY_Y + LINE_H * 3;
const HINT_Y: i32 = BODY_Y + LINE_H * 4;
const NOTE_Y: i32 = BODY_Y + LINE_H * 6;

pub const FOOTER_HINT: &str = "Esc or `: back to floppy mode";

#[derive(Default)]
pub struct ReaderView;

impl ReaderView {
    /// Draw the whole screen.
    pub fn draw(&self, lcd: &mut Display, state: ReaderState, card_bytes: u64, link: LinkState, battery: Option<u8>) {
        fill(lcd, 0, 0, display::WIDTH, display::HEIGHT, color::BG);
        text_line(lcd, 2, HEADER_Y, 60, "SD READER", color::ACCENT, color::BG);
        self.draw_link(lcd, link);
        self.draw_battery(lcd, battery);
        hline(lcd, BODY_Y - 2, color::DIM);
        hline(lcd, FOOTER_Y - 2, color::DIM);

        let mut card: String<40> = String::new();
        if card_bytes > 0 {
            let _ = card.push_str("Card: ");
            push_size(&mut card, card_bytes);
        } else {
            let _ = card.push_str("Card: -");
        }
        text_line(lcd, 2, CARD_Y, display::WIDTH - 2, &card, color::FG, color::BG);
        text_line(
            lcd,
            2,
            NOTE_Y,
            display::WIDTH - 2,
            "Keep FAT16/FAT32 + MBR for floppies.",
            color::DIM,
            color::BG,
        );
        self.draw_state(lcd, state);
        self.draw_footer(lcd, FOOTER_HINT, color::DIM);
    }

    pub fn draw_state(&self, lcd: &mut Display, state: ReaderState) {
        let (line1, fg, line2) = match state {
            ReaderState::Ready => (
                "Ready. The PC can use the whole card.",
                color::FG,
                "Eject it on the PC before leaving.",
            ),
            ReaderState::Ejected => ("Ejected by the PC.", color::ACCENT, "Safe to leave now."),
            ReaderState::Failed => ("SD card error.", color::ERROR, "Check the card, then leave."),
        };
        text_line(lcd, 2, STATE_Y, display::WIDTH - 2, line1, fg, color::BG);
        text_line(lcd, 2, HINT_Y, display::WIDTH - 2, line2, color::DIM, color::BG);
    }

    pub fn draw_link(&self, lcd: &mut Display, link: LinkState) {
        let (text, fg) = match link {
            LinkState::Ready => ("USB:PC", color::FG),
            LinkState::Suspended => ("USB:zz", color::DIM),
            LinkState::Detached => ("USB:--", color::DIM),
        };
        text_line(lcd, 96, HEADER_Y, 40, text, fg, color::BG);
    }

    pub fn draw_battery(&self, lcd: &mut Display, percent: Option<u8>) {
        let mut s: String<8> = String::new();
        if let Some(p) = percent {
            let _ = write!(s, "{:>3}%", p);
        }
        text_line(lcd, display::WIDTH as i32 - 26, HEADER_Y, 26, &s, color::FG, color::BG);
    }

    pub fn draw_footer(&self, lcd: &mut Display, text: &str, fg: Rgb565) {
        text_line(lcd, 2, FOOTER_Y, display::WIDTH - 2, text, fg, color::BG);
    }
}

/// Size in decimal units with one decimal place, as printed on cards ("31.9 GB", "122.0 MB").
fn push_size<const N: usize>(s: &mut String<N>, bytes: u64) {
    let (tenths, unit) = if bytes >= 1_000_000_000 {
        (bytes / 100_000_000, "GB")
    } else {
        (bytes / 100_000, "MB")
    };
    let _ = write!(s, "{}.{} {}", tenths / 10, tenths % 10, unit);
}
