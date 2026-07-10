//! Configuration model and loader.
//!
//! Two surfaces:
//! - declarative TOML at `~/.config/rterm/config.toml` (and platform equivalents)
//! - imperative Lua at `~/.config/rterm/init.lua`, evaluated by `rterm-plugin`
//!
//! This crate only defines the data model + loads the TOML half. Lua evaluation
//! lives in `rterm-plugin` so this crate stays free of native deps.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub shell: ShellConfig,
    pub font: FontConfig,
    pub window: WindowConfig,
    pub colors: ColorsConfig,
    pub keybindings: Vec<Keybinding>,
    pub terminal: TerminalConfig,
    pub appearance: AppearanceConfig,
    pub guake: GuakeConfig,
    pub history: HistoryConfig,
    pub paste: PasteConfig,
    pub image: ImageConfig,
    pub highlight: HighlightConfig,
    /// Saved connection / launch presets (`[[profiles]]`). Empty by
    /// default. Selected with `rterm --profile <name>`.
    pub profiles: Vec<ProfileConfig>,
}

impl Config {
    /// Look up a profile by `name` (case-sensitive, first match).
    pub fn profile(&self, name: &str) -> Option<&ProfileConfig> {
        self.profiles.iter().find(|p| p.name == name)
    }
}

/// Inline-image protocol toggles. When `enabled = false`, both
/// the iTerm2 `OSC 1337 ;File=` and Kitty `APC G` parsers drop
/// new payloads silently. Already-displayed images stay in place
/// — the toggle gates incoming payloads, not the live state, so
/// flipping mid-session doesn't surprise the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    pub enabled: bool,
    /// Opt-in: detect raw PNG / JPEG magic bytes in the input
    /// stream (at newline boundaries) and display them as
    /// images — lets `cat picture.png` "just work" without a
    /// helper utility. Disabled by default because of the
    /// false-positive risk (intentional hex / network dumps).
    /// User can flip from the Settings overlay too.
    pub auto_detect: bool,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_detect: false,
        }
    }
}

/// Bulk-paste safety prompt. When enabled, pasting text that
/// contains newlines triggers a modal confirmation before any byte
/// reaches the PTY — protects against an accidental drag-paste of a
/// half-formed multi-line command (a real footgun: pasting a list of
/// hostnames with embedded newlines auto-submits each one as a
/// command if the shell is at a prompt).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PasteConfig {
    /// Master toggle. `true` (default) shows the confirmation modal
    /// on every multi-line paste; `false` falls back to the legacy
    /// "just paste" behaviour.
    pub confirm_multiline: bool,
    /// Skip the modal for pastes shorter than this many bytes even
    /// when they contain newlines. A two-line `cd` + `ls` chain is
    /// usually intentional and doesn't warrant a prompt. Default
    /// 80 bytes — empirically the size below which pastes are
    /// almost always user-curated snippets rather than blob dumps.
    pub confirm_min_bytes: u32,
}

impl Default for PasteConfig {
    fn default() -> Self {
        Self {
            confirm_multiline: true,
            confirm_min_bytes: 80,
        }
    }
}

/// Terminal-side command-history settings. Controls when the
/// suggestion popup appears, how many rows it shows, and the
/// minimum prefix length before the popup considers itself
/// "armed". Defaults pick a sensible UX for typical shell
/// workflows; tune via the `[history]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    /// Master toggle. `false` disables capture AND the popup —
    /// both effectively become no-ops. `true` (default) keeps
    /// everything running with the rest of the defaults below.
    pub enabled: bool,
    /// Number of suggestion rows the popup shows at once. The
    /// dropdown is scrollable: arrow keys move beyond the visible
    /// window without resizing it.
    pub popup_rows: u8,
    /// Milliseconds the user must pause typing before the popup
    /// queries the history and appears. Default 150ms — short
    /// enough to feel responsive, long enough to skip the
    /// per-keystroke flicker.
    pub popup_debounce_ms: u32,
    /// Minimum number of characters in the current input before
    /// the popup is considered for display. Set higher (e.g. 3)
    /// to silence the popup on single-letter commands like `ls`
    /// or `vi` where users rarely want a dropdown.
    pub min_prefix_len: u8,
    /// When `true`, a command line that included any bracketed-paste
    /// content is NOT written to history — so a pasted password / API
    /// token doesn't linger in the suggestion store. `false` (default)
    /// records pasted commands like any other. Off by default to keep
    /// history complete; privacy-conscious users opt in.
    pub redact_pasted: bool,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            popup_rows: 5,
            popup_debounce_ms: 150,
            min_prefix_len: 1,
            redact_pasted: false,
        }
    }
}

