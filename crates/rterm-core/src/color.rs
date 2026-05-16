//! Terminal colors: 16 named, 256-indexed, 24-bit truecolor.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    Default,
    Named(NamedColor),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl NamedColor {
    pub fn ansi_index(self) -> u8 {
        self as u8
    }
}
