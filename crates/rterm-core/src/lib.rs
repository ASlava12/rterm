//! Terminal core: cell grid, attributes, ANSI/VT processing.
//!
//! Scope of this crate: pure data model + parser. No PTY, no rendering, no I/O.
//! Other crates feed bytes in via `Terminal::advance` and read the grid out.

pub mod color;
pub mod grid;
pub mod parser;
pub mod plugin;
pub mod terminal;

pub use color::{Color, NamedColor, DEFAULT_BG, DEFAULT_FG, DEFAULT_NAMED_PALETTE};
pub use grid::{Cell, CellAttrs, Grid, Position, Size};
pub use plugin::PluginCmd;
pub use terminal::{is_safe_url, CommandFinish, CursorShape, MouseTracking, PaletteUpdate, Terminal};