/// Guake-style drop-down preferences. When `enabled`, the
/// `toggle_guake` action toggles the window between a "dropped-down"
/// state (sized to the configured fraction of the monitor, anchored
/// to one edge, raised above other windows) and a hidden / minimised
/// state. The intent is `tmux popup`-style quick access — same
/// virtual desktop, no app switching.
///
/// Notes:
/// - On Wayland the compositor owns positioning, so `position`
///   degrades to `set_maximized(true)` for "top" / "full". X11 /
///   Windows / macOS honour `set_outer_position` and pin the window
///   to the requested edge.
/// - A true system-wide hotkey (F12 when rterm is not focused)
///   needs a platform-specific global-shortcut binding that this
///   crate intentionally doesn't pull in. The `toggle_guake` action
///   works the same way other actions do — bind it to a key in
///   `[[keybindings]]` and it fires while rterm is focused.
///   Restoring the window from a minimised state requires either
///   the desktop's window-pick hotkey, a system tray click, or a
///   WM shortcut that launches `rterm` (which then targets the
///   already-running instance via the OS focus stealing rules).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuakeConfig {
    /// When `true`, `toggle_guake` runs without a warning. When
    /// `false` (the default), the action still runs the first time it
    /// is invoked but the renderer logs one `info!` line nudging the
    /// user to flip the flag — this preserves the explicit opt-in
    /// signal without leaving a bound action silently no-op'd. The
    /// `[guake]` layout settings (`position`, `height_pct`,
    /// `width_pct`, `global_hotkey`) are honoured regardless.
    pub enabled: bool,
    /// Which edge the dropped window anchors to. `"top"` (default),
    /// `"bottom"`, or `"full"` (full-screen overlay).
    pub position: String,
    /// Fraction of the monitor's height the window occupies when
    /// dropped. Honoured only for `position = "top" | "bottom"`;
    /// `"full"` ignores this and takes the whole screen. Clamped to
    /// `[10, 100]` at runtime. Default `50`.
    pub height_pct: u8,
    /// Fraction of the monitor's width. Default `100`. Clamped to
    /// `[20, 100]`.
    pub width_pct: u8,
    /// OS-level global hotkey that fires `toggle_guake` even when the
    /// rterm window is NOT focused. Uses the same syntax as
    /// `[[keybindings]].keys` (`"F11"`, `"Ctrl+Shift+`"`, ...).
    ///
    /// Currently implemented on Windows via `RegisterHotKey`. On
    /// Linux / macOS the field is parsed and stored but the
    /// hot-key worker logs `warn!` once at startup and falls back to
    /// the in-app-only binding — the system surfaces (XGrabKey,
    /// composer-specific Wayland protocol, RegisterEventHotKey)
    /// require separate per-platform backends that are not yet
    /// wired up.
    ///
    /// Empty / unset = no global hotkey; the in-app binding from
    /// `[[keybindings]]` still works.
    #[serde(default)]
    pub global_hotkey: String,
}

impl Default for GuakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            position: "top".to_string(),
            height_pct: 50,
            width_pct: 100,
            global_hotkey: String::new(),
        }
    }
}

/// Live appearance preferences (theme name + future visual prefs). Kept
/// separate from `[colors]` so users can pick a built-in theme by name
/// without enumerating 16 palette slots.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    /// Canonical built-in theme name (`default`, `dracula`, `nord`, …).
    /// Empty string means "no preference" — App falls back to the
    /// `[colors]` overrides (if any) or the built-in default palette.
    /// Writeable by the cycle-theme action so the user's pick survives
    /// across restarts.
    pub theme: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// Maximum number of lines retained in each pane's scrollback ring.
    /// Memory cost is roughly `lines * cols * 16` bytes.
    pub scrollback: usize,
    /// When true, dump the focused pane's scrollback to
    /// `$XDG_CACHE_HOME/rterm/scrollback-<ts>.txt` automatically on exit
    /// (mirrors the manual `save_scrollback` action).
    pub save_scrollback_on_exit: bool,
    /// When true, write each tab's focused-pane cwd to
    /// `$XDG_CACHE_HOME/rterm/session.toml` on exit and restore the same
    /// list on the next startup. Layouts (splits) are not preserved — each
    /// saved tab restores as a single pane.
    pub restore_session: bool,
    /// When true, any new shell output snaps the focused pane's view back
    /// to live (`scroll_offset = 0`), overriding the default behaviour of
    /// keeping the user anchored to the line they were reading. Useful for
    /// tail-like workflows.
    pub scroll_on_output: bool,
    /// Master cursor-blink toggle. When `false` the cursor is drawn steady
    /// regardless of the shell's DECSCUSR style request. Default `true`.
    pub cursor_blink: bool,
    /// Whether to draw the slim right-edge scrollbar indicator. `false`
    /// hides it entirely (saves a couple of px). Default `true`.
    pub show_scrollbar: bool,
    /// Milliseconds of inactivity before a tab that previously produced
    /// output fires the edge-triggered `tab.silence` event. Plugins can
    /// hook the event to ping the user when a long-running command in a
    /// background tab finishes. Default `5000`.
    pub tab_silence_ms: u64,
    /// Whether a BEL byte (or `rterm.bell()`) triggers the on-screen
    /// flash. Some users find the flash distracting and prefer the
    /// taskbar urgency hint alone; set to `false` to disable. Default
    /// `true`.
    pub bell_visual: bool,
    /// Whether a BEL pings the window manager (taskbar urgency hint /
    /// dock badge) when the rterm window is unfocused. Useful for
    /// "ping me when this build finishes" flows but can be noisy on
    /// some desktops; set to `false` to disable. Default `true`.
    pub bell_urgent: bool,
    /// Threshold in milliseconds: when an OSC 133;D-tagged command takes
    /// at least this long, fire the edge-triggered `pane.slow_command`
    /// plugin event and (when the rterm window is unfocused) ping the
    /// taskbar. Pairs with shell integration to give "ping me when this
    /// build finishes" semantics without a manual `notify-send` step.
    /// `0` disables the feature. Default `10000` (10 s).
    pub slow_command_ms: u64,
    /// OSC 52 clipboard-write policy. Some terminals accept OSC 52
    /// silently; that lets a malicious shell (or anything piping into
    /// `less`) overwrite the system clipboard before the user pastes,
    /// which is a real-world phishing primitive. Default is `false`
    /// (deny) to match xterm and modern peer terminals (kitty / wezterm
    /// / Alacritty all gate this). Set to `true` if you rely on tmux /
    /// mosh / SSH-forwarded clipboard.
    pub allow_osc52: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback: 10_000,
            save_scrollback_on_exit: false,
            restore_session: false,
            scroll_on_output: false,
            cursor_blink: true,
            show_scrollbar: true,
            tab_silence_ms: 5_000,
            bell_visual: true,
            bell_urgent: true,
            slow_command_ms: 10_000,
            allow_osc52: false,
        }
    }
}

