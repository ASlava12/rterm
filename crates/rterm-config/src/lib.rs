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

/// Commented template written on first run.
const DEFAULT_TEMPLATE: &str = include_str!("default.toml");

/// The bundled `default.toml` text — exposed so downstream crates can
/// keep its comment lists in sync with their own canonical names via
/// tests (e.g. `rterm-app` asserts every `AppAction::canonical_names()`
/// entry is mentioned in the template's actions comment).
pub fn default_template() -> &'static str {
    DEFAULT_TEMPLATE
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

    /// Create a commented template at `path` if no config file exists yet.
    /// Idempotent — does nothing when a file is already present.
    pub fn ensure_default(path: &Path) -> Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::write(path, DEFAULT_TEMPLATE)
            .with_context(|| format!("writing default config to {}", path.display()))?;
        Ok(true)
    }

    /// Default config path per platform. Returns `None` if no home dir.
    /// `RTERM_CONFIG_PATH` overrides everything when set and non-empty —
    /// lets a user pin a specific file (multi-profile setups, CI runs,
    /// sandboxes) without having to pass `--config` to every invocation.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(override_path) = std::env::var_os("RTERM_CONFIG_PATH") {
            if !override_path.is_empty() {
                return Some(PathBuf::from(override_path));
            }
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| {
                    let mut p = PathBuf::from(h);
                    p.push(".config");
                    p
                })
            })
            .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))?;
        Some(base.join("rterm").join("config.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // SAFETY: env mutation in single-threaded test code; restore via
        // guard so other tests aren't affected. Same pattern as
        // `abbreviate_home_replaces_prefix` over in rterm-render.
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
        let tpl = DEFAULT_TEMPLATE;
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
        let parsed = Config::from_toml_str(DEFAULT_TEMPLATE)
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
}
