//! Key input definitions (hardware independent).
//!
//! The Cardputer keyboard is physically 4 rows x 14 columns. Key positions are represented by [`KeyPos`],
//! and the logical key [`Key`] is determined from the modifier (Shift / Fn) state.

/// Physical key position. `row` is 0..4 from the top, `col` is 0..14 from the left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPos {
    pub row: u8,
    pub col: u8,
}

pub const ROWS: usize = 4;
pub const COLS: usize = 14;

impl KeyPos {
    /// Index in the bitmap (`row * 14 + col`).
    pub const fn index(self) -> u8 {
        self.row * COLS as u8 + self.col
    }

    pub const fn from_index(index: u8) -> Self {
        Self {
            row: index / COLS as u8,
            col: index % COLS as u8,
        }
    }
}

/// Logical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    Delete,
    Tab,
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Fn,
    Shift,
    Ctrl,
    Opt,
    Alt,
}

impl Key {
    pub const fn is_modifier(self) -> bool {
        matches!(self, Key::Fn | Key::Shift | Key::Ctrl | Key::Opt | Key::Alt)
    }
}

/// Pressed state of the modifier keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub fn_: bool,
    pub ctrl: bool,
    pub opt: bool,
    pub alt: bool,
}

/// Key event. `key` is determined by the modifier state at the moment of pressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub pos: KeyPos,
    pub key: Key,
    pub pressed: bool,
    pub mods: Modifiers,
}

use Key::*;

/// Key layout without modifiers.
#[rustfmt::skip]
const BASE: [[Key; COLS]; ROWS] = [
    [
        Char('`'), Char('1'), Char('2'), Char('3'), Char('4'), Char('5'), Char('6'),
        Char('7'), Char('8'), Char('9'), Char('0'), Char('-'), Char('='), Backspace,
    ],
    [
        Tab, Char('q'), Char('w'), Char('e'), Char('r'), Char('t'), Char('y'),
        Char('u'), Char('i'), Char('o'), Char('p'), Char('['), Char(']'), Char('\\'),
    ],
    [
        Fn, Shift, Char('a'), Char('s'), Char('d'), Char('f'), Char('g'),
        Char('h'), Char('j'), Char('k'), Char('l'), Char(';'), Char('\''), Enter,
    ],
    [
        Ctrl, Opt, Alt, Char('z'), Char('x'), Char('c'), Char('v'),
        Char('b'), Char('n'), Char('m'), Char(','), Char('.'), Char('/'), Char(' '),
    ],
];

/// Symbols when Shift is held (letters are handled by uppercasing).
const fn shifted(c: char) -> char {
    match c {
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        c => c.to_ascii_uppercase(),
    }
}

/// Determine the logical key from the physical key position and the modifier state.
///
/// Fn combinations follow the legends printed on the keycaps:
/// Fn+` = Esc, Fn+Backspace = Delete, Fn+; / . / , / / = Up / Down / Left / Right.
pub fn resolve(pos: KeyPos, mods: Modifiers) -> Key {
    let base = BASE[pos.row as usize][pos.col as usize];
    if mods.fn_ {
        match base {
            Char('`') => return Esc,
            Backspace => return Delete,
            Char(';') => return Up,
            Char('.') => return Down,
            Char(',') => return Left,
            Char('/') => return Right,
            _ => {}
        }
    }
    match base {
        Char(c) if mods.shift => Char(shifted(c)),
        k => k,
    }
}

/// Get the modifier state from the bitmap of pressed keys (bit index = [`KeyPos::index`]).
pub fn modifiers_from_bitmap(bits: u64) -> Modifiers {
    let pressed = |row: u8, col: u8| bits & (1 << KeyPos { row, col }.index()) != 0;
    Modifiers {
        fn_: pressed(2, 0),
        shift: pressed(2, 1),
        ctrl: pressed(3, 0),
        opt: pressed(3, 1),
        alt: pressed(3, 2),
    }
}