/// Per-named-colour overrides. Each entry is an RGB byte triple; `None`
/// keeps the built-in xterm default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    /// Fixed cursor block colour; when None, the cursor uses the inverted
    /// cell fg (xterm-style).
    pub cursor: Option<[u8; 3]>,
    pub black: Option<[u8; 3]>,
    pub red: Option<[u8; 3]>,
    pub green: Option<[u8; 3]>,
    pub yellow: Option<[u8; 3]>,
    pub blue: Option<[u8; 3]>,
    pub magenta: Option<[u8; 3]>,
    pub cyan: Option<[u8; 3]>,
    pub white: Option<[u8; 3]>,
    pub bright_black: Option<[u8; 3]>,
    pub bright_red: Option<[u8; 3]>,
    pub bright_green: Option<[u8; 3]>,
    pub bright_yellow: Option<[u8; 3]>,
    pub bright_blue: Option<[u8; 3]>,
    pub bright_magenta: Option<[u8; 3]>,
    pub bright_cyan: Option<[u8; 3]>,
    pub bright_white: Option<[u8; 3]>,
}

/// WindTerm-style client-side syntax highlighting of terminal output.
/// A set of regex rules recolours matched text; the renderer applies
/// the colour only to cells that still carry the DEFAULT foreground, so
/// highlighting is purely additive over output a program already
/// coloured (`ls --color`, `bat`, `git`, TUIs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightConfig {
    /// Master switch. `false` disables highlighting entirely.
    pub enabled: bool,
    /// Include the built-in rule set (URLs, IPv4, UUID, hex, log
    /// levels, quoted strings, numbers). Set `false` to start from a
    /// clean slate and define everything via `rules`.
    pub builtins: bool,
    /// User rules, evaluated BEFORE the built-ins (first-match-wins per
    /// column), so a rule here overrides a built-in on the same text.
    pub rules: Vec<HighlightRule>,
}

impl Default for HighlightConfig {
    fn default() -> Self {
        Self { enabled: true, builtins: true, rules: Vec::new() }
    }
}

/// One highlight rule: a regex `pattern`, a foreground colour (`fg`, as
/// `#RRGGBB` / `#RGB` / a colour name), and optional `bold`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightRule {
    pub pattern: String,
    pub fg: String,
    pub bold: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    pub program: Option<String>,
    pub args: Vec<String>,
    /// Extra environment variables passed to the spawned shell. Applied
    /// *after* the built-in `TERM=xterm-256color` / `COLORTERM=truecolor`
    /// defaults so user entries override on key collision. Empty by
    /// default. Common use cases: `RUST_BACKTRACE = "1"`,
    /// `LANG = "en_US.UTF-8"`, `EDITOR = "nvim"`. Inherited parent env
    /// stays intact — these are additive overrides, not a replacement.
    pub env: std::collections::BTreeMap<String, String>,
}

/// A saved connection / launch preset — a named shell command plus its
/// working directory, environment and theme. Declared as repeated
/// `[[profiles]]` blocks. Opened via `rterm --profile <name>` or the
/// "New tab with profile…" command-palette entry. The classic SSH case
/// is `program = "ssh"`, `args = ["user@host"]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileConfig {
    /// Unique key used to select the profile (`--profile <name>`).
    pub name: String,
    /// Command to run instead of the default `[shell] program`. When
    /// unset, the profile reuses the default shell (useful for a profile
    /// that only overrides `cwd` / `theme` / `env`).
    pub program: Option<String>,
    /// Arguments for `program` (e.g. `["user@host"]` for an SSH profile).
    pub args: Vec<String>,
    /// Working directory to start in. `~` is expanded by the app.
    pub cwd: Option<String>,
    /// Extra environment variables (additive, like `[shell] env`).
    pub env: std::collections::BTreeMap<String, String>,
    /// Built-in theme name to apply when the profile opens (e.g.
    /// `"dark"`, `"solarized-light"`). `None` keeps the current theme.
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    pub family: String,
    pub size: f32,
    /// When true (xterm default), SGR BOLD also brightens the 8 unbright
    /// named ANSI colours to their bright variant. Set to `false` for
    /// "stop yelling at me" themes.
    pub bold_is_bright: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub width: u32,
    pub height: u32,
    pub opacity: f32,
    /// Whether the platform's window manager draws the title bar and
    /// borders. `false` (default) makes rterm own the entire window
    /// chrome — matches the Chrome/Firefox look. Set `true` to fall
    /// back to native decorations on tiling WMs or when the in-app
    /// drag/resize affordances feel off.
    pub os_decorations: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    pub keys: String,
    pub action: String,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            // Empty = let the renderer auto-pick a preferred installed
            // monospace face (see `default_monospace_family`). Mirrors
            // the bundled `default.toml` template — keeps `--check`
            // from flagging the CSS-generic `"monospace"` fallback as
            // "not installed" on a config-less first run.
            family: String::new(),
            size: 13.0,
            bold_is_bright: true,
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { width: 1024, height: 640, opacity: 1.0, os_decorations: false }
    }
}

