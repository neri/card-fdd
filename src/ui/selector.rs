//! Image file selector.
//!
//! ```text
//!  /FLOPPY/GAMES                       87%   ← Header (path and battery)
//!  ────────────────────────────────────────
//!  ..                                        ← List (8 rows)
//!  Utilities/                        <DIR>
//!  DOS 6.22 Disk1.img                1.44M
//!  Game Disk 1 (Boot).img            1.44M   ← Cursor row has a different background color
//!  old_720k.img                       720K   ← Unsupported sizes are shown dimmed
//!  ────────────────────────────────────────
//!  ;. move  Enter open  ` back               ← Footer (key help or messages)
//! ```

use core::fmt::Write;

use embedded_graphics::pixelcolor::Rgb565;
use heapless::String;

use super::{COLS, LINE_H, color, fill, hline, text_line};
use crate::board::display::{self, Display};
use crate::storage::browser::{Entry, EntryKind};
use crate::storage::image::EntryId;

/// Number of rows shown in the list.
pub const VISIBLE_ROWS: usize = 8;

const HEADER_Y: i32 = 1;
const LIST_Y: i32 = 16;
const FOOTER_Y: i32 = LIST_Y + VISIBLE_ROWS as i32 * LINE_H + 4;
/// Width of the size column on the right edge.
const SIZE_COLS: usize = 6;
/// Width of the name column.
const NAME_COLS: usize = COLS - SIZE_COLS - 2;

#[derive(Default)]
pub struct Selector {
    cursor: usize,
    top: usize,
}

impl Selector {
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Reset the cursor to the top when the list changes (e.g. moving to another folder).
    pub fn reset(&mut self) {
        self.cursor = 0;
        self.top = 0;
    }

    /// Move the cursor by `delta` rows (stops at the ends).
    pub fn move_by(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.reset();
            return;
        }
        let cursor = (self.cursor as isize + delta).clamp(0, len as isize - 1);
        self.cursor = cursor as usize;
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + VISIBLE_ROWS {
            self.top = self.cursor + 1 - VISIBLE_ROWS;
        }
    }

    /// Draw the whole screen.
    pub fn draw(
        &self,
        lcd: &mut Display,
        path: &str,
        entries: &[Entry],
        mounted: Option<EntryId>,
        battery_percent: Option<u8>,
    ) {
        fill(lcd, 0, 0, display::WIDTH, display::HEIGHT, color::BG);
        self.draw_header(lcd, path, battery_percent);
        hline(lcd, LIST_Y - 2, color::DIM);
        self.draw_list(lcd, entries, mounted);
        hline(lcd, FOOTER_Y - 2, color::DIM);
    }

    pub fn draw_header(&self, lcd: &mut Display, path: &str, battery_percent: Option<u8>) {
        // Show the end of the path when it is long
        const PATH_COLS: usize = COLS - 6;
        let shown = if path.len() > PATH_COLS {
            &path[path.len() - PATH_COLS..]
        } else {
            path
        };
        text_line(lcd, 2, HEADER_Y, (PATH_COLS * 6) as u32, shown, color::ACCENT, color::BG);
        self.draw_battery(lcd, battery_percent);
    }

    pub fn draw_battery(&self, lcd: &mut Display, percent: Option<u8>) {
        let mut s: String<8> = String::new();
        if let Some(p) = percent {
            let _ = write!(s, "{:>3}%", p);
        }
        text_line(lcd, display::WIDTH as i32 - 26, HEADER_Y, 26, &s, color::FG, color::BG);
    }

    /// Draw the list. `mounted` is the inserted image (marked with `*` at the start of the row).
    pub fn draw_list(&self, lcd: &mut Display, entries: &[Entry], mounted: Option<EntryId>) {
        for row in 0..VISIBLE_ROWS {
            let y = LIST_Y + row as i32 * LINE_H;
            let index = self.top + row;
            match entries.get(index) {
                Some(entry) => {
                    let is_mounted = entry.kind != EntryKind::Parent && Some(entry.id) == mounted;
                    draw_entry(lcd, y, entry, index == self.cursor, is_mounted)
                }
                None => fill(lcd, 0, y, display::WIDTH, LINE_H as u32, color::BG),
            }
        }
    }

    /// Show one line in the footer.
    pub fn draw_footer(&self, lcd: &mut Display, text: &str, fg: Rgb565) {
        text_line(lcd, 2, FOOTER_Y, display::WIDTH - 2, text, fg, color::BG);
    }

    /// Show a message instead of the list (e.g. when there is no card).
    pub fn draw_message(&self, lcd: &mut Display, lines: &[&str], fg: Rgb565) {
        fill(lcd, 0, LIST_Y, display::WIDTH, (VISIBLE_ROWS as i32 * LINE_H) as u32, color::BG);
        for (i, line) in lines.iter().enumerate() {
            text_line(lcd, 8, LIST_Y + LINE_H * (i as i32 + 1), display::WIDTH - 8, line, fg, color::BG);
        }
    }
}

fn draw_entry(lcd: &mut Display, y: i32, entry: &Entry, selected: bool, mounted: bool) {
    let bg = if selected { color::CURSOR_BG } else { color::BG };
    let (fg, size): (Rgb565, String<SIZE_COLS>) = match entry.kind {
        EntryKind::Parent => (color::DIR, String::new()),
        EntryKind::Dir => (color::DIR, String::try_from("<DIR>").unwrap_or_default()),
        EntryKind::Image(Some(f)) => (
            if f.supported { color::FG } else { color::DIM },
            String::try_from(f.label).unwrap_or_default(),
        ),
        EntryKind::Image(None) => (color::DIM, String::try_from("?").unwrap_or_default()),
    };

    let mut name: String<{ NAME_COLS + 1 }> = String::new();
    let suffix = if entry.kind == EntryKind::Dir { "/" } else { "" };
    if entry.name.len() + suffix.len() > NAME_COLS {
        let _ = name.push_str(&entry.name[..NAME_COLS - 1]);
        let _ = name.push('~');
    } else {
        let _ = name.push_str(&entry.name);
        let _ = name.push_str(suffix);
    }

    text_line(lcd, 0, y, display::WIDTH, "", fg, bg);
    if mounted {
        super::text_at(lcd, 0, y, "*", color::ACCENT);
    }
    super::text_at(lcd, 6, y, &name, fg);
    let size_x = display::WIDTH as i32 - 2 - (size.len() as i32 * 6);
    super::text_at(lcd, size_x, y, &size, fg);
}
