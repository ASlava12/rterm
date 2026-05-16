//! Terminal colour palette and sRGB ↔ linear conversion.
//!
//! The active palette is shared across the renderer via a `OnceLock`. The
//! application sets it once at startup from config; tests and the default
//! `Palette::default()` provide the standard xterm-ish values.

use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use rterm_core::{Color as TermColor, NamedColor};

/// Resolved RGB values for the 16 ANSI colours plus default fg/bg.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub default_fg: [u8; 3],
    pub default_bg: [u8; 3],
    /// Fixed cursor colour; `None` falls back to the cell's foreground.
    pub cursor: Option<[u8; 3]>,
    /// Indexed by `NamedColor::ansi_index()` (0..16).
    pub named: [[u8; 3]; 16],
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            default_fg: [220, 220, 220],
            default_bg: [10, 12, 18],
            cursor: None,
            named: [
                [0, 0, 0],          // Black
                [205, 49, 49],      // Red
                [13, 188, 121],     // Green
                [229, 229, 16],     // Yellow
                [36, 114, 200],     // Blue
                [188, 63, 188],     // Magenta
                [17, 168, 205],     // Cyan
                [229, 229, 229],    // White
                [102, 102, 102],    // BrightBlack
                [241, 76, 76],      // BrightRed
                [35, 209, 139],     // BrightGreen
                [245, 245, 67],     // BrightYellow
                [59, 142, 234],     // BrightBlue
                [214, 112, 214],    // BrightMagenta
                [41, 184, 219],     // BrightCyan
                [255, 255, 255],    // BrightWhite
            ],
        }
    }
}

/// Built-in named themes shipped with rterm. Users can pick one via the
/// `cycle_theme` action, the command palette ("Theme: …"), or the Lua
/// API `rterm.set_theme(name)`. Listed in a stable order so cycling is
/// predictable.
///
/// Each entry is a (canonical name, palette) pair. Display names are
/// derived by Title-casing the canonical name, replacing `-` with spaces.
pub fn builtin_themes() -> &'static [(&'static str, Palette)] {
    &[
        ("default", DEFAULT_THEME),
        ("dark", DEFAULT_THEME),
        ("dracula", DRACULA),
        ("solarized-dark", SOLARIZED_DARK),
        ("solarized-light", SOLARIZED_LIGHT),
        ("nord", NORD),
        ("gruvbox-dark", GRUVBOX_DARK),
        ("light", LIGHT),
    ]
}

/// Look up a theme by canonical name (case-insensitive). Returns the
/// palette + the canonical key so callers can persist / log the
/// resolved name.
pub fn theme_by_name(name: &str) -> Option<(&'static str, Palette)> {
    let needle = name.trim().to_ascii_lowercase();
    builtin_themes()
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&needle))
        .copied()
}

/// Built-in xterm-ish dark — same as `Palette::default()`. Kept as a
/// separate const so the themes table can refer to it by name.
const DEFAULT_THEME: Palette = Palette {
    default_fg: [220, 220, 220],
    default_bg: [10, 12, 18],
    cursor: None,
    named: [
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
    ],
};

const DRACULA: Palette = Palette {
    default_fg: [248, 248, 242],
    default_bg: [40, 42, 54],
    cursor: Some([248, 248, 242]),
    named: [
        [33, 34, 44],     // black
        [255, 85, 85],    // red
        [80, 250, 123],   // green
        [241, 250, 140],  // yellow
        [189, 147, 249],  // blue (purple in dracula)
        [255, 121, 198],  // magenta (pink)
        [139, 233, 253],  // cyan
        [248, 248, 242],  // white
        [98, 114, 164],   // bright black (comment)
        [255, 110, 110],  // bright red
        [105, 255, 148],  // bright green
        [255, 255, 165],  // bright yellow
        [214, 172, 255],  // bright blue
        [255, 146, 223],  // bright magenta
        [164, 255, 255],  // bright cyan
        [255, 255, 255],  // bright white
    ],
};

