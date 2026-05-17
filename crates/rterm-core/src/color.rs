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

/// Factory default foreground RGB (xterm-ish neutral grey on dark
/// background).
///
/// Shared between this crate's OSC 111 / RIS reset paths in
/// `terminal.rs` and `rterm-render`'s `DEFAULT_THEME` palette so that
/// `printf '\e]111\a'` reliably reverts to whatever the renderer also
/// draws as "Default" — drift between the two used to be possible when
/// the constants were hand-duplicated in each crate.
pub const DEFAULT_FG: [u8; 3] = [220, 220, 220];

/// Factory default background RGB; see [`DEFAULT_FG`] for the
/// cross-crate linkage. The matching OSC 112 reset path lives in
/// `terminal.rs`.
pub const DEFAULT_BG: [u8; 3] = [10, 12, 18];

/// Built-in 16-colour palette (8 normal + 8 bright).
///
/// Used by `Terminal::new` to seed `named_palette`, by OSC 104 (reset
/// specific / all named entries) to revert, and by `rterm-render`'s
/// `DEFAULT_THEME`. Matches the xterm-ish defaults that legacy rterm
/// shipped — pinning the colours here keeps every consumer in
/// agreement.
pub const DEFAULT_NAMED_PALETTE: [[u8; 3]; 16] = [
    [0, 0, 0],
    [205, 49, 49],
    [13, 188, 121],
    [229, 229, 16],
    [36, 114, 200],
    [188, 63, 188],
    [17, 168, 205],
    [229, 229, 229],
    [102, 102, 102],
    [241, 76, 76],
    [35, 209, 139],
    [245, 245, 67],
    [59, 142, 234],
    [214, 112, 214],
    [41, 184, 219],
    [255, 255, 255],
];
