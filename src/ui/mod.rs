//! Screen display.
//!
//! Shows about 40 columns x 11 rows with a 6x10 font on the 240x135 screen.
//! There is no framebuffer, so only changed rows are filled with the background and redrawn.

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle, Rectangle};
use embedded_graphics::text::Text;

use crate::board::display::{self, Display};

pub mod reader;
pub mod selector;
pub mod status;

/// Row height [px].
pub const LINE_H: i32 = 12;
/// Character width [px].
pub const CHAR_W: i32 = 6;
/// Characters per row.
pub const COLS: usize = (display::WIDTH as i32 / CHAR_W) as usize;

pub mod color {
    use embedded_graphics::pixelcolor::Rgb565;
    use embedded_graphics::prelude::RgbColor;

    pub const BG: Rgb565 = Rgb565::BLACK;
    pub const FG: Rgb565 = Rgb565::WHITE;
    pub const DIM: Rgb565 = Rgb565::new(12, 24, 12);
    pub const ACCENT: Rgb565 = Rgb565::new(31, 40, 0);
    pub const DIR: Rgb565 = Rgb565::new(12, 48, 31);
    pub const CURSOR_BG: Rgb565 = Rgb565::new(2, 12, 12);
    pub const ERROR: Rgb565 = Rgb565::new(31, 16, 8);
}

/// Fill `width` x one row with the background color, then draw text on it. `y` is the top of the row.
pub fn text_line(lcd: &mut Display, x: i32, y: i32, width: u32, text: &str, fg: Rgb565, bg: Rgb565) {
    fill(lcd, x, y, width, LINE_H as u32, bg);
    text_at(lcd, x, y, text, fg);
}

/// Draw text only, without filling the background. `y` is the top of the row.
pub fn text_at(lcd: &mut Display, x: i32, y: i32, text: &str, fg: Rgb565) {
    let style = MonoTextStyle::new(&FONT_6X10, fg);
    let _ = Text::new(text, Point::new(x, y + 9), style).draw(lcd);
}

pub fn fill(lcd: &mut Display, x: i32, y: i32, width: u32, height: u32, color: Rgb565) {
    let _ = Rectangle::new(Point::new(x, y), Size::new(width, height))
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(lcd);
}

/// Horizontal line across the full screen width.
pub fn hline(lcd: &mut Display, y: i32, color: Rgb565) {
    let _ = Line::new(Point::new(0, y), Point::new(display::WIDTH as i32 - 1, y))
        .into_styled(PrimitiveStyle::with_stroke(color, 1))
        .draw(lcd);
}