/// Language for the auto-generated `config.toml` comments.
///
/// The TOML values themselves are identical across languages — only
/// the surrounding `# …` annotation text differs. The
/// `default_template_matches_default_struct_*` tests pin both
/// languages against the same `Config::default()` snapshot to keep
/// them from drifting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CommentLang {
    /// English (default).
    #[default]
    En,
    /// Russian.
    Ru,
}

impl CommentLang {
    /// Parse a language code as written on the CLI (`--lang en`) or
    /// pulled out of `LANG` / `LC_ALL`. Accepts case-insensitive
    /// ISO 639-1 prefixes (`en`, `en_US.UTF-8`, `EN`, `english` →
    /// English; `ru`, `ru_RU.UTF-8`, `RU`, `russian` → Russian).
    /// Anything else returns `None` so the caller can fall back.
    pub fn parse(raw: &str) -> Option<Self> {
        let head = raw
            .trim()
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("");
        if head.eq_ignore_ascii_case("en") || head.eq_ignore_ascii_case("english") {
            Some(CommentLang::En)
        } else if head.eq_ignore_ascii_case("ru") || head.eq_ignore_ascii_case("russian") {
            Some(CommentLang::Ru)
        } else {
            None
        }
    }

    /// Auto-detect from environment. Priority:
    /// 1. `RTERM_LANG` env var (`en` / `ru`),
    /// 2. `LC_ALL`, then `LANG` (POSIX locale priority),
    /// 3. fall back to `En`.
    ///
    /// First-run template generation calls this so a user with a
    /// `ru_RU.UTF-8` locale gets Russian comments without having to
    /// pass any flags.
    pub fn detect() -> Self {
        for var in ["RTERM_LANG", "LC_ALL", "LANG"] {
            if let Some(raw) = nonempty_env(var).and_then(|s| s.into_string().ok()) {
                if let Some(lang) = Self::parse(&raw) {
                    return lang;
                }
            }
        }
        CommentLang::En
    }
}

/// Commented template written on first run (English version).
const DEFAULT_TEMPLATE_EN: &str = include_str!("default.toml");
/// Commented template written on first run (Russian version).
const DEFAULT_TEMPLATE_RU: &str = include_str!("default.ru.toml");

/// The bundled English `default.toml` text — exposed so downstream
/// crates can keep its comment lists in sync with their own canonical
/// names via tests (e.g. `rterm-app` asserts every
/// `AppAction::canonical_names()` entry is mentioned in the template's
/// actions comment).
///
/// Equivalent to `default_template_for(CommentLang::En)`; kept for
/// backwards compatibility.
pub fn default_template() -> &'static str {
    default_template_for(CommentLang::En)
}

/// Bundled `default.toml` text for the requested language. Both
/// templates encode the same TOML values; only the surrounding
/// comments differ.
pub fn default_template_for(lang: CommentLang) -> &'static str {
    match lang {
        CommentLang::En => DEFAULT_TEMPLATE_EN,
        CommentLang::Ru => DEFAULT_TEMPLATE_RU,
    }
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).context("invalid TOML config")
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::from_toml_str(&s)
    }

    /// Create a commented template at `path` if no config file exists
    /// yet. Idempotent — does nothing when a file is already present.
    /// The comment language is auto-detected from `RTERM_LANG` /
    /// `LC_ALL` / `LANG` (see [`CommentLang::detect`]); pass an
    /// explicit choice via [`Config::ensure_default_with_lang`].
    pub fn ensure_default(path: &Path) -> Result<bool> {
        Self::ensure_default_with_lang(path, CommentLang::detect())
    }

    /// Like [`Config::ensure_default`], but writes the template for the
    /// explicit comment language. Lets callers honour a `--lang` CLI
    /// override without consulting the environment.
    pub fn ensure_default_with_lang(path: &Path, lang: CommentLang) -> Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::write(path, default_template_for(lang))
            .with_context(|| format!("writing default config to {}", path.display()))?;
        Ok(true)
    }

    /// Default config path per platform. Returns `None` if no home dir.
    /// `RTERM_CONFIG_PATH` overrides everything when set and non-empty —
    /// lets a user pin a specific file (multi-profile setups, CI runs,
    /// sandboxes) without having to pass `--config` to every invocation.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(p) = nonempty_env("RTERM_CONFIG_PATH") {
            return Some(PathBuf::from(p));
        }
        let base = nonempty_env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                nonempty_env("HOME").map(|h| {
                    let mut p = PathBuf::from(h);
                    p.push(".config");
                    p
                })
            })
            .or_else(|| nonempty_env("APPDATA").map(PathBuf::from))?;
        Some(base.join("rterm").join("config.toml"))
    }
}

