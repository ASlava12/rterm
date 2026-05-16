//! Terminal core: cell grid, attributes, ANSI/VT processing.
//!
//! Scope of this crate: pure data model + parser. No PTY, no rendering, no I/O.
//! Other crates feed bytes in via `Terminal::advance` and read the grid out.

pub mod color;
pub mod grid;
pub mod parser;
pub mod terminal;

pub use color::{Color, NamedColor};
pub use grid::{Cell, CellAttrs, Grid, Position, Size};
pub use terminal::{CommandFinish, CursorShape, MouseTracking, PaletteUpdate, Terminal};
