//! Plugin → app/renderer command vocabulary.
//!
//! `PluginCmd` is a tagged-union data type — it describes a request,
//! it doesn't execute one. Living here in `rterm-core` (which already
//! hosts the cell-level + parser data types) lets both `rterm-plugin`
//! (producer side, sends commands via a channel) and `rterm-render`
//! (consumer side, drains per-frame and dispatches) reference the
//! same enum without crossing each other's dep edges. The crate's
//! "pure data, no I/O" boundary holds: nothing here does syscalls,
//! spawns threads, or touches OS state.
//!
//! Each variant corresponds to a single Lua-callable `rterm.*`
//! setter on the plugin host side. The host's pre-channel
//! architecture had one `Arc<Mutex<VecDeque<T>>>` per variant; the
//! audit asked for unification, and the dep-graph blocker that
//! delayed the migration is resolved by defining the enum here.
//!
//! Variants are added as their legacy `pending_*` queue is migrated
//! over — this is the destination type for the in-progress refactor.

/// Fire-and-forget command from a Lua plugin to the app / renderer.
///
/// `non_exhaustive` so adding a new variant in a follow-up doesn't
/// break callers who exhaustively match on the enum.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum PluginCmd {
    /// `rterm.notify(message)` — desktop notification. The renderer
    /// drains these and routes to the OS notification surface
    /// (`notify-send` on Linux, `NSUserNotification` on macOS, etc.).
    Notify(String),
}