const SOLARIZED_DARK: Palette = Palette {
    default_fg: [131, 148, 150],
    default_bg: [0, 43, 54],
    cursor: Some([220, 50, 47]),
    named: [
        [7, 54, 66],      // base02
        [220, 50, 47],    // red
        [133, 153, 0],    // green
        [181, 137, 0],    // yellow
        [38, 139, 210],   // blue
        [211, 54, 130],   // magenta
        [42, 161, 152],   // cyan
        [238, 232, 213],  // base2
        [0, 43, 54],      // base03
        [203, 75, 22],    // orange
        [88, 110, 117],   // base01
        [101, 123, 131],  // base00
        [131, 148, 150],  // base0
        [108, 113, 196],  // violet
        [147, 161, 161],  // base1
        [253, 246, 227],  // base3
    ],
};

const SOLARIZED_LIGHT: Palette = Palette {
    default_fg: [101, 123, 131],
    default_bg: [253, 246, 227],
    cursor: Some([220, 50, 47]),
    named: SOLARIZED_DARK.named,
};

const NORD: Palette = Palette {
    default_fg: [216, 222, 233],
    default_bg: [46, 52, 64],
    cursor: Some([216, 222, 233]),
    named: [
        [59, 66, 82],      // black (nord1)
        [191, 97, 106],    // red (nord11)
        [163, 190, 140],   // green (nord14)
        [235, 203, 139],   // yellow (nord13)
        [129, 161, 193],   // blue (nord9)
        [180, 142, 173],   // magenta (nord15)
        [136, 192, 208],   // cyan (nord8)
        [229, 233, 240],   // white (nord5)
        [76, 86, 106],     // bright black (nord3)
        [191, 97, 106],    // bright red
        [163, 190, 140],   // bright green
        [235, 203, 139],   // bright yellow
        [129, 161, 193],   // bright blue
        [180, 142, 173],   // bright magenta
        [143, 188, 187],   // bright cyan (nord7)
        [236, 239, 244],   // bright white (nord6)
    ],
};

const GRUVBOX_DARK: Palette = Palette {
    default_fg: [235, 219, 178],
    default_bg: [40, 40, 40],
    cursor: Some([235, 219, 178]),
    named: [
        [40, 40, 40],
        [204, 36, 29],
        [152, 151, 26],
        [215, 153, 33],
        [69, 133, 136],
        [177, 98, 134],
        [104, 157, 106],
        [168, 153, 132],
        [146, 131, 116],
        [251, 73, 52],
        [184, 187, 38],
        [250, 189, 47],
        [131, 165, 152],
        [211, 134, 155],
        [142, 192, 124],
        [235, 219, 178],
    ],
};

const LIGHT: Palette = Palette {
    default_fg: [40, 42, 54],
    default_bg: [253, 246, 227],
    cursor: Some([40, 42, 54]),
    named: [
        [40, 40, 40],
        [157, 0, 6],
        [0, 102, 0],
        [128, 96, 0],
        [0, 51, 153],
        [128, 0, 128],
        [0, 102, 102],
        [80, 80, 80],
        [120, 120, 120],
        [218, 0, 0],
        [0, 153, 0],
        [200, 150, 0],
        [40, 100, 230],
        [200, 0, 200],
        [0, 178, 178],
        [40, 40, 40],
    ],
};

/// Mutable global palette. Initialised lazily to `Palette::default()` on
/// first read so library callers (tests, embedded usage) don't need to
/// init explicitly. The App replaces the inner Arc on startup and on
/// config hot-reload via `init_palette`.
fn palette_slot() -> &'static Mutex<Arc<Palette>> {
    static SLOT: OnceLock<Mutex<Arc<Palette>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Arc::new(Palette::default())))
}

/// Whether SGR BOLD also brightens NamedColors 0..=7 to their bright variant
/// (xterm convention). Initialised true; the App overrides from config at
/// startup with `set_bold_is_bright`.
static BOLD_IS_BRIGHT: AtomicBool = AtomicBool::new(true);

pub fn set_bold_is_bright(v: bool) {
    BOLD_IS_BRIGHT.store(v, Ordering::Relaxed);
}

pub fn bold_is_bright() -> bool {
    BOLD_IS_BRIGHT.load(Ordering::Relaxed)
}

/// Install a custom palette. Replaces the previously-installed one — used
/// at startup and on config hot-reload.
pub fn init_palette(p: Palette) {
    if let Ok(mut g) = palette_slot().lock() {
        *g = Arc::new(p);
    }
}

/// Read the active palette as a value snapshot. Cheap: 58 bytes plus a
/// brief mutex lock to clone the Arc and copy out the struct.
pub fn palette() -> Palette {
    palette_slot().lock().map(|g| **g).unwrap_or_default()
}

