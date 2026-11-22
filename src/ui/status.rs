//! Status screen (the normal screen while an image is inserted).
//!
//! ```text
//!  card-fdd        USB:PC               87%   ← Header (USB state and battery)
//!  ────────────────────────────────────────
//!
//!  DISK: Game Disk 1 (Boot).img              ← Inserted image
//!        1.44M 2HD 80/2/18  [WP]
//!
//!  TRK 42  H1  S07   READ                     ← Latest access position
//!  [██████████░░░░░░░░░░░░░░░░░░░░]           ← Track position
//!  ────────────────────────────────────────
//!  Tab:files  e:eject  w:WP  u:diag           ← Footer
//! ```

use core::fmt::Write;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use heapless::String;

use super::{COLS, LINE_H, color, fill, hline, text_line};
use crate::board::display::{self, Display};
use crate::storage::geometry::{self, Geometry};
use crate::usb::command::LinkState;

const HEADER_Y: i32 = 1;
const BODY_Y: i32 = 16;
const FOOTER_Y: i32 = BODY_Y + 8 * LINE_H + 4;

const DISK_Y: i32 = BODY_Y + LINE_H;
const FORMAT_Y: i32 = BODY_Y + LINE_H * 2;
const ACCESS_Y: i32 = BODY_Y + LINE_H * 4;
const BAR_Y: i32 = BODY_Y + LINE_H * 5 + 2;
const BAR_X: i32 = 6;
const BAR_W: u32 = display::WIDTH - 12;
const BAR_H: u32 = 8;

/// Information about the inserted image.
pub struct DiskInfo<'a> {
    pub name: &'a str,
    pub geometry: Geometry,
    pub write_protected: bool,
}

pub const FOOTER_HINT: &str = "Tab:files e:eject w:WP s:vol r:speed";

/// The latest access (for the track display).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub cylinder: u16,
    pub head: u8,
    pub sector: u8,
    pub write: bool,
}

#[derive(Default)]
pub struct StatusView {
    cylinders: u16,
}

impl StatusView {
    /// Draw the whole screen.
    pub fn draw(&mut self, lcd: &mut Display, disk: Option<DiskInfo>, link: LinkState, battery: Option<u8>) {
        fill(lcd, 0, 0, display::WIDTH, display::HEIGHT, color::BG);
        text_line(lcd, 2, HEADER_Y, 60, "card-fdd", color::ACCENT, color::BG);
        self.draw_link(lcd, link);
        self.draw_battery(lcd, battery);
        hline(lcd, BODY_Y - 2, color::DIM);
        hline(lcd, FOOTER_Y - 2, color::DIM);

        match disk {
            Some(d) => {
                self.cylinders = d.geometry.cylinders;
                let mut name: String<64> = String::new();
                let _ = write!(name, "DISK: {}", d.name);
                truncate(&mut name, COLS - 1);
                text_line(lcd, 2, DISK_Y, display::WIDTH - 2, &name, color::FG, color::BG);

                let mut fmt: String<48> = String::new();
                let label = geometry::detect(d.geometry.total_bytes());
                let _ = write!(
                    fmt,
                    "      {} {} {}/{}/{}",
                    label.map_or("?", |f| f.label),
                    label.map_or("", |f| f.kind),
                    d.geometry.cylinders,
                    d.geometry.heads,
                    d.geometry.sectors_per_track
                );
                text_line(lcd, 2, FORMAT_Y, display::WIDTH - 2, &fmt, color::DIM, color::BG);
                if d.write_protected {
                    text_line(lcd, display::WIDTH as i32 - 34, FORMAT_Y, 32, "[WP]", color::ERROR, color::BG);
                }
                self.draw_idle(lcd);
            }
            None => {
                self.cylinders = 0;
                text_line(lcd, 2, DISK_Y, display::WIDTH - 2, "No disk inserted.", color::FG, color::BG);
                text_line(lcd, 2, ACCESS_Y, display::WIDTH - 2, "Tab: choose an image", color::DIM, color::BG);
            }
        }
        self.draw_footer(lcd, FOOTER_HINT, color::DIM);
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

    /// Show the access position.
    pub fn draw_access(&mut self, lcd: &mut Display, access: Access) {
        let mut s: String<40> = String::new();
        let _ = write!(
            s,
            " TRK {:>2}  H{}  S{:02}   {}",
            access.cylinder,
            access.head,
            access.sector,
            if access.write { "WRITE" } else { "READ" }
        );
        let fg = if access.write { color::ERROR } else { color::ACCENT };
        text_line(lcd, 2, ACCESS_Y, display::WIDTH - 2, &s, fg, color::BG);
        self.draw_bar(lcd, Some(access.cylinder), fg);
    }

    /// When access stops, keep the position but dim the color.
    pub fn draw_idle(&mut self, lcd: &mut Display) {
        if self.cylinders == 0 {
            return;
        }
        text_line(lcd, 2, ACCESS_Y, display::WIDTH - 2, " idle", color::DIM, color::BG);
        self.draw_bar(lcd, None, color::DIM);
    }

    fn draw_bar(&mut self, lcd: &mut Display, cylinder: Option<u16>, fg: Rgb565) {
        let _ = Rectangle::new(Point::new(BAR_X, BAR_Y), Size::new(BAR_W, BAR_H))
            .into_styled(PrimitiveStyle::with_stroke(color::DIM, 1))
            .draw(lcd);
        fill(lcd, BAR_X + 1, BAR_Y + 1, BAR_W - 2, BAR_H - 2, color::BG);
        if let Some(c) = cylinder
            && self.cylinders > 1
        {
            let span = (BAR_W - 6) as i32;
            let x = BAR_X + 2 + span * i32::from(c) / i32::from(self.cylinders - 1);
            fill(lcd, x, BAR_Y + 1, 2, BAR_H - 2, fg);
        }
    }

    pub fn draw_footer(&self, lcd: &mut Display, text: &str, fg: Rgb565) {
        text_line(lcd, 2, FOOTER_Y, display::WIDTH - 2, text, fg, color::BG);
    }
}

fn truncate<const N: usize>(s: &mut String<N>, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut cut = max - 1;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    let _ = s.push('~');
}