/// Read an environment variable and return it only when set AND non-empty.
/// XDG and POSIX both treat empty values as "unset" for path-prefix vars
/// (e.g. `XDG_CONFIG_HOME=` should mean "use the default", not "use the
/// current working directory") — `std::env::var_os` returns `Some("")`
/// in that case, which would otherwise become `PathBuf::from("")` and
/// produce a relative path like `rterm/config.toml`.
fn nonempty_env(name: &str) -> Option<std::ffi::OsString> {
    let v = std::env::var_os(name)?;
    if v.is_empty() { None } else { Some(v) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise tests that mutate process-wide environment variables.
    /// `cargo test` runs tests in parallel by default; without this
    /// guard two tests calling `set_var("RTERM_CONFIG_PATH", ...)` on
    /// different threads can interleave and observe each other's
    /// values. Acquire it at the top of any test that touches env.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn empty_toml_yields_defaults() {
        let cfg = Config::from_toml_str("").unwrap();
        assert_eq!(cfg.font.size, 13.0);
        assert_eq!(cfg.window.width, 1024);
        // Empty family is the "let the renderer auto-pick" sentinel —
        // pin it so a regression that re-introduces the literal
        // `"monospace"` (the CSS-generic name, which no installed face
        // actually has) gets caught. `--check font.family` warns on
        // names not in `list_monospace_families()`, so the wrong
        // default would flag every config-less user.
        assert_eq!(cfg.font.family, "");
    }

    #[test]
    fn nonempty_env_treats_blank_as_unset() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let key = "RTERM_TEST_NONEMPTY_PROBE";
        // SAFETY: env mutation serialised by ENV_GUARD; this key is
        // ours, no race with other code.
        unsafe { std::env::set_var(key, "") };
        assert!(nonempty_env(key).is_none(), "empty string must look unset");
        unsafe { std::env::set_var(key, "x") };
        assert_eq!(nonempty_env(key).map(|s| s.into_string().unwrap()), Some("x".into()));
        unsafe { std::env::remove_var(key) };
        assert!(nonempty_env(key).is_none(), "absent must look unset");
    }

    #[test]
    fn overrides_apply() {
        let cfg = Config::from_toml_str(r#"
            [font]
            family = "JetBrains Mono"
            size = 14.5
        "#).unwrap();
        assert_eq!(cfg.font.family, "JetBrains Mono");
        assert_eq!(cfg.font.size, 14.5);
    }

    #[test]
    fn terminal_scrollback_parses() {
        let cfg = Config::from_toml_str(r#"
            [terminal]
            scrollback = 50000
        "#).unwrap();
        assert_eq!(cfg.terminal.scrollback, 50_000);
        // And the default still applies when missing.
        let blank = Config::from_toml_str("").unwrap();
        assert_eq!(blank.terminal.scrollback, 10_000);
    }

    #[test]
    fn terminal_toggles_parse() {
        let cfg = Config::from_toml_str(r#"
            [terminal]
            cursor_blink = false
            show_scrollbar = false
            scroll_on_output = true
            save_scrollback_on_exit = true
            restore_session = true
        "#).unwrap();
        assert!(!cfg.terminal.cursor_blink);
        assert!(!cfg.terminal.show_scrollbar);
        assert!(cfg.terminal.scroll_on_output);
        assert!(cfg.terminal.save_scrollback_on_exit);
        assert!(cfg.terminal.restore_session);
    }

    #[test]
    fn tab_silence_ms_parses() {
        let cfg = Config::from_toml_str(r#"
            [terminal]
            tab_silence_ms = 12345
        "#).unwrap();
        assert_eq!(cfg.terminal.tab_silence_ms, 12_345);
        // Default is the 5s threshold matching the renderer's old constant.
        let blank = Config::from_toml_str("").unwrap();
        assert_eq!(blank.terminal.tab_silence_ms, 5_000);
    }

    #[test]
    fn rterm_config_path_env_overrides_default_lookup() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: env mutation serialised by ENV_GUARD; restore via
        // guard so other tests aren't affected.
        let saved = std::env::var_os("RTERM_CONFIG_PATH");
        unsafe { std::env::set_var("RTERM_CONFIG_PATH", "/etc/rterm-special.toml"); }
        let path = Config::default_path().expect("override yields a path");
        assert_eq!(path.to_string_lossy(), "/etc/rterm-special.toml");
        // Empty value is treated as unset (fall through to platform default).
        unsafe { std::env::set_var("RTERM_CONFIG_PATH", ""); }
        let fallback = Config::default_path();
        // Whichever platform path got picked, it must NOT be the empty
        // override.
        if let Some(p) = fallback {
            assert_ne!(p.as_os_str(), "");
        }
        // Restore.
        match saved {
            Some(v) => unsafe { std::env::set_var("RTERM_CONFIG_PATH", v); },
            None => unsafe { std::env::remove_var("RTERM_CONFIG_PATH"); },
        }
    }

    #[test]
    fn default_path_returns_sensible_location() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Whatever the host env: if either XDG_CONFIG_HOME, HOME, or
        // APPDATA is set (true on every supported platform / CI), the
        // path must end in `rterm/config.toml` so the watcher and the
        // ensure_default writer agree on what file to track.
        if std::env::var_os("XDG_CONFIG_HOME").is_some()
            || std::env::var_os("HOME").is_some()
            || std::env::var_os("APPDATA").is_some()
        {
            let path = Config::default_path().expect("at least one path env set");
            let s = path.to_string_lossy();
            assert!(
                s.ends_with("rterm/config.toml") || s.ends_with("rterm\\config.toml"),
                "unexpected default path {s:?}",
            );
        }
    }

    #[test]
    fn shell_env_parses_as_key_value_table() {
        // `[shell.env]` is a TOML inline table or section — both must
        // deserialize into the `BTreeMap`. Pinning so a refactor that
        // accidentally renames the field can't drop the feature silently.
        let cfg = Config::from_toml_str(
            r#"
            [shell.env]
            RUST_BACKTRACE = "1"
            LANG = "en_US.UTF-8"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.shell.env.get("RUST_BACKTRACE").map(|s| s.as_str()), Some("1"));
        assert_eq!(
            cfg.shell.env.get("LANG").map(|s| s.as_str()),
            Some("en_US.UTF-8"),
        );
        // Defaults to empty when omitted — additive override, never
        // wipes the inherited parent env.
        let blank = Config::from_toml_str("").unwrap();
        assert!(blank.shell.env.is_empty());
    }

    #[test]
    fn default_template_is_well_formed_toml_with_trailing_newline() {
        // Two structural properties first-run users depend on:
        //   1. The template ends with `\n`. Without it, `rterm
        //      --print-default-config > config.toml` produces a
        //      file whose last line lacks a newline — editors
        //      flag that and `wc -l` undercounts.
        //   2. Comment lines start with `# `, never tab-indented
        //      `#\t`, so reflowing in a normal editor doesn't
        //      lose the leading space.
        let tpl = DEFAULT_TEMPLATE_EN;
        assert!(
            tpl.ends_with('\n'),
            "DEFAULT_TEMPLATE must end with a newline",
        );
        for (lineno, line) in tpl.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                // Comments may either be a bare `#` (blank
                // comment line) or `# <text>`. Tab-indented
                // comments would be a sign of accidental
                // reformatting.
                assert!(
                    !trimmed.starts_with("#\t"),
                    "line {} starts with tab-indented comment: {:?}",
                    lineno + 1,
                    line,
                );
            }
        }
    }

    #[test]
    fn default_template_parses_and_matches_default_struct() {
        // The commented `default.toml` template is what `ensure_default`
        // writes for first-run users. If its concrete values drift away
        // from `Config::default()`, new users get behaviour that
        // disagrees with what the binary uses when no config exists.
        // Parse the template and compare every field we explicitly set.
        let parsed = Config::from_toml_str(DEFAULT_TEMPLATE_EN)
            .expect("bundled default.toml must always parse");
        let baseline = Config::default();
        // [font]
        assert_eq!(parsed.font.size, baseline.font.size);
        assert_eq!(parsed.font.bold_is_bright, baseline.font.bold_is_bright);
        // Both surfaces must agree on empty `font.family` so first-run
        // (template-on-disk) and config-less (in-memory default) users
        // get identical "let the renderer auto-pick" behaviour. A drift
        // here would re-introduce the spurious `--check font.family`
        // warning the earlier iteration fixed.
        assert_eq!(parsed.font.family, baseline.font.family);
        assert_eq!(parsed.font.family, "");
        // [window]
        assert_eq!(parsed.window.width, baseline.window.width);
        assert_eq!(parsed.window.height, baseline.window.height);
        assert!((parsed.window.opacity - baseline.window.opacity).abs() < f32::EPSILON);
        // [terminal]
        assert_eq!(parsed.terminal.scrollback, baseline.terminal.scrollback);
        assert_eq!(
            parsed.terminal.save_scrollback_on_exit,
            baseline.terminal.save_scrollback_on_exit,
        );
        assert_eq!(parsed.terminal.restore_session, baseline.terminal.restore_session);
        assert_eq!(parsed.terminal.scroll_on_output, baseline.terminal.scroll_on_output);
        assert_eq!(parsed.terminal.cursor_blink, baseline.terminal.cursor_blink);
        assert_eq!(parsed.terminal.show_scrollbar, baseline.terminal.show_scrollbar);
        assert_eq!(parsed.terminal.tab_silence_ms, baseline.terminal.tab_silence_ms);
        assert_eq!(parsed.terminal.bell_visual, baseline.terminal.bell_visual);
        assert_eq!(parsed.terminal.bell_urgent, baseline.terminal.bell_urgent);
        assert_eq!(parsed.terminal.slow_command_ms, baseline.terminal.slow_command_ms);
        // [shell] — the template's example entries are commented out so
        // a fresh-install config produces an empty env map. If a future
        // edit accidentally un-comments one, this test will surface the
        // drift before users wonder why their parent env got shadowed.
        assert!(parsed.shell.env.is_empty(), "[shell.env] should be empty by default");
        assert_eq!(parsed.shell.program, baseline.shell.program);
        assert_eq!(parsed.shell.args, baseline.shell.args);
    }

    #[test]
    fn slow_command_ms_parses_and_default() {
        // 10s default matches the `default.toml` template; explicit value
        // round-trips through TOML.
        let cfg = Config::from_toml_str(r#"
            [terminal]
            slow_command_ms = 30000
        "#).unwrap();
        assert_eq!(cfg.terminal.slow_command_ms, 30_000);
        let blank = Config::from_toml_str("").unwrap();
        assert_eq!(blank.terminal.slow_command_ms, 10_000);
    }

    #[test]
    fn bell_toggles_parse_and_default() {
        // Both `bell_visual` and `bell_urgent` are explicit hot-reloadable
        // switches. Defaults preserve the original "always flash + ping"
        // behaviour so an empty `[terminal]` section gives the historical
        // experience.
        let cfg = Config::from_toml_str(r#"
            [terminal]
            bell_visual = false
            bell_urgent = false
        "#).unwrap();
        assert!(!cfg.terminal.bell_visual);
        assert!(!cfg.terminal.bell_urgent);
        let blank = Config::from_toml_str("").unwrap();
        assert!(blank.terminal.bell_visual);
        assert!(blank.terminal.bell_urgent);
    }

    #[test]
    fn keybindings_parse_from_array() {
        // `default.toml` shows commented `[[keybindings]]` entries; this
        // pins the wire format so a serde rename doesn't silently break
        // existing user configs.
        let cfg = Config::from_toml_str(
            r#"
                [[keybindings]]
                keys = "Ctrl+T"
                action = "new_tab"

                [[keybindings]]
                keys = "F2"
                action = "search"
            "#,
        )
        .expect("parses");
        assert_eq!(cfg.keybindings.len(), 2);
        assert_eq!(cfg.keybindings[0].keys, "Ctrl+T");
        assert_eq!(cfg.keybindings[0].action, "new_tab");
        assert_eq!(cfg.keybindings[1].keys, "F2");
        assert_eq!(cfg.keybindings[1].action, "search");
    }

    #[test]
    fn default_config_round_trips_through_toml_serialize() {
        // `--print-config` runs `toml::to_string_pretty(&Config)` and
        // users sometimes pipe that into a fresh file. The result must
        // re-parse cleanly via `from_toml_str`, and the parsed value
        // must equal the original. A serde rename / missing field that
        // breaks this contract would silently corrupt user backups.
        let baseline = Config::default();
        let dumped = toml::to_string(&baseline).expect("serialize default config");
        let parsed = Config::from_toml_str(&dumped)
            .expect("printed config must re-parse");
        assert_eq!(parsed.font.family, baseline.font.family);
        assert_eq!(parsed.font.size, baseline.font.size);
        assert_eq!(parsed.window.width, baseline.window.width);
        assert_eq!(parsed.terminal.scrollback, baseline.terminal.scrollback);
        assert_eq!(parsed.shell.program, baseline.shell.program);
    }

    #[test]
    fn load_from_missing_path_returns_defaults() {
        // `--config /tmp/nope.toml` (a typo / first-run scenario)
        // should fall through to the in-memory default rather than
        // erroring. The CLI surface relies on this contract: --check
        // and --print-config both call load_from and don't pre-check
        // for existence.
        let path = PathBuf::from("/tmp/rterm-test-nonexistent-config-xxxxxxxx.toml");
        // Sanity: actually make sure the path doesn't exist.
        assert!(!path.exists());
        let cfg = Config::load_from(&path).expect("missing → defaults");
        let baseline = Config::default();
        assert_eq!(cfg.font.size, baseline.font.size);
        assert_eq!(cfg.window.width, baseline.window.width);
    }

    #[test]
    fn profiles_parse_and_resolve() {
        let toml = r#"
[[profiles]]
name = "myssh"
program = "ssh"
args = ["user@host"]
theme = "dark"

[profiles.env]
LANG = "en_US.UTF-8"

[[profiles]]
name = "logs"
cwd = "/var/log"
"#;
        let cfg = Config::from_toml_str(toml).unwrap();
        assert_eq!(cfg.profiles.len(), 2);
        let ssh = cfg.profile("myssh").expect("myssh profile");
        assert_eq!(ssh.program.as_deref(), Some("ssh"));
        assert_eq!(ssh.args, vec!["user@host".to_string()]);
        assert_eq!(ssh.theme.as_deref(), Some("dark"));
        assert_eq!(ssh.env.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
        // A profile that only overrides cwd keeps program unset.
        let logs = cfg.profile("logs").expect("logs profile");
        assert_eq!(logs.cwd.as_deref(), Some("/var/log"));
        assert!(logs.program.is_none());
        assert!(cfg.profile("nope").is_none());
    }

    #[test]
    fn config_round_trips_through_toml() {
        // Validates that `--print-config` produces a TOML that parses back
        // to an equivalent Config — catches accidental schema drift where
        // Serialize emits something Deserialize won't accept.
        let original = Config::default();
        let serialized = toml::to_string_pretty(&original).unwrap();
        let reparsed = Config::from_toml_str(&serialized).unwrap();
        assert_eq!(reparsed.font.size, original.font.size);
        assert_eq!(reparsed.window.width, original.window.width);
        assert_eq!(reparsed.terminal.scrollback, original.terminal.scrollback);
        assert_eq!(reparsed.font.bold_is_bright, original.font.bold_is_bright);
        assert_eq!(reparsed.terminal.cursor_blink, original.terminal.cursor_blink);
        // Recent additions — pin them too so a future serde renames don't
        // silently break `--print-config` output.
        assert_eq!(reparsed.terminal.tab_silence_ms, original.terminal.tab_silence_ms);
        assert_eq!(reparsed.terminal.show_scrollbar, original.terminal.show_scrollbar);
        assert_eq!(reparsed.terminal.scroll_on_output, original.terminal.scroll_on_output);
        assert_eq!(
            reparsed.terminal.save_scrollback_on_exit,
            original.terminal.save_scrollback_on_exit,
        );
        assert_eq!(reparsed.terminal.restore_session, original.terminal.restore_session);
        assert_eq!(reparsed.terminal.bell_visual, original.terminal.bell_visual);
        assert_eq!(reparsed.terminal.bell_urgent, original.terminal.bell_urgent);
        assert_eq!(reparsed.terminal.slow_command_ms, original.terminal.slow_command_ms);
    }

    #[test]
    fn comment_lang_parse_accepts_iso_and_locale_forms() {
        // Both `en` (CLI shorthand) and `en_US.UTF-8` (POSIX LANG)
        // must resolve to English; same for Russian. The function
        // tolerates leading whitespace + trailing locale modifiers
        // since real-world `LANG` values include both. Names like
        // `english` / `russian` are accepted as a developer-friendly
        // alias.
        assert_eq!(CommentLang::parse("en"), Some(CommentLang::En));
        assert_eq!(CommentLang::parse("EN"), Some(CommentLang::En));
        assert_eq!(CommentLang::parse("en_US.UTF-8"), Some(CommentLang::En));
        assert_eq!(CommentLang::parse(" english "), Some(CommentLang::En));
        assert_eq!(CommentLang::parse("ru"), Some(CommentLang::Ru));
        assert_eq!(CommentLang::parse("RU"), Some(CommentLang::Ru));
        assert_eq!(CommentLang::parse("ru_RU.UTF-8"), Some(CommentLang::Ru));
        assert_eq!(CommentLang::parse("russian"), Some(CommentLang::Ru));
        // Unknown / unsupported languages fall back to None so the
        // caller can decide (most call sites pick English).
        assert!(CommentLang::parse("fr_FR.UTF-8").is_none());
        assert!(CommentLang::parse("").is_none());
        assert!(CommentLang::parse("C").is_none());
    }

    #[test]
    fn comment_lang_detect_priority_rterm_lang_wins() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Snapshot + clear so the test is deterministic regardless of
        // the host locale.
        let saved = (
            std::env::var_os("RTERM_LANG"),
            std::env::var_os("LC_ALL"),
            std::env::var_os("LANG"),
        );
        // SAFETY: env mutation is serialised by ENV_GUARD; restored
        // via the snapshot at the end of the test.
        unsafe {
            std::env::remove_var("RTERM_LANG");
            std::env::remove_var("LC_ALL");
            std::env::remove_var("LANG");
        }
        // No env at all → English fallback.
        assert_eq!(CommentLang::detect(), CommentLang::En);
        // `LANG` is honoured.
        unsafe { std::env::set_var("LANG", "ru_RU.UTF-8") };
        assert_eq!(CommentLang::detect(), CommentLang::Ru);
        // `LC_ALL` wins over `LANG` (POSIX priority).
        unsafe { std::env::set_var("LC_ALL", "en_US.UTF-8") };
        assert_eq!(CommentLang::detect(), CommentLang::En);
        // `RTERM_LANG` wins over both (CLI / user intent).
        unsafe { std::env::set_var("RTERM_LANG", "ru") };
        assert_eq!(CommentLang::detect(), CommentLang::Ru);

        // Restore the host's original state.
        unsafe {
            match saved.0 {
                Some(v) => std::env::set_var("RTERM_LANG", v),
                None => std::env::remove_var("RTERM_LANG"),
            }
            match saved.1 {
                Some(v) => std::env::set_var("LC_ALL", v),
                None => std::env::remove_var("LC_ALL"),
            }
            match saved.2 {
                Some(v) => std::env::set_var("LANG", v),
                None => std::env::remove_var("LANG"),
            }
        }
    }

    #[test]
    fn default_template_for_returns_distinct_bilingual_text() {
        let en = default_template_for(CommentLang::En);
        let ru = default_template_for(CommentLang::Ru);
        // The two templates SHOULD differ (otherwise we shipped the
        // same file twice). Cheap sanity check.
        assert_ne!(en, ru, "EN and RU templates must not be identical");
        // English headline appears only in the EN template; same for
        // a Russian-specific Cyrillic word in the RU one. Catches an
        // accidental swap of `include_str!` paths.
        assert!(en.contains("English comments"));
        assert!(!en.contains("русские комментарии"));
        assert!(ru.contains("русские комментарии"));
        assert!(!ru.contains("English comments"));
    }

    #[test]
    fn ru_template_parses_and_matches_default_struct() {
        // Same invariant `default_template_parses_and_matches_default_
        // struct` enforces for the EN template — the Russian template
        // must also decode to a `Config` that equals
        // `Config::default()` for every key it sets explicitly.
        // Without this, a `ru_RU` first-run user would silently get
        // different defaults than every other locale.
        let parsed = Config::from_toml_str(DEFAULT_TEMPLATE_RU)
            .expect("bundled default.ru.toml must always parse");
        let baseline = Config::default();
        assert_eq!(parsed.font.size, baseline.font.size);
        assert_eq!(parsed.font.family, baseline.font.family);
        assert_eq!(parsed.font.bold_is_bright, baseline.font.bold_is_bright);
        assert_eq!(parsed.window.width, baseline.window.width);
        assert_eq!(parsed.window.height, baseline.window.height);
        assert!((parsed.window.opacity - baseline.window.opacity).abs() < f32::EPSILON);
        assert_eq!(parsed.window.os_decorations, baseline.window.os_decorations);
        assert_eq!(parsed.terminal.scrollback, baseline.terminal.scrollback);
        assert_eq!(parsed.terminal.cursor_blink, baseline.terminal.cursor_blink);
        assert_eq!(parsed.terminal.show_scrollbar, baseline.terminal.show_scrollbar);
        assert_eq!(parsed.terminal.allow_osc52, baseline.terminal.allow_osc52);
        assert_eq!(parsed.appearance.theme, baseline.appearance.theme);
        assert_eq!(parsed.guake.enabled, baseline.guake.enabled);
        assert_eq!(parsed.guake.position, baseline.guake.position);
        assert_eq!(parsed.guake.height_pct, baseline.guake.height_pct);
        assert_eq!(parsed.guake.width_pct, baseline.guake.width_pct);
        assert_eq!(parsed.guake.global_hotkey, baseline.guake.global_hotkey);
    }

    #[test]
    fn ensure_default_with_lang_writes_the_requested_template() {
        // Pick a unique tmpdir per test invocation so parallel runs
        // don't clobber each other. Both branches must produce a file
        // that round-trips through `Config::from_toml_str`.
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "rterm-test-ensure-default-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        for lang in [CommentLang::En, CommentLang::Ru] {
            let path = dir.join(format!("{:?}.toml", lang));
            let created = Config::ensure_default_with_lang(&path, lang)
                .expect("ensure_default_with_lang");
            assert!(created, "ensure_default should report 'created'");
            let written = std::fs::read_to_string(&path).expect("read back");
            assert_eq!(written, default_template_for(lang));
            // Idempotent: a second call sees the file and returns false.
            let again = Config::ensure_default_with_lang(&path, lang)
                .expect("ensure_default_with_lang idempotent");
            assert!(!again);
        }
        // Tidy up so /tmp doesn't accumulate stragglers across runs.
        let _ = std::fs::remove_dir_all(&dir);
    }
}
