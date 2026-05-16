//! Thin wrapper around `vte::Parser`. The actual VT actions are dispatched onto
//! a `vte::Perform` impl on the terminal — see `terminal.rs`.

pub use vte::{Params, Parser, Perform};
