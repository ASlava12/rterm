//! Keybinding parsing.
//!
//! - `parse_key_spec` turns a string like `"Ctrl+Shift+T"` /
//!   `"alt+right"` / `"F1"` into a (mods, key-match) pair.
//! - `KeyMatch` is the small enum the runtime keybind walker
//!   matches against incoming winit `KeyEvent`s.
//! - `UserBinding` is the resolved (config-derived) binding —
//!   modifiers + key + a typed `AppAction` + the original spec
//!   string for display in the help overlay.
//!
//! Fields on `UserBinding` and `KeyMatch` are `pub(crate)` so the
//! key-match walker in `lib.rs` can match directly without going
//! through accessor methods. They are not part of the crate's
//! public API.

use winit::keyboard::{ModifiersState, NamedKey};

use crate::AppAction;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyMatch {
    /// Lowercase character — compared against `Key::Character` case-insensitively.
    Char(String),
    Named(NamedKey),
}

#[derive(Debug, Clone)]
pub struct UserBinding {
    pub(crate) mods: ModifiersState,
    pub(crate) key: KeyMatch,
    pub(crate) action: AppAction,
    /// Original key spec from config (`"Ctrl+Shift+T"`). Preserved
    /// verbatim for the help-overlay listing so the user sees the
    /// exact text they wrote rather than a normalised form.
    pub(crate) spec: String,
    /// Canonical action name (`"new_tab"`, `"split_horizontal"`, ...).
    /// Stored alongside the typed `AppAction` so the help overlay can
    /// print it without re-deriving from the enum variant.
    pub(crate) action_name: String,
}

/// Parse a key spec like `"Ctrl+Shift+T"`, `"Alt+Right"`, `"F1"`.
/// Returns None if the spec is malformed or names an unknown key.
pub(crate) fn parse_key_spec(s: &str) -> Option<(ModifiersState, KeyMatch)> {
    let mut mods = ModifiersState::empty();
    let mut key: Option<KeyMatch> = None;
    for token in s.split('+').map(str::trim).filter(|t| !t.is_empty()) {
        let lower = token.to_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods |= ModifiersState::CONTROL,
            "shift" => mods |= ModifiersState::SHIFT,
            "alt" | "option" => mods |= ModifiersState::ALT,
            "super" | "cmd" | "meta" | "win" => mods |= ModifiersState::SUPER,
            other => {
                let named = match other {
                    "enter" | "return" => Some(NamedKey::Enter),
                    "esc" | "escape" => Some(NamedKey::Escape),
                    "tab" => Some(NamedKey::Tab),
                    "space" => Some(NamedKey::Space),
                    "backspace" => Some(NamedKey::Backspace),
                    "delete" | "del" => Some(NamedKey::Delete),
                    "insert" | "ins" => Some(NamedKey::Insert),
                    "home" => Some(NamedKey::Home),
                    "end" => Some(NamedKey::End),
                    "pageup" | "pgup" => Some(NamedKey::PageUp),
                    "pagedown" | "pgdn" => Some(NamedKey::PageDown),
                    "up" | "arrowup" => Some(NamedKey::ArrowUp),
                    "down" | "arrowdown" => Some(NamedKey::ArrowDown),
                    "left" | "arrowleft" => Some(NamedKey::ArrowLeft),
                    "right" | "arrowright" => Some(NamedKey::ArrowRight),
                    "f1" => Some(NamedKey::F1),
                    "f2" => Some(NamedKey::F2),
                    "f3" => Some(NamedKey::F3),
                    "f4" => Some(NamedKey::F4),
                    "f5" => Some(NamedKey::F5),
                    "f6" => Some(NamedKey::F6),
                    "f7" => Some(NamedKey::F7),
                    "f8" => Some(NamedKey::F8),
                    "f9" => Some(NamedKey::F9),
                    "f10" => Some(NamedKey::F10),
                    "f11" => Some(NamedKey::F11),
                    "f12" => Some(NamedKey::F12),
                    _ => None,
                };
                // Named-key shorthand for punctuation that can't sit in
                // a `+`-separated token (the literal `+` itself) or that
                // a user might prefer to spell out. They map to `Char`
                // entries so the existing binding match treats them
                // identically to typing the character directly.
                let punct: Option<&str> = match other {
                    "plus" => Some("+"),
                    "minus" | "dash" => Some("-"),
                    "equal" | "equals" | "eq" => Some("="),
                    "comma" => Some(","),
                    "period" | "dot" => Some("."),
                    "slash" => Some("/"),
                    "backslash" => Some("\\"),
                    "semicolon" => Some(";"),
                    "colon" => Some(":"),
                    "apostrophe" | "quote" => Some("'"),
                    _ => None,
                };
                key = Some(match (named, punct) {
                    (Some(n), _) => KeyMatch::Named(n),
                    (None, Some(s)) => KeyMatch::Char(s.to_string()),
                    (None, None) => KeyMatch::Char(other.to_string()),
                });
            }
        }
    }
    key.map(|k| (mods, k))
}

impl UserBinding {
    /// Try to build a binding from a (keys, action) config entry.
    pub fn from_config(keys: &str, action: &str) -> Option<Self> {
        let (mods, key) = parse_key_spec(keys)?;
        let parsed = AppAction::from_name(action)?;
        Some(Self {
            mods,
            key,
            action: parsed,
            spec: keys.to_string(),
            action_name: action.to_string(),
        })
    }

    /// Human-readable action label for the help overlay. Falls back to
    /// the configured action name when no canonical match is found
    /// (won't happen in practice — `from_config` already filters).
    pub fn action_name(&self) -> &str {
        &self.action_name
    }
}
