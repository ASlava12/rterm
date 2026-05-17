//! Trait through which the renderer surfaces side-channel events
//! to the plugin host (rterm-plugin) and pulls plugin-side
//! commands back. Implemented by `rterm-app` against
//! `rterm_plugin::PluginHost`; the renderer itself is plugin-
//! agnostic — it only sees this trait.
//!
//! Every method has a default no-op so an embedder can plug in
//! a partial impl (or `NullSink` for tests) without spelling out
//! the entire surface.

use crate::{GuakeRunConfig, TerminalSnapshot};

/// Side channel for plugin events. The renderer fires events without knowing
/// anything about the Lua host; rterm-app implements this against PluginHost.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &str, payload: &str);
    /// Names of plugin-registered actions to surface in the command palette.
    fn list_actions(&self) -> Vec<String> {
        Vec::new()
    }
    /// Run a plugin-registered action by name. No-op if unknown.
    fn run_action(&self, _name: &str) {}
    /// Drain queued addressed input from `rterm.send_to_pane(...)`. The
    /// `(tab, pane)` indices are 0-based; out-of-range entries are
    /// silently dropped by the App.
    fn drain_pending_routed_input_by_uid(&self) -> Vec<(u64, Vec<u8>)> {
        Vec::new()
    }
    fn drain_pending_routed_input(&self) -> Vec<((usize, usize), Vec<u8>)> {
        Vec::new()
    }
    /// Drain queued tab-title overrides from `rterm.set_tab_title` /
    /// `rterm.set_tab_title_by_index`. Each entry is
    /// `(Option<tab_0based>, name)`: `None` targets the active tab,
    /// `Some(i)` a specific tab. Empty `name` clears the override.
    fn drain_pending_tab_titles(&self) -> Vec<(Option<usize>, String)> {
        Vec::new()
    }
    /// Latest `rterm.set_window_title` override; outer `Some` means an
    /// update was requested, inner `None` clears any existing override.
    fn take_pending_window_title(&self) -> Option<Option<String>> {
        None
    }
    /// Drain `(tab, pane, title)` overrides from `rterm.set_pane_title`.
    /// 0-based indices. An empty title clears the pane's dynamic title.
    fn drain_pending_pane_titles_by_uid(&self) -> Vec<(u64, String)> {
        Vec::new()
    }
    fn drain_pending_pane_titles(&self) -> Vec<(usize, usize, String)> {
        Vec::new()
    }
    /// Most-recent scrollback-limit override from `rterm.set_scrollback(N)`,
    /// or `None` if no Lua call has fired since last frame.
    fn take_pending_scrollback_limit(&self) -> Option<usize> {
        None
    }
    /// Most-recent silence-threshold override (milliseconds), or `None`.
    /// The config watcher publishes this on every TOML reload so editing
    /// `terminal.tab_silence_ms` takes effect without a restart.
    fn take_pending_tab_silence_ms(&self) -> Option<u64> {
        None
    }
    /// Hot-reloadable `terminal.cursor_blink`. None = no change requested.
    fn take_pending_cursor_blink(&self) -> Option<bool> {
        None
    }
    /// Hot-reloadable `terminal.show_scrollbar`.
    fn take_pending_show_scrollbar(&self) -> Option<bool> {
        None
    }
    /// Hot-reloadable `terminal.scroll_on_output`.
    fn take_pending_scroll_on_output(&self) -> Option<bool> {
        None
    }
    /// Hot-reloadable `terminal.bell_visual`. `false` suppresses the
    /// on-screen flash; the `bell` plugin event still fires.
    fn take_pending_bell_visual(&self) -> Option<bool> {
        None
    }
    /// Hot-reloadable `terminal.bell_urgent`. `false` suppresses the
    /// WM taskbar urgency ping when the rterm window is unfocused.
    fn take_pending_bell_urgent(&self) -> Option<bool> {
        None
    }
    /// Hot-reloadable `[guake]` snapshot. `Some(None)` means the user
    /// turned the feature off (`enabled = false`); `Some(Some(...))`
    /// installs new config. `None` means no change since last drain.
    fn take_pending_guake(&self) -> Option<Option<GuakeRunConfig>> {
        None
    }
    /// Hot-reloadable `[font].family` override. Empty string means
    /// "auto-pick the system's preferred monospace face". `None`
    /// means no change since last drain.
    fn take_pending_font_family(&self) -> Option<String> {
        None
    }
    /// Drain `(tab, pane, muted)` requests from
    /// `rterm.set_pane_bell_muted` (0-based indices). The App writes
    /// each entry into the matching pane's `bell_muted` flag so the
    /// next BEL is either silently consumed or normally announced.
    fn drain_pending_pane_bell_mute(&self) -> Vec<(usize, usize, bool)> {
        Vec::new()
    }
    /// Drain `(uid, muted)` requests from
    /// `rterm.set_pane_bell_muted_by_uid`. App resolves uid → live
    /// (tab, pane) by walking the tab tree; entries for vanished
    /// panes are silently dropped.
    fn drain_pending_pane_bell_mute_by_uid(&self) -> Vec<(u64, bool)> {
        Vec::new()
    }
    /// Hot-reloadable `terminal.slow_command_ms` threshold.
    fn take_pending_slow_command_ms(&self) -> Option<u64> {
        None
    }
    /// True if `rterm.attention()` was called since the last drain — the
    /// App treats it like an OSC 9 / bell and pings the taskbar.
    fn take_pending_attention(&self) -> bool {
        false
    }
    /// Drain plugin-emitted commands from the unified channel.
    /// Callers `match` on `PluginCmd` variants — there's no
    /// per-purpose `drain_pending_X` anymore (the legacy queues
    /// are being folded into this channel; new variants land here
    /// as their queues migrate). Order across variants is
    /// preserved in `PluginHost`'s sender.
    fn drain_pending_commands(&self) -> Vec<rterm_core::PluginCmd> {
        Vec::new()
    }
    /// Latest `(tab, pane)` focus request (0-based) from `rterm.focus_pane`.
    fn take_pending_focus(&self) -> Option<(usize, usize)> {
        None
    }
    /// Latest uid focus request from `rterm.focus_pane_by_uid(uid)`. App
    /// walks live panes to map this to `(tab, pane)`.
    fn take_pending_focus_by_uid(&self) -> Option<u64> {
        None
    }
    /// Latest 0-based tab index from `rterm.focus_tab(idx)`.
    fn take_pending_tab_focus(&self) -> Option<usize> {
        None
    }
    /// Latest plugin-requested clipboard text from `rterm.copy(text)`.
    fn take_pending_copy(&self) -> Option<String> {
        None
    }
    /// Latest logical line target from `rterm.scroll_to_line(line)`.
    fn take_pending_scroll_to_line(&self) -> Option<usize> {
        None
    }
    /// Latest `(query, regex_mode)` from `rterm.start_search`.
    fn take_pending_start_search(&self) -> Option<(String, bool)> {
        None
    }
    /// Latest absolute font size from `rterm.set_font_size(size)`.
    fn take_pending_font_size(&self) -> Option<f32> {
        None
    }
    /// Latest absolute opacity from `rterm.set_opacity(value)`. Already
    /// clamped to `0.0..=1.0`.
    fn take_pending_opacity(&self) -> Option<f32> {
        None
    }
    /// True if `rterm.bell()` was called since the last drain — App fires
    /// the standard bell flash + attention ping.
    fn take_pending_bell(&self) -> bool {
        false
    }
    /// Plugin-supplied full palette swap from `rterm.set_palette`. Returns
    /// `(default_fg, default_bg, cursor, named)`, each optional; an entry
    /// at `None` means "keep current value".
    #[allow(clippy::type_complexity)]
    fn take_pending_palette(
        &self,
    ) -> Option<(
        Option<[u8; 3]>,
        Option<[u8; 3]>,
        Option<[u8; 3]>,
        Option<[[u8; 3]; 16]>,
    )> {
        None
    }
    /// Plugin-supplied built-in theme switch from `rterm.set_theme(name)`.
    /// Returns the canonical theme name to install (one of the entries from
    /// `palette::builtin_themes()`). `None` if nothing queued.
    fn take_pending_theme(&self) -> Option<String> {
        None
    }
    /// Update plugin-visible terminal state snapshot (cwd, title, size).
    fn set_terminal_state(&self, _snap: TerminalSnapshot) {}
    /// `(rule_name, capture_groups)` for every rule that fired against
    /// `line`. For substring rules the capture list is empty; for regex
    /// rules it's the numbered groups (1..) of the first regex match. The
    /// App emits one `match` event per hit. Default = no rules.
    fn match_output_line(&self, _line: &str) -> Vec<(String, Vec<String>)> {
        Vec::new()
    }
}

/// No-op sink for tests or for embedders that don't want plugins.
pub struct NullSink;
impl EventSink for NullSink {
    fn emit(&self, _: &str, _: &str) {}
}