/// Back-compat helpers — most callers want the active palette's defaults.
pub fn default_fg() -> [u8; 3] {
    palette().default_fg
}
pub fn default_bg() -> [u8; 3] {
    palette().default_bg
}
pub fn cursor_color() -> Option<[u8; 3]> {
    palette().cursor
}

/// Constants kept for tests and call sites that took them by value.
pub const DEFAULT_FG: [u8; 3] = [220, 220, 220];
pub const DEFAULT_BG: [u8; 3] = [10, 12, 18];

pub fn color_to_rgb(c: TermColor, default: [u8; 3]) -> [u8; 3] {
    match c {
        TermColor::Default => default,
        TermColor::Rgb(r, g, b) => [r, g, b],
        TermColor::Named(n) => named_color_to_rgb(n),
        TermColor::Indexed(i) => indexed_color_to_rgb(i),
    }
}

pub fn named_color_to_rgb(n: NamedColor) -> [u8; 3] {
    palette().named[n as usize]
}

pub fn indexed_color_to_rgb(i: u8) -> [u8; 3] {
    if i < 16 {
        return palette().named[i as usize];
    }
    if i < 232 {
        let v = i - 16;
        let r = v / 36;
        let g = (v / 6) % 6;
        let b = v % 6;
        let map = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
        return [map(r), map(g), map(b)];
    }
    let gray = 8 + (i - 232) * 10;
    [gray, gray, gray]
}

pub fn srgb_byte_to_linear(c: u8) -> f32 {
    let v = c as f32 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

pub fn rgb_to_linear_rgba(rgb: [u8; 3], a: f32) -> [f32; 4] {
    [
        srgb_byte_to_linear(rgb[0]),
        srgb_byte_to_linear(rgb[1]),
        srgb_byte_to_linear(rgb[2]),
        a,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rterm_core::Color;

    #[test]
    fn default_falls_back() {
        assert_eq!(color_to_rgb(Color::Default, DEFAULT_FG), DEFAULT_FG);
        assert_eq!(color_to_rgb(Color::Default, DEFAULT_BG), DEFAULT_BG);
    }

    #[test]
    fn rgb_passes_through() {
        assert_eq!(color_to_rgb(Color::Rgb(10, 20, 30), DEFAULT_FG), [10, 20, 30]);
    }

    #[test]
    fn indexed_low_matches_named() {
        assert_eq!(indexed_color_to_rgb(1), named_color_to_rgb(NamedColor::Red));
        assert_eq!(indexed_color_to_rgb(9), named_color_to_rgb(NamedColor::BrightRed));
    }

    #[test]
    fn indexed_cube_corners() {
        assert_eq!(indexed_color_to_rgb(16), [0, 0, 0]);
        assert_eq!(indexed_color_to_rgb(231), [255, 255, 255]);
    }

    #[test]
    fn grayscale_progression() {
        assert_eq!(indexed_color_to_rgb(232), [8, 8, 8]);
        assert_eq!(indexed_color_to_rgb(255), [238, 238, 238]);
    }

    #[test]
    fn builtin_themes_are_discoverable_and_distinct() {
        let names: Vec<&str> = builtin_themes().iter().map(|(n, _)| *n).collect();
        // Stable order so cycle_theme produces a predictable sequence.
        assert_eq!(names[0], "default");
        // Every shipped theme name must resolve via theme_by_name,
        // case-insensitively.
        for name in &names {
            let (canon, _) = theme_by_name(name).expect("theme should resolve");
            assert_eq!(canon, *name);
            let (canon_upper, _) = theme_by_name(&name.to_ascii_uppercase())
                .expect("case-insensitive lookup");
            assert_eq!(canon_upper, *name);
        }
        // Unknown names return None.
        assert!(theme_by_name("not-a-real-theme").is_none());
        // Dracula and Solarized Dark are distinct palettes.
        let (_, d) = theme_by_name("dracula").unwrap();
        let (_, s) = theme_by_name("solarized-dark").unwrap();
        assert_ne!(d.default_bg, s.default_bg);
    }

    #[test]
    fn srgb_linear_roundtrip_endpoints() {
        let a = srgb_byte_to_linear(0);
        let b = srgb_byte_to_linear(255);
        assert!(a.abs() < 1e-6);
        assert!((b - 1.0).abs() < 1e-4);
    }
}
