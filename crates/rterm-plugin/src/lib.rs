//! Lua plugin host.
//!
//! Plugins live in `~/.config/rterm/plugins/*.lua` (or are loaded from
//! `init.lua`). Each plugin gets a sandboxed-ish `rterm` table exposing the
//! host API. Plugins register handlers for events (key, output, command,
//! window-focus, etc) via `rterm.on(event, fn)`.
//!
//! This module gives a minimal skeleton: a `PluginHost` that loads scripts,
//! exposes `rterm.log`, and dispatches a single string event. Real API surface
//! grows from here.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mlua::{Function, Lua, RegistryKey, Table, Value};
use regex::Regex;

/// Cap applied to every Lua→App pending queue. A handler that loops
/// `rterm.send_to_pane(...)` a few million times between frames would
/// otherwise grow the queue without bound (the App drains once per
/// frame). 1024 entries comfortably covers legitimate burst usage.
const PLUGIN_QUEUE_CAP: usize = 1024;

/// Per-dispatch execution budget for event handlers and palette
/// actions. Handlers run synchronously on the render thread — without
/// a budget, `while true do end` in any handler freezes the terminal
/// forever with no log and no escape short of `kill`.
const HANDLER_EXEC_BUDGET: Duration = Duration::from_secs(2);

/// Execution budget for whole-script loads (init.lua / plugins/*.lua
/// top level). More generous than the per-handler budget: startup
/// scripts legitimately do file IO and table building.
const SCRIPT_EXEC_BUDGET: Duration = Duration::from_secs(10);

/// Watchdog state shared with the Lua instruction hook. `arm` stores a
/// monotonic deadline before user code runs; the hook (fired every N
/// VM instructions) aborts the chunk with a Lua error once the
/// deadline passes. Stored as millis-since-`epoch` in an atomic so the
/// hook never takes a lock.
struct ExecDeadline {
    epoch: Instant,
    /// Deadline in ms since `epoch`; 0 = disarmed.
    deadline_ms: AtomicU64,
}

impl ExecDeadline {
    fn new() -> Self {
        Self { epoch: Instant::now(), deadline_ms: AtomicU64::new(0) }
    }
    fn arm(&self, budget: Duration) {
        let dl = self
            .epoch
            .elapsed()
            .saturating_add(budget)
            .as_millis()
            .min(u64::MAX as u128) as u64;
        // `.max(1)` keeps an armed zero-budget distinguishable from
        // the disarmed sentinel.
        self.deadline_ms.store(dl.max(1), Ordering::Release);
    }
    fn disarm(&self) {
        self.deadline_ms.store(0, Ordering::Release);
    }
    fn expired(&self) -> bool {
        let dl = self.deadline_ms.load(Ordering::Acquire);
        dl != 0 && self.epoch.elapsed().as_millis() as u64 > dl
    }
}

/// Push to a bounded Lua→App queue, dropping the OLDEST entry past the
/// cap. Right semantics for last-write-wins payloads (titles): the
/// newest value is the one the plugin wants applied.
fn push_capped_drop_oldest<T>(q: &Mutex<VecDeque<T>>, item: T) {
    if let Ok(mut q) = q.lock() {
        if q.len() >= PLUGIN_QUEUE_CAP {
            q.pop_front();
        }
        q.push_back(item);
    }
}

/// Push to a bounded Lua→App queue, rejecting the NEW entry when full.
/// Right semantics for routed-input streams: silently dropping a chunk
/// from the MIDDLE of queued input corrupts whatever the plugin was
/// typing into the pane — better to refuse the tail and say so.
fn push_capped_reject<T>(q: &Mutex<VecDeque<T>>, item: T) -> bool {
    match q.lock() {
        Ok(mut q) if q.len() < PLUGIN_QUEUE_CAP => {
            q.push_back(item);
            true
        }
        _ => false,
    }
}

/// A snapshot of the focused pane's terminal that the App pushes into the
/// plugin host so Lua callbacks can read it via `rterm.cwd()` etc.
#[derive(Debug, Clone, Default)]
pub struct TerminalState {
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub panes: Vec<PaneInfo>,
    pub tabs: Vec<TabInfo>,
    /// Focused pane's visible grid text, one row per `\n`. Trailing spaces
    /// on each row are trimmed. Updated each frame.
    pub grid_text: String,
    /// Current font size in points.
    pub font_size: f32,
    /// Resolved font family name actually rendering. Empty string when
    /// rterm is on cosmic-text's built-in fallback.
    pub font_family: String,
    /// Per-cell pixel width (probed monospace advance).
    pub cell_width: f32,
    /// Per-line pixel height (baseline-to-baseline).
    pub line_height: f32,
    /// Current `tab.silence` threshold in ms. `0` disables the event.
    pub tab_silence_ms: u64,
    /// Current `pane.slow_command` threshold in ms. `0` disables.
    pub slow_command_ms: u64,
    /// Whether new pane output snaps the viewport back to the live
    /// grid (cancels a user-driven scroll-up). Read via
    /// `rterm.scroll_on_output()`.
    pub scroll_on_output: bool,
    /// Whether the scrollbar is rendered. Read via
    /// `rterm.show_scrollbar()`.
    pub show_scrollbar: bool,
    /// Whether BEL flashes the focused pane. Read via
    /// `rterm.bell_visual()`.
    pub bell_visual: bool,
    /// Whether BEL pings the taskbar when unfocused. Read via
    /// `rterm.bell_urgent()`.
    pub bell_urgent: bool,
    /// Global cursor-blink config flag (the user-config / hot-reload
    /// override). Distinct from `PaneInfo.cursor_blink`, which is
    /// per-pane DECSCUSR. Read via `rterm.cursor_blink()`.
    pub cursor_blink: bool,
    /// The 16 named ANSI palette slots (index 0..=15) currently
    /// active. Read via `rterm.named_palette()`.
    pub named_palette: [[u8; 3]; 16],
    /// 1-based index of the tab being dragged, or `None` when
    /// no drag is in progress. Read via `rterm.dragging_tab()`.
    pub dragging_tab: Option<u32>,
    /// Scrollback ring capacity (lines retained per pane).
    pub scrollback_limit: usize,
    /// Text of the user's current mouse selection, or `None` if there is
    /// no active selection. Updated each frame.
    pub selection_text: Option<String>,
    /// Window opacity (0.0..=1.0). 1.0 means fully opaque.
    pub opacity: f32,
    /// Whether the window currently has keyboard focus.
    pub window_focused: bool,
    /// Most recent shell exit code captured via OSC 133;D (`shell.exit`).
    /// `None` until the shell has finished its first command.
    pub last_exit_code: Option<i32>,
    /// Focused pane's OSC 133;A prompt marks (logical line indices,
    /// scrollback first then grid). Empty when no marks captured.
    pub prompt_mark_lines: Vec<usize>,
    /// Focused pane's OSC 133;C command-start marks.
    pub command_mark_lines: Vec<usize>,
    /// Live default foreground colour (RGB bytes). What the renderer
    /// uses for unstyled cells. Updated each frame so a theme swap via
    /// `set_palette` or OSC 10/11 is reflected by the next Lua read.
    pub theme_fg: [u8; 3],
    /// Live default background colour (RGB bytes).
    pub theme_bg: [u8; 3],
    /// Live cursor colour (RGB bytes). Mirrors `palette::cursor()` —
    /// when no explicit cursor colour is set, this equals `theme_fg`.
    pub theme_cursor: [u8; 3],
    /// Focused pane's recent scrollback as a `\n`-joined string. The
    /// renderer caps this at a fixed line count so the snapshot stays
    /// bounded; empty on alt screen (vim/less). Surfaced via
    /// `rterm.scrollback_text(max_lines?)`.
    pub scrollback_text: String,
    /// True when the in-app search overlay (Ctrl+Shift+F) is open.
    /// Surfaced via `rterm.is_search_active()` so status-line plugins
    /// can branch on search mode.
    pub search_active: bool,
    /// Current search query text. Empty when search is closed or the
    /// query buffer is blank. Surfaced via `rterm.search_query()`.
    pub search_query: String,
    /// 1-based current match index and total count for the search
    /// overlay. `(0, 0)` when search is closed or has no matches.
    /// Surfaced via `rterm.search_matches()`.
    pub search_match_index: u32,
    pub search_match_total: u32,
    /// True when search is active in regex mode (toggled via Ctrl+R
    /// inside the overlay). Surfaced via `rterm.search_regex_mode()`.
    pub search_regex_mode: bool,
    /// Canonical name of the active built-in theme (e.g. `"dracula"`).
    /// Empty when the palette was overridden by `set_palette` or via
    /// custom config without matching a built-in. Surfaced via
    /// `rterm.current_theme()`.
    pub active_theme: String,
}

/// Palette snapshot pushed by `rterm.set_palette` (full theme swap). Any
/// field left at its default keeps the current value when the App applies.
#[derive(Debug, Clone, Default)]
pub struct PluginPalette {
    pub default_fg: Option<[u8; 3]>,
    pub default_bg: Option<[u8; 3]>,
    pub cursor: Option<[u8; 3]>,
    pub named: Option<[[u8; 3]; 16]>,
}

/// Lightweight info about a tab, exposed via `rterm.tabs()`.
#[derive(Debug, Clone, Default)]
pub struct TabInfo {
    pub idx: usize,
    pub focused: bool,
    pub pane_count: usize,
    pub focused_pane: usize,
    /// Stable uid of the focused pane within this tab. Survives pane
    /// reorders / closes — plugins addressing "the focused pane of tab
    /// N" should stash this rather than the DFS index when they need
    /// long-term identity. `0` when no pane has focus in the tab.
    pub focused_pane_uid: u64,
    pub zoomed: bool,
    pub custom_title: Option<String>,
    /// Min `idle_ms` across the tab's panes (ms since most-recently-active
    /// pane produced output). `u64::MAX` when no pane has ever printed.
    pub idle_ms: u64,
    /// True if this non-focused tab has produced output since it was last
    /// focused; cleared on focus.
    pub unread: bool,
    /// Aggregate OSC 9;4 progress across this tab's panes. `None` when
    /// no pane has an active report; otherwise `(state, percent)` from
    /// the most-severe pane (error > warn > indeterminate > set,
    /// ties broken by largest percent).
    pub progress: Option<(u8, u8)>,
}

/// Lightweight info about a single pane, exposed to Lua via `rterm.list_panes()`.
#[derive(Debug, Clone, Default)]
pub struct PaneInfo {
    pub tab: usize,
    pub pane: usize,
    /// Stable per-pane identifier. Unlike `(tab, pane)`, the uid
    /// survives reorders / closes — surface it through `list_panes()`
    /// so plugins can capture "the cargo-build pane" once and address
    /// it later via `rterm.set_pane_title_by_uid(uid, "build")`.
    pub uid: u64,
    pub title: String,
    pub focused: bool,
    /// Milliseconds since the pane's PTY last produced output. Plugins use
    /// it for "monitor-silence"-style features.
    pub idle_ms: u64,
    /// Lines scrolled up into scrollback (0 = live).
    pub scroll_offset: u16,
    /// True if the pane is currently on the alternate screen.
    pub alt_screen: bool,
    /// True when DECSCNM (?5) is set — renderer is drawing the
    /// pane with default fg/bg swapped.
    pub reverse_screen: bool,
    /// Pane's shell-advertised working directory (OSC 7), if any.
    pub cwd: Option<String>,
    /// Current grid dimensions of the pane's terminal.
    pub cols: u16,
    pub rows: u16,
    /// 1-based cursor position inside the pane's terminal grid.
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// Cursor shape from DECSCUSR: `"block"`, `"underline"`, or `"bar"`.
    pub cursor_shape: String,
    /// Whether the cursor should blink (low bit of DECSCUSR).
    pub cursor_blink: bool,
    /// Number of scrollback lines retained for this pane.
    pub scrollback_len: usize,
    /// Whether the pane's terminal cursor is visible (DEC ?25).
    pub cursor_visible: bool,
    /// Mouse tracking mode: `"off"`, `"x10"`, `"btn"`, or `"any"`.
    pub mouse_mode: String,
    /// Number of OSC 133;A prompt marks captured so far.
    pub prompt_marks: usize,
    /// Number of OSC 133;C command-start marks captured so far.
    pub command_marks: usize,
    /// OS process id of the shell, if the PTY backend reports one.
    pub pid: Option<u32>,
    /// PID of the current foreground process group on the pane's PTY.
    /// `None` on Windows or when the PTY backend doesn't expose `tcgetpgrp`.
    pub foreground_pgid: Option<u32>,
    /// Executable name of the foreground process (Linux: `/proc/<pgid>/comm`),
    /// e.g. `"vim"` while the user is editing, `"bash"` at a prompt.
    pub foreground_process: Option<String>,
    /// Whether bells from this pane are currently muted. Toggled by
    /// `rterm.set_pane_bell_muted`. When `true`, BEL doesn't fire the
    /// `bell` plugin event or the visual flash.
    pub bell_muted: bool,
    /// Most recent OSC 133;D exit code for the shell in this pane.
    /// `None` until the shell finishes its first command.
    pub last_exit_code: Option<i32>,
    /// Most recent OSC 9;4 progress as `(state, percent)`. `None` until
    /// the pane has received a non-clear progress report.
    pub progress: Option<(u8, u8)>,
    /// Visible grid text for this pane. Same `\n`-joined / trim-trailing
    /// shape as `TerminalState::grid_text` for the focused pane.
    /// Returned by `rterm.terminal_text(tab, pane)`.
    pub text: String,
    /// Capped per-pane scrollback tail (`\n`-joined). The renderer
    /// snapshots a fixed number of recent lines per frame; this is
    /// less than the focused-pane snapshot to keep per-frame
    /// allocation bounded as the pane count grows. Empty on alt
    /// screen. Exposed via `rterm.scrollback_text_of(tab, pane, ...)`.
    pub scrollback_tail: String,
}

pub type ClipboardReader = Arc<dyn Fn() -> Option<String> + Send + Sync>;
type RoutedInputQueue = Arc<Mutex<VecDeque<((usize, usize), Vec<u8>)>>>;
type RoutedInputByUidQueue = Arc<Mutex<VecDeque<(u64, Vec<u8>)>>>;
type TabTitleQueue = Arc<Mutex<VecDeque<(Option<usize>, String)>>>;

/// A pattern Lua plugins can register via `rterm.add_match(name, pattern, opts)`.
/// Each completed shell line is checked against every rule; a hit fires a
/// `match` event with body `<name>\t<line>`. Use regex for capture-group
/// extraction (event handlers can re-evaluate the same pattern Lua-side).
enum MatchKind {
    Substring(String),
    Regex(Regex),
}

struct MatchRule {
    name: String,
    kind: MatchKind,
}

pub struct PluginHost {
    lua: Lua,
    handlers: Arc<Mutex<HashMap<String, Vec<RegistryKey>>>>,
    /// User-registered actions exposed in the command palette.
    /// Map: action name → Lua callback (registered as RegistryKey).
    actions: Arc<Mutex<HashMap<String, RegistryKey>>>,
    /// Unified plugin → app/renderer command channel. The audit
    /// (rounds 2 and 3) flagged the per-purpose `pending_*: Arc<
    /// Mutex<VecDeque<T>>>` shape; this `Sender<PluginCmd>` is the
    /// migration destination. New variants land here first; legacy
    /// queues are folded in as their semantics get matched in
    /// `rterm_core::PluginCmd`. The `Receiver` lives behind a
    /// `Mutex` so the App can drain from any thread (in practice
    /// it's always the render thread).
    ///
    /// `cmd_tx` is intentionally held even though it isn't read
    /// directly from `&self` — Lua closures captured clones at
    /// `new()` time. Keeping the master sender alive gives future
    /// in-process producers (e.g. a Rust-side `enqueue_cmd` API
    /// when more queues migrate) a path that doesn't require
    /// re-plumbing through every closure.
    #[allow(dead_code)]
    cmd_tx: std::sync::mpsc::Sender<rterm_core::PluginCmd>,
    cmd_rx: Arc<Mutex<std::sync::mpsc::Receiver<rterm_core::PluginCmd>>>,
    //
    // === Plugin command queues — architecture note ===
    //
    // Each plugin → app/renderer command has its own
    // `Arc<Mutex<VecDeque<T>>>` below. The audit asked whether this
    // could be a single `Sender<PluginCmd>` channel with one big
    // enum and per-frame `drain + match`. That refactor was
    // **investigated and intentionally deferred** in this round.
    //
    // The blocker is the type's home. The renderer's `EventSink`
    // trait is the receiver of these drains; the plugin host is
    // the producer. `rterm-render` doesn't depend on `rterm-plugin`
    // and vice versa — both only share `rterm-core`. Defining a
    // single `PluginCmd` enum forces either:
    //   - putting plugin-host types into `rterm-core` (breaks
    //     core's "pure data, no I/O" boundary), or
    //   - duplicating the enum in both crates with `From`
    //     conversions at the `rterm-app` seam (ugly twin types).
    //
    // Per-queue Mutex<VecDeque<T>> has been audit-clean since
    // [48c1c07] + [166aa17] (no panics on poison) and the lock
    // contention is bounded — each queue is touched by at most
    // one Lua callback push + one frame-drain pop per tick. The
    // setter batch helper `apply_config_snapshot` [410d612]
    // already covers the worst contention case (11 config-snapshot
    // mutations in one call).
    //
    // Re-evaluate if any of these become true:
    //   * we add a 4th workspace member that needs to share the
    //     command vocabulary (justifies a `rterm-types` crate),
    //   * lock contention shows up in flamegraphs,
    //   * a new command can't fit the legacy `pending_*` shape.
    /// Queue of byte payloads addressed to a specific (tab, pane) pair via
    /// `rterm.send_to_pane(tab, pane, payload)`. Indices are 0-based here;
    /// the Lua API converts from the 1-based form used in `list_panes`.
    pending_routed_input: RoutedInputQueue,
    /// Queue of byte payloads addressed by stable pane uid via
    /// `rterm.send_to_pane_by_uid(uid, payload)`. App resolves each uid
    /// to a live `(tab, pane)` at drain time.
    pending_routed_input_by_uid: RoutedInputByUidQueue,
    /// Set when `rterm.attention()` is called. The App drains it once per
    /// frame and asks the window manager to ping the taskbar.
    pending_attention: Arc<Mutex<bool>>,
    /// Most recent `(tab, pane)` focus request (0-based) from
    /// `rterm.focus_pane(...)`.
    pending_focus: Arc<Mutex<Option<(usize, usize)>>>,
    /// Most recent uid focus request from `rterm.focus_pane_by_uid(uid)`.
    /// App resolves uid → live `(tab, pane)` at drain time.
    pending_focus_by_uid: Arc<Mutex<Option<u64>>>,
    /// `rterm.focus_tab(idx)` request (0-based tab index).
    pending_tab_focus: Arc<Mutex<Option<usize>>>,
    /// Latest text requested for the system clipboard via `rterm.copy(s)`.
    pending_copy: Arc<Mutex<Option<String>>>,
    /// Synchronous clipboard reader injected by the App so `rterm.read_clipboard()`
    /// can return the current system clipboard text from Lua. Unset until the
    /// App installs one — calls then return `nil`.
    clipboard_reader: Arc<Mutex<Option<ClipboardReader>>>,
    /// User config directory (e.g. `~/.config/rterm`), exposed via
    /// `rterm.config_dir()`. Empty until the App installs it.
    config_dir: Arc<Mutex<String>>,
    /// Resolved shell program (path or name). Pushed by main at startup.
    shell_program: Arc<Mutex<String>>,
    /// User cache directory (e.g. `~/.cache/rterm`), exposed via
    /// `rterm.cache_dir()`. Empty until the App installs it.
    cache_dir: Arc<Mutex<String>>,
    /// Built-in action names (e.g. `"new_tab"`), populated by the App.
    /// Surfaced through `rterm.builtin_actions()`.
    builtin_actions: Arc<Mutex<Vec<String>>>,
    /// `canonical_name -> human label` map surfaced through
    /// `rterm.builtin_action_label(name)`. Pushed by `set_builtin_action_labels`
    /// at App startup so plugin-built palettes match the in-app one.
    builtin_action_labels: Arc<Mutex<HashMap<String, String>>>,
    /// Surfaced through `rterm.builtin_events()`.
    builtin_events: Arc<Mutex<Vec<String>>>,
    /// Absolute scroll target from `rterm.scroll_to_line(line)`. 0-based
    /// logical line index (scrollback first, then grid).
    pending_scroll_to_line: Arc<Mutex<Option<usize>>>,
    /// `(query, regex_mode)` from `rterm.start_search`. Empty query opens
    /// the overlay blank — same as the `search` action.
    pending_start_search: Arc<Mutex<Option<(String, bool)>>>,
    /// Latest absolute font size requested by `rterm.set_font_size(size)`.
    pending_font_size: Arc<Mutex<Option<f32>>>,
    /// Latest window-opacity request from `rterm.set_opacity(value)`. App
    /// clamps to `0.0..=1.0` and updates the GPU clear colour. On
    /// compositors that bake transparency into surface creation a change
    /// from opaque (1.0) to translucent at runtime may not affect the
    /// window background — but cell-level alpha tracking still works.
    pending_opacity: Arc<Mutex<Option<f32>>>,
    /// `rterm.bell()` was called — App fires the same visual flash and
    /// attention ping a terminal BEL byte triggers.
    pending_bell: Arc<Mutex<bool>>,
    /// Plugin-supplied full palette swap from `rterm.set_palette(t)`. The
    /// App replaces the live renderer palette with this snapshot.
    pending_palette: Arc<Mutex<Option<PluginPalette>>>,
    /// Plugin-supplied built-in theme switch from `rterm.set_theme(name)`.
    /// Canonical name from `palette::builtin_themes()`. None when no
    /// theme change is queued.
    pending_theme: Arc<Mutex<Option<String>>>,
    /// Queue of tab-title overrides requested by Lua via
    /// `rterm.set_tab_title` / `set_tab_title_by_index`. Each entry is
    /// `(Option<tab_0based>, name)`: `None` targets the active tab (the
    /// historical behaviour), `Some(i)` targets a specific tab.
    /// Empty `name` clears the override.
    pending_tab_titles: TabTitleQueue,
    /// `rterm.set_window_title(name)`. Outer Option = whether the override
    /// was set this frame; inner Option = `None` clears it, `Some(name)`
    /// sets it.
    pending_window_title: Arc<Mutex<Option<Option<String>>>>,
    /// `(tab, pane, title)` overrides from `rterm.set_pane_title`. 0-based.
    /// An empty title clears any current override for that pane.
    pending_pane_titles: Arc<Mutex<VecDeque<(usize, usize, String)>>>,
    /// `(uid, title)` overrides from `rterm.set_pane_title_by_uid`. The
    /// App walks live panes to find the matching uid each frame; a uid
    /// that no longer exists (pane closed) is silently dropped. Empty
    /// title clears the override (same convention as the index-based
    /// setter).
    pending_pane_titles_by_uid: Arc<Mutex<VecDeque<(u64, String)>>>,
    /// Latest scrollback-limit override requested by Lua via
    /// `rterm.set_scrollback`. The App drains and applies to every pane.
    pending_scrollback_limit: Arc<Mutex<Option<usize>>>,
    /// Most-recent `terminal.tab_silence_ms` override published either by
    /// the config watcher on TOML reload or by `rterm.set_tab_silence_ms`.
    /// Drained per frame.
    pending_tab_silence_ms: Arc<Mutex<Option<u64>>>,
    /// Hot-reloadable `terminal.cursor_blink` override published by the
    /// config watcher. Drained per frame.
    pending_cursor_blink: Arc<Mutex<Option<bool>>>,
    /// Hot-reloadable `terminal.show_scrollbar` override.
    pending_show_scrollbar: Arc<Mutex<Option<bool>>>,
    /// Hot-reloadable `terminal.scroll_on_output` override.
    pending_scroll_on_output: Arc<Mutex<Option<bool>>>,
    /// Hot-reloadable `terminal.bell_visual` override.
    pending_bell_visual: Arc<Mutex<Option<bool>>>,
    /// Hot-reloadable `terminal.bell_urgent` override.
    pending_bell_urgent: Arc<Mutex<Option<bool>>>,
    /// Hot-reloadable `[font].family` override. Empty string means
    /// "auto-pick the system's preferred monospace face".
    pending_font_family: Arc<Mutex<Option<String>>>,
    /// Hot-reloadable `[guake]` snapshot. Outer `Option` = "any
    /// pending change", inner = "new state" (None = disable).
    /// Plugin crate doesn't know the renderer's `GuakeRunConfig`
    /// type, so we carry the four primitive fields as a tuple
    /// `(enabled, position, height_pct, width_pct)`; the App-side
    /// EventSink adapter converts to the renderer's struct.
    #[allow(clippy::type_complexity)]
    pending_guake: Arc<Mutex<Option<Option<(bool, String, u8, u8)>>>>,
    /// Per-pane bell mute toggles queued by `rterm.set_pane_bell_muted`.
    /// Each entry is `(tab_0based, pane_0based, muted)`. The App drains
    /// per frame and writes to the matching `Pane::bell_muted` atomic.
    pending_pane_bell_mute: Arc<Mutex<VecDeque<(usize, usize, bool)>>>,
    /// Same as `pending_pane_bell_mute` but addressed by stable uid.
    /// The App walks the tab tree to resolve uid → pane, silently
    /// drops entries for vanished panes. Lets plugins toggle a
    /// specific pane's mute without re-checking its tab/pane
    /// indices on each call.
    pending_pane_bell_mute_by_uid: Arc<Mutex<VecDeque<(u64, bool)>>>,
    /// Hot-reloadable `terminal.slow_command_ms` override published by
    /// the config watcher (or `rterm.set_slow_command_ms`). `0` disables.
    pending_slow_command_ms: Arc<Mutex<Option<u64>>>,
    /// Read by Lua queries; written by the App each frame.
    state: Arc<Mutex<TerminalState>>,
    /// Output-line match rules registered by Lua via `rterm.add_match`.
    /// The App calls `match_output_line` for each completed line and emits
    /// a `match` event for every hit.
    match_rules: Arc<Mutex<Vec<MatchRule>>>,
    /// Watchdog deadline checked by the Lua instruction hook — see
    /// `with_exec_budget`. Keeps a runaway handler (`while true do
    /// end`) from freezing the render thread forever.
    exec_deadline: Arc<ExecDeadline>,
}

/// Parse `#RRGGBB`, `#RGB`, or the same forms without the leading `#`
/// into a `[u8; 3]` triple. Hex digits are case-insensitive. The
/// 3-digit form doubles each digit (CSS convention: `#FA0` → `#FFAA00`).
/// Returns `None` for anything else (wrong length, non-hex byte) so
/// the caller can surface a "did you mean?" message instead of getting
/// a silent garbage value.
fn parse_hex_rgb(hex: &str) -> Option<[u8; 3]> {
    let s = hex.trim().strip_prefix('#').unwrap_or(hex.trim());
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some([r, g, b])
        }
        3 => {
            // CSS short form: `#FA0` → r=0xFF, g=0xAA, b=0x00.
            let digit = |i| u8::from_str_radix(&s[i..i + 1], 16).ok();
            let r = digit(0)?;
            let g = digit(1)?;
            let b = digit(2)?;
            Some([r * 16 + r, g * 16 + g, b * 16 + b])
        }
        _ => None,
    }
}

/// Return true when an sRGB byte triple's relative luminance places it
/// in the dark half (< 0.5). Uses the ITU-R BT.709 channel weights
/// (0.2126 R + 0.7152 G + 0.0722 B) directly on the sRGB bytes — a
/// gamma-aware comparison would be marginally more accurate but the
/// threshold split is robust to ~5 % drift, so the cheap arithmetic
/// suffices for the "should status text be light or dark?" decision
/// this drives in plugins.
fn luminance_is_dark(rgb: [u8; 3]) -> bool {
    let r = rgb[0] as f32;
    let g = rgb[1] as f32;
    let b = rgb[2] as f32;
    let lum = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
    lum < 0.5
}

/// WCAG-style relative luminance (0.0..=1.0) on a gamma-linearised
/// sRGB triple. The `luminance_is_dark` helper uses a coarse linear
/// approximation that's fine for a boolean split; this gamma-aware
/// form is what theme-validators need to compute a real contrast
/// ratio between two colours (`contrast_ratio` below).
fn relative_luminance(rgb: [u8; 3]) -> f32 {
    let chan = |c: u8| {
        let x = c as f32 / 255.0;
        if x <= 0.03928 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * chan(rgb[0]) + 0.7152 * chan(rgb[1]) + 0.0722 * chan(rgb[2])
}

/// WCAG 2.x contrast ratio between two sRGB triples. Range:
/// 1.0 (identical) to 21.0 (black vs white). 4.5 is the AA
/// threshold for body text, 7.0 is AAA. Plugins use this to
/// validate that an overlay colour pairing meets accessibility
/// thresholds without rolling the formula themselves.
fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (light, dark) = if la >= lb { (la, lb) } else { (lb, la) };
    (light + 0.05) / (dark + 0.05)
}

/// Convert a WCAG contrast ratio into the conventional grade
/// label for body text: `"fail"` (<4.5), `"AA"` (≥4.5), `"AAA"`
/// (≥7.0). Large-text thresholds (3.0 / 4.5) are not modelled
/// here since a terminal renders body-size glyphs.
fn contrast_grade(ratio: f32) -> &'static str {
    if ratio >= 7.0 {
        "AAA"
    } else if ratio >= 4.5 {
        "AA"
    } else {
        "fail"
    }
}

/// Compute the 6×6×6 cube / grayscale ramp RGB for an indexed
/// colour slot in 16..=255. Mirrors the renderer's
/// `indexed_color_to_rgb` for the non-named tiers. Kept here so
/// `palette_color` and `nearest_palette_index` share one source
/// of truth; the test in `tests::` pins the byte output against
/// known anchor points.
fn cube_or_grayscale_rgb(i: u8) -> [u8; 3] {
    if i < 232 {
        let v = i - 16;
        let r = v / 36;
        let g = (v / 6) % 6;
        let b = v % 6;
        let map = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
        [map(r), map(g), map(b)]
    } else {
        let gray = 8 + (i - 232) * 10;
        [gray, gray, gray]
    }
}

/// Find the 8-bit palette index whose RGB is closest (Euclidean
/// distance in linear-sRGB byte space) to `target`. Searches all
/// 256 slots: 0..=15 from the live named palette, 16..=255 from
/// the synthetic cube + grayscale ramp. Returns the index; ties
/// pick the lower slot for stability.
fn nearest_palette_index(target: [u8; 3], named: &[[u8; 3]; 16]) -> u8 {
    let dist2 = |c: [u8; 3]| -> u32 {
        let dr = c[0] as i32 - target[0] as i32;
        let dg = c[1] as i32 - target[1] as i32;
        let db = c[2] as i32 - target[2] as i32;
        (dr * dr + dg * dg + db * db) as u32
    };
    let mut best_idx: u8 = 0;
    let mut best_d = u32::MAX;
    for i in 0u16..256 {
        let rgb = if i < 16 {
            named[i as usize]
        } else {
            cube_or_grayscale_rgb(i as u8)
        };
        let d = dist2(rgb);
        if d < best_d {
            best_d = d;
            best_idx = i as u8;
        }
    }
    best_idx
}

/// Human-readable label for an OSC 9;4 progress state byte. Mirrors
/// the iTerm2 / Windows Terminal definition: 0 = clear (no progress
/// shown), 1 = set (definite percent), 2 = error, 3 = indeterminate
/// (spinner), 4 = warning. Unknown bytes fall back to `"unknown"` so a
/// future spec extension doesn't surface as a hard `nil`.
fn progress_state_name(state: u8) -> &'static str {
    match state {
        0 => "clear",
        1 => "set",
        2 => "error",
        3 => "indeterminate",
        4 => "warning",
        _ => "unknown",
    }
}

/// Atomically take the value out of a `Mutex<Option<T>>` slot.
/// Returns `None` for empty slots AND for poisoned mutexes — the
/// renderer's `take_pending_*` callers all treat poison the same way
/// as "no pending value", so a worker thread that panicked during a
/// Lua callback can't take a config-reload setter down with it. Used
/// by the score of `PluginHost::take_pending_*` methods below so the
/// `.lock().ok().and_then(|mut g| g.take())` triple stays in one
/// place and isn't a copy-paste fan-out.
fn take_slot<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    slot.lock().ok().and_then(|mut g| g.take())
}

impl PluginHost {
    /// Construct a fresh Lua plugin host.
    ///
    /// ## Trust model
    ///
    /// `Lua::new()` opens the full Lua 5.4 standard library: `io`,
    /// `os` (including `os.execute` and `os.remove`), `package`
    /// (including `package.loadlib` — arbitrary C extension loading),
    /// `debug`, and friends. This is **intentional** — rterm plugins
    /// are written by the same user who owns the host shell, sit in
    /// the user's config directory (`~/.config/rterm/`), and have the
    /// same trust level as shell rc files.
    ///
    /// A plugin can:
    /// - read/write any file the user can,
    /// - spawn subprocesses,
    /// - send arbitrary bytes to any pane via `rterm.send_input` /
    ///   `rterm.send_to_pane`,
    /// - open URLs through `rterm.open_url` (filtered by the safe-
    ///   scheme whitelist on the renderer side; see
    ///   `rterm_core::is_safe_url`).
    ///
    /// The threat boundary is the **filesystem permission** to write
    /// into `~/.config/rterm/`. An attacker who can drop a file there
    /// already has user-level access by definition.
    ///
    /// If you ever need a restricted Lua surface — e.g. running
    /// third-party plugins — replace `Lua::new()` with
    /// `Lua::new_with(StdLib::SAFE, LuaOptions::default())` and audit
    /// every `rterm.set("foo", lua.create_function(...))` setter for
    /// side-effects on host state.
    pub fn new() -> Result<Self> {
        let lua = Lua::new();
        // Execution watchdog: an instruction-count hook that aborts the
        // running chunk once the armed deadline passes. Disarmed (the
        // common case) it costs one atomic load per 100k VM
        // instructions. This is the only line of defense against a
        // buggy `while true do end` handler — plugins run synchronously
        // on the render thread.
        let exec_deadline = Arc::new(ExecDeadline::new());
        {
            let dl = Arc::clone(&exec_deadline);
            lua.set_hook(
                mlua::HookTriggers::new().every_nth_instruction(100_000),
                move |_lua, _debug| {
                    if dl.expired() {
                        Err(mlua::Error::RuntimeError(
                            "rterm: plugin code exceeded its execution budget \
                             (infinite loop in a handler?) — aborted"
                                .to_string(),
                        ))
                    } else {
                        Ok(mlua::VmState::Continue)
                    }
                },
            );
        }
        let handlers: Arc<Mutex<HashMap<String, Vec<RegistryKey>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Unified plugin-command channel. Lua callbacks send variants
        // here instead of pushing onto per-purpose queues. Cloned
        // `cmd_tx` handles flow into each `lua.create_function` body.
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<rterm_core::PluginCmd>();
        let cmd_rx = Arc::new(Mutex::new(cmd_rx));

        let rterm: Table = lua.create_table()?;

        rterm.set(
            "log",
            lua.create_function(|_, msg: String| {
                tracing::info!(target: "rterm::plugin", "{}", msg);
                Ok(())
            })?,
        )?;

        let handlers_for_on = Arc::clone(&handlers);
        rterm.set(
            "on",
            lua.create_function(move |lua, (event, callback): (String, Function)| {
                let key = lua.create_registry_value(callback)?;
                handlers_for_on
                    .lock()
                    .unwrap()
                    .entry(event)
                    .or_default()
                    .push(key);
                Ok(())
            })?,
        )?;
        let handlers_for_off = Arc::clone(&handlers);
        rterm.set(
            "off",
            lua.create_function(move |_, event: String| {
                let removed = handlers_for_off
                    .lock()
                    .map(|mut g| g.remove(&event).map(|v| v.len()).unwrap_or(0))
                    .unwrap_or(0);
                Ok(removed)
            })?,
        )?;

        let handlers_for_count = Arc::clone(&handlers);
        rterm.set(
            "handler_count",
            lua.create_function(move |_, event: String| {
                // Number of callbacks registered for `event`, or 0 if no
                // handler was ever attached. Helpful for plugins that want
                // to log "subscribed N to bell" or guard a side-effecting
                // setup behind "if rterm.handler_count('output.line') > 0".
                let n = handlers_for_count
                    .lock()
                    .map(|g| g.get(&event).map(|v| v.len()).unwrap_or(0))
                    .unwrap_or(0);
                Ok(n)
            })?,
        )?;

        // `rterm.handler_counts()` — bulk variant returning the full
        // `{ [event_name] = N }` map. Useful for plugin self-diagnosis
        // ("am I the only listener?") or for a meta-plugin that audits
        // event coverage. Skips events with zero handlers to keep the
        // map shape minimal.
        let handlers_for_bulk = Arc::clone(&handlers);
        rterm.set(
            "handler_counts",
            lua.create_function(move |lua, ()| {
                let snapshot: Vec<(String, usize)> = handlers_for_bulk
                    .lock()
                    .map(|g| {
                        g.iter()
                            .filter(|(_, v)| !v.is_empty())
                            .map(|(k, v)| (k.clone(), v.len()))
                            .collect()
                    })
                    .unwrap_or_default();
                let t = lua.create_table()?;
                for (k, n) in snapshot {
                    t.set(k, n)?;
                }
                Ok(t)
            })?,
        )?;

        let actions: Arc<Mutex<HashMap<String, RegistryKey>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let actions_for_register = Arc::clone(&actions);
        rterm.set(
            "register_action",
            lua.create_function(move |lua, (name, callback): (String, Function)| {
                let key = lua.create_registry_value(callback)?;
                actions_for_register.lock().unwrap().insert(name, key);
                Ok(())
            })?,
        )?;
        let actions_for_list = Arc::clone(&actions);
        rterm.set(
            "list_actions",
            lua.create_function(move |lua, ()| {
                let mut names: Vec<String> = actions_for_list
                    .lock()
                    .map(|g| g.keys().cloned().collect())
                    .unwrap_or_default();
                names.sort();
                let arr = lua.create_table()?;
                for (i, n) in names.into_iter().enumerate() {
                    arr.set(i + 1, n)?;
                }
                Ok(arr)
            })?,
        )?;
        let actions_for_unregister = Arc::clone(&actions);
        rterm.set(
            "unregister_action",
            lua.create_function(move |_, name: String| {
                Ok(actions_for_unregister
                    .lock()
                    .map(|mut g| g.remove(&name).is_some())
                    .unwrap_or(false))
            })?,
        )?;

        let send_input_tx = cmd_tx.clone();
        rterm.set(
            "send_input",
            lua.create_function(move |_, payload: String| {
                let _ = send_input_tx
                    .send(rterm_core::PluginCmd::SendInput(payload.into_bytes()));
                Ok(())
            })?,
        )?;

        let pending_tab_titles: TabTitleQueue =
            Arc::new(Mutex::new(VecDeque::new()));
        // `rterm.set_tab_title(name)` — target the active tab. Historical
        // shape, unchanged for plugins that don't care about indices.
        let titles_for_set = Arc::clone(&pending_tab_titles);
        rterm.set(
            "set_tab_title",
            lua.create_function(move |_, name: String| {
                push_capped_drop_oldest(&titles_for_set, (None, name));
                Ok(())
            })?,
        )?;
        // `rterm.set_tab_title_by_index(idx, name)` — target a specific
        // tab (1-based, matching `rterm.tabs()`). Empty `name` clears
        // any custom override on that tab. Indices that don't map to a
        // tab at apply time are silently dropped by the App.
        let titles_for_set_idx = Arc::clone(&pending_tab_titles);
        rterm.set(
            "set_tab_title_by_index",
            lua.create_function(move |_, (idx, name): (u32, String)| {
                let idx0 = idx.saturating_sub(1) as usize;
                push_capped_drop_oldest(&titles_for_set_idx, (Some(idx0), name));
                Ok(())
            })?,
        )?;

        // `rterm.set_window_title(title)` — override the OS window title.
        // Empty string clears the override; the App falls back to its
        // auto-derived title ("rterm — tab N/M · pane N/M · ...").
        let pending_window_title: Arc<Mutex<Option<Option<String>>>> =
            Arc::new(Mutex::new(None));
        let win_title_for_set = Arc::clone(&pending_window_title);
        rterm.set(
            "set_window_title",
            lua.create_function(move |_, name: String| {
                let v = if name.is_empty() { None } else { Some(name) };
                *win_title_for_set.lock().unwrap() = Some(v);
                Ok(())
            })?,
        )?;

        let pending_pane_titles: Arc<Mutex<VecDeque<(usize, usize, String)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let pane_titles_for_set = Arc::clone(&pending_pane_titles);
        rterm.set(
            "set_pane_title",
            lua.create_function(move |_, (tab, pane, title): (u32, u32, String)| {
                let t = tab.saturating_sub(1) as usize;
                let p = pane.saturating_sub(1) as usize;
                push_capped_drop_oldest(&pane_titles_for_set, (t, p, title));
                Ok(())
            })?,
        )?;

        // `rterm.set_pane_title_by_uid(uid, title)` — same as
        // `set_pane_title` but addresses the pane by its stable uid.
        // Survives reorders / pane closes — a uid pointing at a now-
        // gone pane is silently dropped App-side.
        let pending_pane_titles_by_uid: Arc<Mutex<VecDeque<(u64, String)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let pane_titles_by_uid_for_set = Arc::clone(&pending_pane_titles_by_uid);
        rterm.set(
            "set_pane_title_by_uid",
            lua.create_function(move |_, (uid, title): (u64, String)| {
                push_capped_drop_oldest(&pane_titles_by_uid_for_set, (uid, title));
                Ok(())
            })?,
        )?;

        let pending_scrollback_limit: Arc<Mutex<Option<usize>>> =
            Arc::new(Mutex::new(None));
        let scrollback_for_set = Arc::clone(&pending_scrollback_limit);
        rterm.set(
            "set_scrollback",
            lua.create_function(move |_, n: u64| {
                let clamped = n.min(usize::MAX as u64) as usize;
                *scrollback_for_set.lock().unwrap() = Some(clamped);
                Ok(())
            })?,
        )?;

        let pending_cursor_blink: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let pending_show_scrollbar: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let pending_scroll_on_output: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));

        let pending_tab_silence_ms: Arc<Mutex<Option<u64>>> =
            Arc::new(Mutex::new(None));
        let silence_for_set = Arc::clone(&pending_tab_silence_ms);
        rterm.set(
            "set_tab_silence_ms",
            lua.create_function(move |_, n: u64| {
                *silence_for_set.lock().unwrap() = Some(n);
                Ok(())
            })?,
        )?;

        // Lua sugar over the three hot-reloadable `[terminal]` toggles.
        // Plugins use these for e.g. a presentation-mode toggle (steady
        // cursor + scrollbar off) without touching config.toml.
        let cursor_blink_for_set = Arc::clone(&pending_cursor_blink);
        rterm.set(
            "set_cursor_blink",
            lua.create_function(move |_, v: bool| {
                *cursor_blink_for_set.lock().unwrap() = Some(v);
                Ok(())
            })?,
        )?;
        let show_scrollbar_for_set = Arc::clone(&pending_show_scrollbar);
        rterm.set(
            "set_show_scrollbar",
            lua.create_function(move |_, v: bool| {
                *show_scrollbar_for_set.lock().unwrap() = Some(v);
                Ok(())
            })?,
        )?;
        let scroll_on_output_for_set = Arc::clone(&pending_scroll_on_output);
        rterm.set(
            "set_scroll_on_output",
            lua.create_function(move |_, v: bool| {
                *scroll_on_output_for_set.lock().unwrap() = Some(v);
                Ok(())
            })?,
        )?;

        let pending_bell_visual: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let bell_visual_for_set = Arc::clone(&pending_bell_visual);
        rterm.set(
            "set_bell_visual",
            lua.create_function(move |_, v: bool| {
                *bell_visual_for_set.lock().unwrap() = Some(v);
                Ok(())
            })?,
        )?;
        let pending_bell_urgent: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let bell_urgent_for_set = Arc::clone(&pending_bell_urgent);
        rterm.set(
            "set_bell_urgent",
            lua.create_function(move |_, v: bool| {
                *bell_urgent_for_set.lock().unwrap() = Some(v);
                Ok(())
            })?,
        )?;
        let pending_font_family: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let font_family_for_set = Arc::clone(&pending_font_family);
        rterm.set(
            "set_font_family",
            lua.create_function(move |_, name: String| {
                *font_family_for_set.lock().unwrap() = Some(name);
                Ok(())
            })?,
        )?;

        // Guake snapshot override. Stored as primitive tuple so the
        // plugin crate doesn't need to know the renderer's type. App-
        // side EventSink converts to the GuakeRunConfig struct.
        #[allow(clippy::type_complexity)]
        let pending_guake: Arc<Mutex<Option<Option<(bool, String, u8, u8)>>>> =
            Arc::new(Mutex::new(None));

        // Slow-command threshold. Plugins can adjust at runtime to
        // raise/lower the bar without touching the TOML config.
        let pending_slow_command_ms: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let slow_for_set = Arc::clone(&pending_slow_command_ms);
        rterm.set(
            "set_slow_command_ms",
            lua.create_function(move |_, n: u64| {
                *slow_for_set.lock().unwrap() = Some(n);
                Ok(())
            })?,
        )?;

        // Per-pane bell mute. Lua uses 1-based indices (matching
        // `list_panes`); convert to 0-based here so the App drain doesn't
        // have to know about the wire format.
        let pending_pane_bell_mute: Arc<Mutex<VecDeque<(usize, usize, bool)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let mute_for_set = Arc::clone(&pending_pane_bell_mute);
        rterm.set(
            "set_pane_bell_muted",
            lua.create_function(move |_, (tab, pane, muted): (u32, u32, bool)| {
                let tab_idx = (tab.saturating_sub(1)) as usize;
                let pane_idx = (pane.saturating_sub(1)) as usize;
                push_capped_drop_oldest(&mute_for_set, (tab_idx, pane_idx, muted));
                Ok(())
            })?,
        )?;
        // `rterm.set_pane_bell_muted_by_uid(uid, muted)` — stable-id
        // sibling. Lets a plugin that captured a pane uid once
        // toggle its mute without re-resolving (tab, pane) on
        // every call.
        let pending_pane_bell_mute_by_uid: Arc<Mutex<VecDeque<(u64, bool)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let mute_by_uid_for_set = Arc::clone(&pending_pane_bell_mute_by_uid);
        rterm.set(
            "set_pane_bell_muted_by_uid",
            lua.create_function(move |_, (uid, muted): (u64, bool)| {
                push_capped_drop_oldest(&mute_by_uid_for_set, (uid, muted));
                Ok(())
            })?,
        )?;

        // `rterm.add_match(name, pattern, opts?)`. `opts.regex = true` makes
        // `pattern` a regex (compiled here so bad patterns reject at register
        // time, not on each line). Returns true if the rule was installed.
        // Re-registering a name replaces the prior pattern atomically.
        // Capped at 64 active rules to bound work per drained line.
        const MATCH_RULES_CAP: usize = 64;
        let match_rules: Arc<Mutex<Vec<MatchRule>>> = Arc::new(Mutex::new(Vec::new()));
        let rules_for_add = Arc::clone(&match_rules);
        rterm.set(
            "add_match",
            lua.create_function(move |_, (name, pattern, opts): (String, String, Option<Table>)| {
                if name.is_empty() {
                    return Ok(false);
                }
                let is_regex = opts
                    .and_then(|t| t.get::<Option<bool>>("regex").ok().flatten())
                    .unwrap_or(false);
                let kind = if is_regex {
                    match Regex::new(&pattern) {
                        Ok(re) => MatchKind::Regex(re),
                        Err(e) => {
                            tracing::warn!(
                                target: "rterm::plugin",
                                "add_match {name:?}: invalid regex {pattern:?}: {e}",
                            );
                            return Ok(false);
                        }
                    }
                } else {
                    MatchKind::Substring(pattern)
                };
                let Ok(mut rules) = rules_for_add.lock() else {
                    return Ok(false);
                };
                rules.retain(|r| r.name != name);
                if rules.len() >= MATCH_RULES_CAP {
                    tracing::warn!(
                        target: "rterm::plugin",
                        "add_match {name:?}: cap of {MATCH_RULES_CAP} rules reached; dropping",
                    );
                    return Ok(false);
                }
                rules.push(MatchRule { name, kind });
                Ok(true)
            })?,
        )?;
        let rules_for_remove = Arc::clone(&match_rules);
        rterm.set(
            "remove_match",
            lua.create_function(move |_, name: String| {
                let Ok(mut rules) = rules_for_remove.lock() else {
                    return Ok(false);
                };
                let before = rules.len();
                rules.retain(|r| r.name != name);
                Ok(rules.len() < before)
            })?,
        )?;
        // `rterm.remove_all_matches()` — bulk-clear sibling of
        // `remove_match`. Returns the number of rules that were
        // dropped, so plugins can confirm whether anything was
        // registered before the reset.
        let rules_for_clear = Arc::clone(&match_rules);
        rterm.set(
            "remove_all_matches",
            lua.create_function(move |_, ()| {
                let Ok(mut rules) = rules_for_clear.lock() else {
                    return Ok(0u32);
                };
                let n = rules.len() as u32;
                rules.clear();
                Ok(n)
            })?,
        )?;
        let rules_for_list = Arc::clone(&match_rules);
        rterm.set(
            "list_matches",
            lua.create_function(move |lua, ()| {
                let names: Vec<String> = rules_for_list
                    .lock()
                    .map(|g| g.iter().map(|r| r.name.clone()).collect())
                    .unwrap_or_default();
                let arr = lua.create_table()?;
                for (i, n) in names.into_iter().enumerate() {
                    arr.set(i + 1, n)?;
                }
                Ok(arr)
            })?,
        )?;

        // `rterm.match_rules()` — rich variant of `list_matches`.
        // Returns an array of `{name, kind, pattern}` tables where
        // `kind` is `"substring"` or `"regex"`. The pattern is the
        // exact source string for substring rules; for regex rules
        // it's the compiled regex's debug form (close to the source
        // — useful for introspection but plugins shouldn't rely on
        // an exact byte-for-byte match).
        let rules_for_rich = Arc::clone(&match_rules);
        rterm.set(
            "match_rules",
            lua.create_function(move |lua, ()| {
                let arr = lua.create_table()?;
                let rules = rules_for_rich.lock().ok();
                if let Some(rules) = rules {
                    for (i, r) in rules.iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("name", r.name.clone())?;
                        match &r.kind {
                            MatchKind::Substring(s) => {
                                entry.set("kind", "substring")?;
                                entry.set("pattern", s.clone())?;
                            }
                            MatchKind::Regex(re) => {
                                entry.set("kind", "regex")?;
                                entry.set("pattern", re.as_str().to_string())?;
                            }
                        }
                        arr.set(i + 1, entry)?;
                    }
                }
                Ok(arr)
            })?,
        )?;

        // `rterm.find_match(name)` — single-rule lookup. Returns the
        // same `{name, kind, pattern}` shape as one element of
        // `match_rules()`, or `nil` if no rule by that name exists.
        // Cheaper than iterating `match_rules()` when a plugin only
        // cares whether one specific rule is registered.
        let rules_for_find = Arc::clone(&match_rules);
        rterm.set(
            "find_match",
            lua.create_function(move |lua, name: String| {
                let Ok(rules) = rules_for_find.lock() else {
                    return Ok(mlua::Value::Nil);
                };
                let Some(r) = rules.iter().find(|r| r.name == name) else {
                    return Ok(mlua::Value::Nil);
                };
                let entry = lua.create_table()?;
                entry.set("name", r.name.clone())?;
                match &r.kind {
                    MatchKind::Substring(s) => {
                        entry.set("kind", "substring")?;
                        entry.set("pattern", s.clone())?;
                    }
                    MatchKind::Regex(re) => {
                        entry.set("kind", "regex")?;
                        entry.set("pattern", re.as_str().to_string())?;
                    }
                }
                Ok(mlua::Value::Table(entry))
            })?,
        )?;

        let pending_routed_input: RoutedInputQueue = Arc::new(Mutex::new(VecDeque::new()));
        let routed_for_send = Arc::clone(&pending_routed_input);
        rterm.set(
            "send_to_pane",
            lua.create_function(
                move |_, (tab, pane, payload): (u32, u32, String)| {
                    // Lua uses 1-based indices to match `list_panes`; clamp
                    // back to 0-based for the App. Saturating-sub keeps the
                    // call from underflowing if the caller passes 0.
                    let tab_idx = (tab.saturating_sub(1)) as usize;
                    let pane_idx = (pane.saturating_sub(1)) as usize;
                    if !push_capped_reject(
                        &routed_for_send,
                        ((tab_idx, pane_idx), payload.into_bytes()),
                    ) {
                        tracing::warn!(
                            target: "rterm::plugin",
                            "send_to_pane queue full ({PLUGIN_QUEUE_CAP}) — dropping payload"
                        );
                    }
                    Ok(())
                },
            )?,
        )?;

        // `rterm.send_to_pane_by_uid(uid, payload)` — sibling of
        // `send_to_pane(tab, pane, payload)` that addresses by stable
        // uid. Useful for "type a build command into the cargo-build
        // pane" workflows where the pane index might shift between
        // when the plugin remembers it and when it sends.
        let pending_routed_input_by_uid: RoutedInputByUidQueue =
            Arc::new(Mutex::new(VecDeque::new()));
        let routed_by_uid_for_send = Arc::clone(&pending_routed_input_by_uid);
        rterm.set(
            "send_to_pane_by_uid",
            lua.create_function(move |_, (uid, payload): (u64, String)| {
                if !push_capped_reject(&routed_by_uid_for_send, (uid, payload.into_bytes())) {
                    tracing::warn!(
                        target: "rterm::plugin",
                        "send_to_pane_by_uid queue full ({PLUGIN_QUEUE_CAP}) — dropping payload"
                    );
                }
                Ok(())
            })?,
        )?;

        let pending_attention: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let attention_for_set = Arc::clone(&pending_attention);
        rterm.set(
            "attention",
            lua.create_function(move |_, ()| {
                *attention_for_set.lock().unwrap() = true;
                Ok(())
            })?,
        )?;

        // `rterm.notify(message)` — fires a `notification` plugin event
        // and requests taskbar attention if the window is unfocused.
        // First queue migrated to the unified `cmd_tx` channel (the
        // legacy `Arc<Mutex<VecDeque<String>>>` field is gone).
        let notify_tx = cmd_tx.clone();
        rterm.set(
            "notify",
            lua.create_function(move |_, msg: String| {
                let _ = notify_tx.send(rterm_core::PluginCmd::Notify(msg));
                Ok(())
            })?,
        )?;

        let run_action_tx = cmd_tx.clone();
        rterm.set(
            "run_action",
            lua.create_function(move |_, name: String| {
                let _ = run_action_tx.send(rterm_core::PluginCmd::RunAction(name));
                Ok(())
            })?,
        )?;

        let pending_focus: Arc<Mutex<Option<(usize, usize)>>> =
            Arc::new(Mutex::new(None));
        let focus_for_set = Arc::clone(&pending_focus);
        rterm.set(
            "focus_pane",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let t = tab.saturating_sub(1) as usize;
                let p = pane.saturating_sub(1) as usize;
                *focus_for_set.lock().unwrap() = Some((t, p));
                Ok(())
            })?,
        )?;

        // `rterm.focus_pane_by_uid(uid)` — sibling of `focus_pane(tab,
        // pane)` that addresses the pane by its stable uid. App walks
        // live panes to resolve. A uid that no longer points at a live
        // pane is silently dropped (same convention as
        // `set_pane_title_by_uid`).
        let pending_focus_by_uid: Arc<Mutex<Option<u64>>> =
            Arc::new(Mutex::new(None));
        let focus_by_uid_for_set = Arc::clone(&pending_focus_by_uid);
        rterm.set(
            "focus_pane_by_uid",
            lua.create_function(move |_, uid: u64| {
                *focus_by_uid_for_set.lock().unwrap() = Some(uid);
                Ok(())
            })?,
        )?;

        // `rterm.focus_tab(idx)` — switch to the 1-based tab index without
        // touching which pane is focused inside it. Sibling of `focus_pane`
        // for the common "just go to tab N" case.
        let pending_tab_focus: Arc<Mutex<Option<usize>>> =
            Arc::new(Mutex::new(None));
        let tab_focus_for_set = Arc::clone(&pending_tab_focus);
        rterm.set(
            "focus_tab",
            lua.create_function(move |_, idx: u32| {
                let i = idx.saturating_sub(1) as usize;
                *tab_focus_for_set.lock().unwrap() = Some(i);
                Ok(())
            })?,
        )?;

        let pending_copy: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let copy_for_set = Arc::clone(&pending_copy);
        rterm.set(
            "copy",
            lua.create_function(move |_, text: String| {
                *copy_for_set.lock().unwrap() = Some(text);
                Ok(())
            })?,
        )?;

        let clipboard_reader: Arc<Mutex<Option<ClipboardReader>>> =
            Arc::new(Mutex::new(None));
        let reader_for_read = Arc::clone(&clipboard_reader);
        rterm.set(
            "read_clipboard",
            lua.create_function(move |_, ()| {
                let text = reader_for_read
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|f| f()))
                    .flatten();
                Ok(text)
            })?,
        )?;

        let config_dir: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let cfg_dir_for_read = Arc::clone(&config_dir);
        rterm.set(
            "config_dir",
            lua.create_function(move |_, ()| {
                let s = cfg_dir_for_read.lock().map(|g| g.clone()).unwrap_or_default();
                if s.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(s))
                }
            })?,
        )?;

        // `rterm.pid()` — OS pid of the rterm process itself (not the
        // shell). Useful for IPC, log correlation, and "find my window"
        // style integrations.
        rterm.set(
            "pid",
            lua.create_function(|_, ()| Ok(std::process::id()))?,
        )?;

        // `rterm.executable_path()` — absolute path of the rterm binary
        // currently running. Useful for plugins that want to re-spawn
        // rterm with different flags, generate desktop entries pointing
        // at the right binary, or surface the version in a status bar.
        // Returns nil on platforms where `std::env::current_exe` fails
        // (unusual; usually only happens when the binary has been
        // deleted while running).
        rterm.set(
            "executable_path",
            lua.create_function(|_, ()| {
                Ok(std::env::current_exe()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned()))
            })?,
        )?;

        // `rterm.executable_args()` — array of process argv strings
        // (1-based, excluding argv[0] which is the binary path).
        // Plugins surfacing "rterm was started with `--config X`"
        // in a status bar or bug-report dialog read this. The
        // values are captured at PluginHost::new() — args don't
        // change at runtime so a stale snapshot here would be a
        // process-level confusion, not a plugin one.
        rterm.set(
            "executable_args",
            lua.create_function(|lua, ()| {
                let arr = lua.create_table()?;
                for (i, arg) in std::env::args().skip(1).enumerate() {
                    arr.set(i + 1, arg)?;
                }
                Ok(arr)
            })?,
        )?;

        let shell_program: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let shell_for_read = Arc::clone(&shell_program);
        rterm.set(
            "shell",
            lua.create_function(move |_, ()| {
                let s = shell_for_read.lock().map(|g| g.clone()).unwrap_or_default();
                if s.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(s))
                }
            })?,
        )?;

        let cache_dir: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let cache_dir_for_read = Arc::clone(&cache_dir);
        rterm.set(
            "cache_dir",
            lua.create_function(move |_, ()| {
                let s = cache_dir_for_read.lock().map(|g| g.clone()).unwrap_or_default();
                if s.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(s))
                }
            })?,
        )?;

        // `rterm.config_path()` — full path to `config.toml`. Returns `nil`
        // when no config dir has been resolved (e.g. exotic CI shells).
        let cfg_dir_for_path = Arc::clone(&config_dir);
        rterm.set(
            "config_path",
            lua.create_function(move |_, ()| {
                let dir = cfg_dir_for_path.lock().map(|g| g.clone()).unwrap_or_default();
                if dir.is_empty() {
                    Ok(None)
                } else {
                    let mut p = std::path::PathBuf::from(dir);
                    p.push("config.toml");
                    Ok(Some(p.display().to_string()))
                }
            })?,
        )?;

        let builtin_actions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let actions_for_list = Arc::clone(&builtin_actions);
        rterm.set(
            "builtin_actions",
            lua.create_function(move |lua, ()| {
                let names = actions_for_list
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let arr = lua.create_table()?;
                for (i, n) in names.iter().enumerate() {
                    arr.set(i + 1, n.clone())?;
                }
                Ok(arr)
            })?,
        )?;

        // `rterm.builtin_action_label(name)` — human-readable label for
        // a canonical built-in action name (e.g. `"new_tab"` →
        // `"New tab"`). Returns nil for plugin-registered actions or
        // unknown names. Plugins building custom action pickers /
        // palettes pair this with `builtin_actions()` to render the
        // same "name + description" two-column layout the in-app
        // command palette does — without duplicating the label table.
        let builtin_action_labels: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let labels_for_get = Arc::clone(&builtin_action_labels);
        rterm.set(
            "builtin_action_label",
            lua.create_function(move |_, name: String| {
                Ok(labels_for_get
                    .lock()
                    .ok()
                    .and_then(|g| g.get(&name).cloned()))
            })?,
        )?;

        // `rterm.builtin_action_labels()` — bulk variant returning the
        // full `{ [name] = label }` table in one Lua→Rust hop. Plugins
        // populating an action palette would otherwise need N calls to
        // `builtin_action_label` (one per name); the bulk form keeps
        // the FFI cost constant regardless of the action count.
        let labels_for_bulk = Arc::clone(&builtin_action_labels);
        rterm.set(
            "builtin_action_labels",
            lua.create_function(move |lua, ()| {
                let snapshot: Vec<(String, String)> = labels_for_bulk
                    .lock()
                    .map(|g| g.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                let t = lua.create_table()?;
                for (k, v) in snapshot {
                    t.set(k, v)?;
                }
                Ok(t)
            })?,
        )?;

        let builtin_events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_list = Arc::clone(&builtin_events);
        rterm.set(
            "builtin_events",
            lua.create_function(move |lua, ()| {
                let names = events_for_list
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let arr = lua.create_table()?;
                for (i, n) in names.iter().enumerate() {
                    arr.set(i + 1, n.clone())?;
                }
                Ok(arr)
            })?,
        )?;

        // Canonical enum strings for the `pane.cursor_shape` and
        // `pane.mouse_mode` events. Plugins use these to:
        //   - validate input before calling `set_*` mode toggles,
        //   - render dropdown / tab-complete UIs for the same set,
        //   - confirm a freshly-parsed event payload matches a
        //     known value rather than typo-fall-through to "off"
        //     in their own code.
        // Each entry's index mirrors the internal u8 code so a
        // plugin that wants the numeric form can subtract 1.
        rterm.set(
            "cursor_shape_names",
            lua.create_function(|lua, ()| {
                let arr = lua.create_table()?;
                for (i, n) in ["block", "underline", "bar"].iter().enumerate() {
                    arr.set(i + 1, *n)?;
                }
                Ok(arr)
            })?,
        )?;
        rterm.set(
            "mouse_mode_names",
            lua.create_function(|lua, ()| {
                let arr = lua.create_table()?;
                for (i, n) in ["off", "x10", "btn", "any"].iter().enumerate() {
                    arr.set(i + 1, *n)?;
                }
                Ok(arr)
            })?,
        )?;


        let emit_event_tx = cmd_tx.clone();
        rterm.set(
            "emit_event",
            lua.create_function(move |_, (name, body): (String, Option<String>)| {
                let _ = emit_event_tx
                    .send(rterm_core::PluginCmd::EmitEvent(name, body.unwrap_or_default()));
                Ok(())
            })?,
        )?;

        // `rterm.version()` returns the host crate version, useful for
        // plugins that need to gate behaviour on rterm features added in a
        // specific release.
        rterm.set(
            "version",
            lua.create_function(|_, ()| {
                Ok(env!("CARGO_PKG_VERSION").to_string())
            })?,
        )?;

        // `rterm.version_info()` — richer form returning a table
        // `{version, target_os, target_arch, profile}` mirroring
        // the `--version --json` CLI output. Plugins building a
        // bug-report dialog include this verbatim so the user
        // doesn't have to copy multiple fields manually.
        rterm.set(
            "version_info",
            lua.create_function(|lua, ()| {
                let t = lua.create_table()?;
                t.set("version", env!("CARGO_PKG_VERSION"))?;
                t.set("target_os", std::env::consts::OS)?;
                t.set("target_arch", std::env::consts::ARCH)?;
                t.set(
                    "profile",
                    if cfg!(debug_assertions) { "debug" } else { "release" },
                )?;
                Ok(t)
            })?,
        )?;


        // `rterm.platform()` returns a small table describing the host so
        // plugins can branch on it instead of reaching into `os.execute`.
        // `wsl` is true when running under WSL1/2 (kernel release string
        // contains "microsoft") — useful for plugins that want to avoid
        // notify-send / clipboard quirks on WSL.
        rterm.set(
            "platform",
            lua.create_function(|lua, ()| {
                let t = lua.create_table()?;
                t.set("os", std::env::consts::OS)?;
                t.set("family", std::env::consts::FAMILY)?;
                t.set("arch", std::env::consts::ARCH)?;
                // `target_os` / `target_arch` aliases mirror the keys
                // used by `rterm.version_info()` and `--version --json`.
                // Lets plugins reach for whichever name fits the
                // surrounding code (e.g. a switch on Cargo cfg keys).
                t.set("target_os", std::env::consts::OS)?;
                t.set("target_arch", std::env::consts::ARCH)?;
                let wsl = if cfg!(target_os = "linux") {
                    std::fs::read_to_string("/proc/sys/kernel/osrelease")
                        .map(|s| s.to_ascii_lowercase().contains("microsoft"))
                        .unwrap_or(false)
                } else {
                    false
                };
                t.set("wsl", wsl)?;
                Ok(t)
            })?,
        )?;

        let scroll_tx = cmd_tx.clone();
        rterm.set(
            "scroll",
            lua.create_function(move |_, delta: i32| {
                let _ = scroll_tx.send(rterm_core::PluginCmd::Scroll(delta));
                Ok(())
            })?,
        )?;
        let scroll_to_live_tx = cmd_tx.clone();
        rterm.set(
            "scroll_to_live",
            lua.create_function(move |_, ()| {
                // A large negative delta is clamped to 0 by the App.
                let _ = scroll_to_live_tx.send(rterm_core::PluginCmd::Scroll(i32::MIN));
                Ok(())
            })?,
        )?;

        // `rterm.scroll_to_line(n)` — anchor logical line `n` at the top
        // of the focused pane's viewport. Pairs with `rterm.prompt_marks()`
        // for "jump to my Nth prompt".
        let pending_scroll_to_line: Arc<Mutex<Option<usize>>> =
            Arc::new(Mutex::new(None));
        let scroll_line_for_set = Arc::clone(&pending_scroll_to_line);
        rterm.set(
            "scroll_to_line",
            lua.create_function(move |_, line: u64| {
                *scroll_line_for_set.lock().unwrap() = Some(line as usize);
                Ok(())
            })?,
        )?;

        // `rterm.start_search(query?, regex_mode?)` — open the search
        // overlay, optionally pre-filling the query (and switching to
        // regex mode). Empty query → just opens the empty overlay.
        let pending_start_search: Arc<Mutex<Option<(String, bool)>>> =
            Arc::new(Mutex::new(None));
        let start_search_for_set = Arc::clone(&pending_start_search);
        rterm.set(
            "start_search",
            lua.create_function(
                move |_, (query, regex): (Option<String>, Option<bool>)| {
                    *start_search_for_set.lock().unwrap() =
                        Some((query.unwrap_or_default(), regex.unwrap_or(false)));
                    Ok(())
                },
            )?,
        )?;

        let new_tab_tx = cmd_tx.clone();
        rterm.set(
            "new_tab",
            lua.create_function(move |_, cwd: Option<String>| {
                let _ = new_tab_tx.send(rterm_core::PluginCmd::NewTab(cwd));
                Ok(())
            })?,
        )?;

        let split_tx = cmd_tx.clone();
        rterm.set(
            "split",
            lua.create_function(move |_, (dir, cwd): (String, Option<String>)| {
                let _ = split_tx.send(rterm_core::PluginCmd::Split(dir, cwd));
                Ok(())
            })?,
        )?;

        let pending_font_size: Arc<Mutex<Option<f32>>> = Arc::new(Mutex::new(None));
        let font_for_set = Arc::clone(&pending_font_size);
        rterm.set(
            "set_font_size",
            lua.create_function(move |_, size: f32| {
                // Drop NaN / Infinity at the boundary so the renderer
                // never sees a value `f32::clamp` would panic on, and
                // plugins get a no-op rather than a silent stall when
                // they compute a bogus size (e.g. `n / 0`).
                if size.is_finite() {
                    *font_for_set.lock().unwrap() = Some(size);
                }
                Ok(())
            })?,
        )?;

        // `rterm.set_opacity(value)` — adjust window/background alpha at
        // runtime. Clamped to `0.0..=1.0`; NaN/Inf is dropped so the
        // renderer never sees a value `f32::clamp` would panic on.
        let pending_opacity: Arc<Mutex<Option<f32>>> = Arc::new(Mutex::new(None));
        let opacity_for_set = Arc::clone(&pending_opacity);
        rterm.set(
            "set_opacity",
            lua.create_function(move |_, value: f32| {
                if value.is_finite() {
                    *opacity_for_set.lock().unwrap() = Some(value.clamp(0.0, 1.0));
                }
                Ok(())
            })?,
        )?;

        let paste_tx = cmd_tx.clone();
        rterm.set(
            "paste",
            lua.create_function(move |_, text: String| {
                let _ = paste_tx.send(rterm_core::PluginCmd::Paste(text.into_bytes()));
                Ok(())
            })?,
        )?;

        let open_url_tx = cmd_tx.clone();
        rterm.set(
            "open_url",
            lua.create_function(move |_, url: String| {
                let _ = open_url_tx.send(rterm_core::PluginCmd::OpenUrl(url));
                Ok(())
            })?,
        )?;

        let kill_pane_tx = cmd_tx.clone();
        rterm.set(
            "kill_pane",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let t = tab.saturating_sub(1) as usize;
                let p = pane.saturating_sub(1) as usize;
                let _ = kill_pane_tx.send(rterm_core::PluginCmd::KillPane(t, p));
                Ok(())
            })?,
        )?;

        // `rterm.kill_pane_by_uid(uid)` — sibling of `kill_pane(tab, pane)`
        // that addresses by stable uid. Same dispose-not-found behaviour
        // (uid no longer pointing at a live pane is silently dropped).
        let kill_pane_by_uid_tx = cmd_tx.clone();
        rterm.set(
            "kill_pane_by_uid",
            lua.create_function(move |_, uid: u64| {
                let _ = kill_pane_by_uid_tx.send(rterm_core::PluginCmd::KillPaneByUid(uid));
                Ok(())
            })?,
        )?;

        let kill_tab_tx = cmd_tx.clone();
        rterm.set(
            "kill_tab",
            lua.create_function(move |_, idx: u32| {
                let i = idx.saturating_sub(1) as usize;
                let _ = kill_tab_tx.send(rterm_core::PluginCmd::KillTab(i));
                Ok(())
            })?,
        )?;

        let pending_bell: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let bell_for_call = Arc::clone(&pending_bell);
        rterm.set(
            "bell",
            lua.create_function(move |_, ()| {
                *bell_for_call.lock().unwrap() = true;
                Ok(())
            })?,
        )?;

        // Built-in theme names — kept in lockstep with
        // `rterm_render::palette::builtin_themes()` via a test in
        // rterm-app. Lua side accepts any string; unknown names are
        // silently ignored by the App (no panic, no error).
        let theme_names: &[&str] = &[
            "default",
            "dark",
            "dracula",
            "solarized-dark",
            "solarized-light",
            "nord",
            "gruvbox-dark",
            "light",
        ];
        let pending_theme: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let theme_for_set = Arc::clone(&pending_theme);
        rterm.set(
            "set_theme",
            lua.create_function(move |_, name: String| {
                let lower = name.trim().to_ascii_lowercase();
                let known = theme_names.iter().any(|t| t.eq_ignore_ascii_case(&lower));
                if !known {
                    return Ok(false);
                }
                *theme_for_set.lock().unwrap() = Some(lower);
                Ok(true)
            })?,
        )?;
        rterm.set(
            "themes",
            lua.create_function(move |lua, ()| {
                let t = lua.create_table()?;
                for (i, name) in theme_names.iter().enumerate() {
                    t.set(i + 1, *name)?;
                }
                Ok(t)
            })?,
        )?;

        let pending_palette: Arc<Mutex<Option<PluginPalette>>> =
            Arc::new(Mutex::new(None));
        let palette_for_set = Arc::clone(&pending_palette);
        rterm.set(
            "set_palette",
            lua.create_function(move |_, t: mlua::Table| {
                fn rgb_from(v: Option<mlua::Value>) -> Option<[u8; 3]> {
                    let t = v?.as_table()?.clone();
                    let r = t.get::<u8>(1).ok()?;
                    let g = t.get::<u8>(2).ok()?;
                    let b = t.get::<u8>(3).ok()?;
                    Some([r, g, b])
                }
                let named = match t.get::<mlua::Table>("named") {
                    Ok(named) => {
                        let mut arr = [[0u8; 3]; 16];
                        let mut ok = true;
                        for (i, slot) in arr.iter_mut().enumerate() {
                            match rgb_from(named.get::<mlua::Value>(i + 1).ok()) {
                                Some(rgb) => *slot = rgb,
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok {
                            Some(arr)
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                };
                let p = PluginPalette {
                    default_fg: rgb_from(t.get::<mlua::Value>("default_fg").ok()),
                    default_bg: rgb_from(t.get::<mlua::Value>("default_bg").ok()),
                    cursor: rgb_from(t.get::<mlua::Value>("cursor").ok()),
                    named,
                };
                *palette_for_set.lock().unwrap() = Some(p);
                Ok(())
            })?,
        )?;

        let state: Arc<Mutex<TerminalState>> = Arc::new(Mutex::new(TerminalState::default()));
        let state_for_cwd = Arc::clone(&state);
        rterm.set(
            "cwd",
            lua.create_function(move |_, ()| {
                Ok(state_for_cwd.lock().ok().and_then(|s| s.cwd.clone()))
            })?,
        )?;
        // `rterm.current_theme()` — canonical name of the active
        // built-in theme. Empty string if `set_palette` overrode the
        // palette directly without matching a built-in.
        let state_for_theme = Arc::clone(&state);
        rterm.set(
            "current_theme",
            lua.create_function(move |_, ()| {
                Ok(state_for_theme
                    .lock()
                    .ok()
                    .map(|s| s.active_theme.clone())
                    .unwrap_or_default())
            })?,
        )?;
        // `rterm.cwd_of(tab, pane)` — per-pane cwd by 1-based
        // index. Returns nil for out-of-range pairs, or when the
        // pane's shell hasn't advertised one via OSC 7 / OSC 1337
        // / OSC 633;P;Cwd= yet.
        let state_for_cwd_of = Arc::clone(&state);
        rterm.set(
            "cwd_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_cwd_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .and_then(|p| p.cwd.clone()))
            })?,
        )?;
        // `rterm.cwd_by_uid(uid)` — stable-id sibling.
        let state_for_cwd_uid = Arc::clone(&state);
        rterm.set(
            "cwd_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_cwd_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .and_then(|p| p.cwd.clone()))
            })?,
        )?;
        let state_for_title = Arc::clone(&state);
        rterm.set(
            "title",
            lua.create_function(move |_, ()| {
                Ok(state_for_title.lock().ok().and_then(|s| s.title.clone()))
            })?,
        )?;
        // `rterm.title_of(tab, pane)` — per-pane title (NOT the
        // window title — `rterm.title()` returns that). Returns
        // nil for out-of-range pairs. The pane title is always a
        // string for live panes (empty if no title has been set);
        // callers wanting an Option-flavoured "is there a title?"
        // check `#title_of(...) > 0`.
        let state_for_pane_title_of = Arc::clone(&state);
        rterm.set(
            "title_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_pane_title_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .map(|p| p.title.clone()))
            })?,
        )?;
        // `rterm.title_by_uid(uid)` — stable-id sibling.
        let state_for_pane_title_uid = Arc::clone(&state);
        rterm.set(
            "title_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_pane_title_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .map(|p| p.title.clone()))
            })?,
        )?;
        // `rterm.foreground_process_of(tab, pane)` — per-pane
        // foreground-process name (Linux-only — read from
        // `/proc/<pgid>/comm` each frame). Returns nil for
        // out-of-range pairs OR when the backend doesn't report
        // a foreground pgid (Windows / fallback). Plugins use this
        // to show "what's running in this pane right now" badges.
        let state_for_fp_of = Arc::clone(&state);
        rterm.set(
            "foreground_process_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_fp_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .and_then(|p| p.foreground_process.clone()))
            })?,
        )?;
        let state_for_fp_uid = Arc::clone(&state);
        rterm.set(
            "foreground_process_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_fp_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .and_then(|p| p.foreground_process.clone()))
            })?,
        )?;
        // `rterm.foreground_pgid_of(tab, pane)` — PID of the
        // pane's foreground process group (Linux-only — derived
        // from `tcgetpgrp`). Returns nil for out-of-range or
        // when the backend doesn't report one. Plugins doing
        // ptrace-style debugging or signalling use this to
        // target the specific group rather than the shell PID.
        let state_for_fpg_of = Arc::clone(&state);
        rterm.set(
            "foreground_pgid_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_fpg_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .and_then(|p| p.foreground_pgid))
            })?,
        )?;
        let state_for_fpg_uid = Arc::clone(&state);
        rterm.set(
            "foreground_pgid_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_fpg_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .and_then(|p| p.foreground_pgid))
            })?,
        )?;

        // `rterm.prompt_marks()` / `rterm.command_marks()` — focused pane's
        // OSC 133;A / ;C mark logical lines (scrollback first then grid).
        // Plugins use these to e.g. "scroll to my Nth-last prompt".
        let state_for_pm = Arc::clone(&state);
        rterm.set(
            "prompt_marks",
            lua.create_function(move |lua, ()| {
                let lines = state_for_pm
                    .lock()
                    .ok()
                    .map(|g| g.prompt_mark_lines.clone())
                    .unwrap_or_default();
                let arr = lua.create_table()?;
                for (i, line) in lines.into_iter().enumerate() {
                    arr.set(i + 1, line as u64)?;
                }
                Ok(arr)
            })?,
        )?;
        let state_for_cm = Arc::clone(&state);
        rterm.set(
            "command_marks",
            lua.create_function(move |lua, ()| {
                let lines = state_for_cm
                    .lock()
                    .ok()
                    .map(|g| g.command_mark_lines.clone())
                    .unwrap_or_default();
                let arr = lua.create_table()?;
                for (i, line) in lines.into_iter().enumerate() {
                    arr.set(i + 1, line as u64)?;
                }
                Ok(arr)
            })?,
        )?;
        let state_for_size = Arc::clone(&state);
        rterm.set(
            "size",
            lua.create_function(move |lua, ()| {
                let s = state_for_size.lock().ok();
                let (cols, rows) = match s {
                    Some(g) => (g.cols, g.rows),
                    None => (0, 0),
                };
                let t = lua.create_table()?;
                t.set("cols", cols)?;
                t.set("rows", rows)?;
                Ok(t)
            })?,
        )?;
        // `rterm.size_of(tab, pane)` — per-pane variant of `size()`.
        // 1-based indices match `list_panes`. Returns `{cols, rows}`
        // or nil when the pair is out of range. Sibling of
        // `cursor_of` for plugins that need to lay out output to a
        // specific pane's geometry (e.g. choose a `cargo --color`
        // mode based on a build-pane's width).
        let state_for_size_of = Arc::clone(&state);
        rterm.set(
            "size_of",
            lua.create_function(move |lua, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_size_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                let Some(p) = state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                else {
                    return Ok(None);
                };
                let t = lua.create_table()?;
                t.set("cols", p.cols)?;
                t.set("rows", p.rows)?;
                Ok(Some(t))
            })?,
        )?;
        // `rterm.size_by_uid(uid)` — stable-id sibling of `size_of`.
        // Survives reorders / splits. Returns `{cols, rows}` or nil.
        let state_for_size_by_uid = Arc::clone(&state);
        rterm.set(
            "size_by_uid",
            lua.create_function(move |lua, uid: u64| {
                let Ok(state) = state_for_size_by_uid.lock() else {
                    return Ok(None);
                };
                let Some(p) = state.panes.iter().find(|p| p.uid == uid) else {
                    return Ok(None);
                };
                let t = lua.create_table()?;
                t.set("cols", p.cols)?;
                t.set("rows", p.rows)?;
                Ok(Some(t))
            })?,
        )?;
        // `rterm.idle_of(tab, pane)` — milliseconds since the pane's
        // PTY last produced output. Returns nil for out-of-range.
        // The documented use case for the underlying field is
        // "monitor-silence" features — a plugin pings every few
        // seconds and notifies when a long-running build pane has
        // gone quiet for >N ms.
        let state_for_idle_of = Arc::clone(&state);
        rterm.set(
            "idle_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_idle_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .map(|p| p.idle_ms))
            })?,
        )?;
        // `rterm.idle_by_uid(uid)` — stable-id sibling of `idle_of`.
        let state_for_idle_by_uid = Arc::clone(&state);
        rterm.set(
            "idle_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_idle_by_uid.lock() else {
                    return Ok(None);
                };
                Ok(state.panes.iter().find(|p| p.uid == uid).map(|p| p.idle_ms))
            })?,
        )?;
        // `rterm.active_tab()` — 1-based focused tab index (0 when there
        // are no tabs at all). Derived from the per-frame snapshot.
        let state_for_active_tab = Arc::clone(&state);
        rterm.set(
            "active_tab",
            lua.create_function(move |_, ()| {
                let idx = state_for_active_tab
                    .lock()
                    .ok()
                    .and_then(|g| g.tabs.iter().find(|t| t.focused).map(|t| t.idx + 1))
                    .unwrap_or(0);
                Ok(idx as u32)
            })?,
        )?;

        // `rterm.active_pane()` — 1-based focused pane index within the
        // focused tab (0 when nothing is focused).
        let state_for_active_pane = Arc::clone(&state);
        rterm.set(
            "active_pane",
            lua.create_function(move |_, ()| {
                let idx = state_for_active_pane
                    .lock()
                    .ok()
                    .and_then(|g| g.panes.iter().find(|p| p.focused).map(|p| p.pane + 1))
                    .unwrap_or(0);
                Ok(idx as u32)
            })?,
        )?;

        // `rterm.active_pane_uid()` — stable uid of whatever pane is
        // focused, or 0 when no pane has focus. Symmetric counterpart
        // to the `*_by_uid` setters: capture once at the moment a user
        // does something interesting, address later even after focus
        // moves on. Zero is reserved as a "no pane" sentinel by the
        // monotonic uid counter (see `Pane::new`).
        let state_for_active_pane_uid = Arc::clone(&state);
        rterm.set(
            "active_pane_uid",
            lua.create_function(move |_, ()| {
                let uid = state_for_active_pane_uid
                    .lock()
                    .ok()
                    .and_then(|g| g.panes.iter().find(|p| p.focused).map(|p| p.uid))
                    .unwrap_or(0);
                Ok(uid)
            })?,
        )?;

        // `rterm.is_search_active()` — true while the in-app search
        // overlay (Ctrl+Shift+F) is open. Pair with `search.start` /
        // `search.end` events for status-line plugins that want to
        // render a "(searching)" indicator.
        let state_for_search = Arc::clone(&state);
        rterm.set(
            "is_search_active",
            lua.create_function(move |_, ()| {
                Ok(state_for_search
                    .lock()
                    .ok()
                    .map(|g| g.search_active)
                    .unwrap_or(false))
            })?,
        )?;

        // `rterm.search_query()` — current search query string. Empty
        // when search is closed or the query buffer is blank. Plugins
        // can render the live query in a status-line badge alongside
        // the boolean `is_search_active()`.
        let state_for_query = Arc::clone(&state);
        rterm.set(
            "search_query",
            lua.create_function(move |_, ()| {
                Ok(state_for_query
                    .lock()
                    .ok()
                    .map(|g| g.search_query.clone())
                    .unwrap_or_default())
            })?,
        )?;

        // `rterm.search_regex_mode()` — true when search is active in
        // regex mode (Ctrl+R inside the overlay). Status-line plugins
        // can render a "regex" badge / different highlight colour.
        let state_for_regex = Arc::clone(&state);
        rterm.set(
            "search_regex_mode",
            lua.create_function(move |_, ()| {
                Ok(state_for_regex
                    .lock()
                    .ok()
                    .map(|g| g.search_regex_mode)
                    .unwrap_or(false))
            })?,
        )?;

        // `rterm.search_matches()` — `{current, total}` array of the
        // search overlay's match cursor. `{0, 0}` when search is
        // closed or has no matches. Status-line plugins can mirror
        // the overlay's `n/m` counter without subscribing to
        // `search.step` events.
        let state_for_match_count = Arc::clone(&state);
        rterm.set(
            "search_matches",
            lua.create_function(move |lua, ()| {
                let (cur, total) = state_for_match_count
                    .lock()
                    .ok()
                    .map(|g| (g.search_match_index, g.search_match_total))
                    .unwrap_or((0, 0));
                let t = lua.create_table()?;
                t.set(1, cur)?;
                t.set(2, total)?;
                Ok(t)
            })?,
        )?;

        // `rterm.session_uptime_ms()` — milliseconds since the
        // PluginHost was constructed (≈ rterm startup, plus the few ms
        // of pre-Lua boot). Plugins use this for "show uptime in
        // status bar" or "first-N-seconds suppress this event" gates
        // without having to capture their own start time at the first
        // `startup` event.
        let session_start = std::time::Instant::now();
        rterm.set(
            "session_uptime_ms",
            lua.create_function(move |_, ()| {
                Ok(session_start.elapsed().as_millis() as u64)
            })?,
        )?;

        // `rterm.now_ms()` — wall-clock milliseconds since the Unix
        // epoch. More precise than Lua's `os.time() * 1000` (which is
        // truncated to whole seconds) and easier than rolling
        // SystemTime arithmetic in plugin code. Pair with
        // `session_uptime_ms` for "did this happen in the last N ms"
        // gates without subtracting a stashed wall clock.
        rterm.set(
            "now_ms",
            lua.create_function(|_, ()| {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Ok(ms)
            })?,
        )?;

        let state_for_font_size = Arc::clone(&state);
        rterm.set(
            "font_size",
            lua.create_function(move |_, ()| {
                Ok(state_for_font_size
                    .lock()
                    .ok()
                    .map(|g| g.font_size)
                    .unwrap_or(0.0))
            })?,
        )?;

        let state_for_font_family = Arc::clone(&state);
        rterm.set(
            "font_family",
            lua.create_function(move |_, ()| {
                Ok(state_for_font_family
                    .lock()
                    .ok()
                    .map(|g| g.font_family.clone())
                    .unwrap_or_default())
            })?,
        )?;

        let state_for_cell_w = Arc::clone(&state);
        rterm.set(
            "cell_width",
            lua.create_function(move |_, ()| {
                Ok(state_for_cell_w
                    .lock()
                    .ok()
                    .map(|g| g.cell_width)
                    .unwrap_or(0.0))
            })?,
        )?;

        let state_for_line_h = Arc::clone(&state);
        rterm.set(
            "line_height",
            lua.create_function(move |_, ()| {
                Ok(state_for_line_h
                    .lock()
                    .ok()
                    .map(|g| g.line_height)
                    .unwrap_or(0.0))
            })?,
        )?;

        let state_for_silence = Arc::clone(&state);
        rterm.set(
            "tab_silence_ms",
            lua.create_function(move |_, ()| {
                Ok(state_for_silence
                    .lock()
                    .ok()
                    .map(|g| g.tab_silence_ms)
                    .unwrap_or(0))
            })?,
        )?;

        // `rterm.slow_command_ms()` — current threshold (ms) above
        // which `pane.slow_command` fires. `0` means disabled.
        // Sibling of `tab_silence_ms()`. Plugins that surface a
        // "slow build" UI use this to label thresholds in their
        // status overlay.
        let state_for_slow = Arc::clone(&state);
        rterm.set(
            "slow_command_ms",
            lua.create_function(move |_, ()| {
                Ok(state_for_slow
                    .lock()
                    .ok()
                    .map(|g| g.slow_command_ms)
                    .unwrap_or(0))
            })?,
        )?;

        // Read-side complements for the four `set_*` toggles. Same
        // shape: read from the snapshot, fall back to `false` if
        // the state mutex is poisoned. Plugins that toggle a flag
        // also want to display its current state in a UI.
        type FlagProj = fn(&TerminalState) -> bool;
        let flag_entries: [(&str, FlagProj); 5] = [
            ("scroll_on_output", |s| s.scroll_on_output),
            ("show_scrollbar", |s| s.show_scrollbar),
            ("bell_visual", |s| s.bell_visual),
            ("bell_urgent", |s| s.bell_urgent),
            // Global cursor-blink config flag (distinct from per-pane
            // `cursor_blink` exposed via `cursor()` / `list_panes()`).
            ("cursor_blink", |s| s.cursor_blink),
        ];
        for (name, project) in flag_entries {
            let state_for_flag = Arc::clone(&state);
            rterm.set(
                name,
                lua.create_function(move |_, ()| {
                    Ok(state_for_flag
                        .lock()
                        .ok()
                        .map(|g| project(&g))
                        .unwrap_or(false))
                })?,
            )?;
        }

        let state_for_sb_limit = Arc::clone(&state);
        rterm.set(
            "scrollback_limit",
            lua.create_function(move |_, ()| {
                Ok(state_for_sb_limit
                    .lock()
                    .ok()
                    .map(|g| g.scrollback_limit as u64)
                    .unwrap_or(0))
            })?,
        )?;

        let state_for_selection = Arc::clone(&state);
        rterm.set(
            "selection",
            lua.create_function(move |_, ()| {
                // `nil` when nothing is selected — easier to test in Lua
                // than the empty string ("if rterm.selection() then…").
                Ok(state_for_selection
                    .lock()
                    .ok()
                    .and_then(|g| g.selection_text.clone()))
            })?,
        )?;

        let state_for_opacity = Arc::clone(&state);
        rterm.set(
            "opacity",
            lua.create_function(move |_, ()| {
                Ok(state_for_opacity
                    .lock()
                    .ok()
                    .map(|g| g.opacity)
                    .unwrap_or(1.0))
            })?,
        )?;

        let state_for_focus = Arc::clone(&state);
        rterm.set(
            "window_focused",
            lua.create_function(move |_, ()| {
                Ok(state_for_focus
                    .lock()
                    .ok()
                    .map(|g| g.window_focused)
                    .unwrap_or(true))
            })?,
        )?;

        // `rterm.theme()` — live default colours as `{ fg, bg, cursor }`,
        // each an `{r, g, b}` byte triple. Returned values reflect the
        // current renderer palette (so OSC 10/11 swaps and plugin
        // `set_palette` calls are visible on the next frame). Plugins
        // pair this with the `theme` event to repaint their overlays
        // when the palette changes — `on("theme", ...)` fires, then a
        // call to `theme()` returns the new colours.
        let state_for_theme = Arc::clone(&state);
        rterm.set(
            "theme",
            lua.create_function(move |lua, ()| {
                let (fg, bg, cursor) = state_for_theme
                    .lock()
                    .ok()
                    .map(|g| (g.theme_fg, g.theme_bg, g.theme_cursor))
                    .unwrap_or(([0, 0, 0], [0, 0, 0], [0, 0, 0]));
                let t = lua.create_table()?;
                let to_rgb = |lua: &Lua, c: [u8; 3]| -> mlua::Result<Table> {
                    let rgb = lua.create_table()?;
                    rgb.set(1, c[0])?;
                    rgb.set(2, c[1])?;
                    rgb.set(3, c[2])?;
                    Ok(rgb)
                };
                t.set("fg", to_rgb(lua, fg)?)?;
                t.set("bg", to_rgb(lua, bg)?)?;
                t.set("cursor", to_rgb(lua, cursor)?)?;
                t.set("is_dark", luminance_is_dark(bg))?;
                Ok(t)
            })?,
        )?;

        // `rterm.named_palette()` — array of 16 `{r,g,b}` triples
        // for the named ANSI palette (index 0..=15). Updates as
        // OSC 4 / OSC 104 land at runtime. Theme-aware plugins use
        // this for swatch UIs without re-implementing the named
        // colour table themselves.
        let state_for_named = Arc::clone(&state);
        rterm.set(
            "named_palette",
            lua.create_function(move |lua, ()| {
                let pal = state_for_named
                    .lock()
                    .ok()
                    .map(|g| g.named_palette)
                    .unwrap_or([[0u8; 3]; 16]);
                let arr = lua.create_table()?;
                for (i, c) in pal.iter().enumerate() {
                    let rgb = lua.create_table()?;
                    rgb.set(1, c[0])?;
                    rgb.set(2, c[1])?;
                    rgb.set(3, c[2])?;
                    arr.set(i + 1, rgb)?;
                }
                Ok(arr)
            })?,
        )?;

        // `rterm.palette_color(index)` — single-slot lookup over
        // the full 8-bit indexed-colour range. Returns a `{r, g, b}`
        // table or `nil` for `index` outside 0..=255. Mirrors the
        // renderer's `indexed_color_to_rgb`: 0..=15 from the named
        // (themeable) palette, 16..=231 from the 6×6×6 cube,
        // 232..=255 from the grayscale ramp. Plugins displaying a
        // 256-colour swatch UI don't have to call
        // `named_palette()` and then re-implement the cube math.
        let state_for_slot = Arc::clone(&state);
        rterm.set(
            "palette_color",
            lua.create_function(move |lua, index: u32| {
                if index > 255 {
                    return Ok(None);
                }
                let i = index as u8;
                let rgb: [u8; 3] = if i < 16 {
                    state_for_slot
                        .lock()
                        .ok()
                        .map(|g| g.named_palette[i as usize])
                        .unwrap_or([0, 0, 0])
                } else {
                    cube_or_grayscale_rgb(i)
                };
                let t = lua.create_table()?;
                t.set(1, rgb[0])?;
                t.set(2, rgb[1])?;
                t.set(3, rgb[2])?;
                Ok(Some(t))
            })?,
        )?;

        // `rterm.nearest_palette_index({r,g,b})` — find the 8-bit
        // indexed-colour slot whose live RGB is closest (Euclidean
        // in sRGB byte space) to the given truecolour. Plugins
        // downsampling truecolour to a 256-colour environment use
        // this without rolling the search themselves.
        let state_for_nearest = Arc::clone(&state);
        rterm.set(
            "nearest_palette_index",
            lua.create_function(move |_, rgb: Table| {
                let r: u32 = rgb.get(1).unwrap_or(0);
                let g: u32 = rgb.get(2).unwrap_or(0);
                let b: u32 = rgb.get(3).unwrap_or(0);
                let target = [r.min(255) as u8, g.min(255) as u8, b.min(255) as u8];
                let named = state_for_nearest
                    .lock()
                    .ok()
                    .map(|g| g.named_palette)
                    .unwrap_or([[0u8; 3]; 16]);
                Ok(nearest_palette_index(target, &named) as u32)
            })?,
        )?;

        // `rterm.rgb_to_hex(r, g, b)` — format three 0..=255 byte
        // components as a `#RRGGBB` string. Pair with `rterm.theme()`
        // for plugins emitting HTML / CSS colour codes in status-line
        // overlays or notification bubbles. Values are clamped to byte
        // range so a plugin passing `300` doesn't trigger a panic.
        rterm.set(
            "rgb_to_hex",
            lua.create_function(|_, (r, g, b): (u32, u32, u32)| {
                let r = r.min(255) as u8;
                let g = g.min(255) as u8;
                let b = b.min(255) as u8;
                Ok(format!("#{:02X}{:02X}{:02X}", r, g, b))
            })?,
        )?;

        // `rterm.contrast_ratio({r,g,b}, {r,g,b})` — WCAG 2.x
        // contrast ratio (1.0..=21.0). 4.5 is AA body-text
        // threshold; 7.0 is AAA. Plugin theme-validators use this
        // to flag overlay colours that won't meet accessibility
        // thresholds against the live theme bg.
        rterm.set(
            "contrast_ratio",
            lua.create_function(|_, (a, b): (Table, Table)| {
                let read = |t: Table| -> [u8; 3] {
                    let r: u32 = t.get(1).unwrap_or(0);
                    let g: u32 = t.get(2).unwrap_or(0);
                    let b: u32 = t.get(3).unwrap_or(0);
                    [r.min(255) as u8, g.min(255) as u8, b.min(255) as u8]
                };
                Ok(contrast_ratio(read(a), read(b)))
            })?,
        )?;

        // `rterm.contrast_grade({r,g,b}, {r,g,b})` — convenience
        // sugar over `contrast_ratio` returning the WCAG body-text
        // label `"fail"` / `"AA"` / `"AAA"`. Plugins building a
        // theme picker use this directly in the UI label.
        rterm.set(
            "contrast_grade",
            lua.create_function(|_, (a, b): (Table, Table)| {
                let read = |t: Table| -> [u8; 3] {
                    let r: u32 = t.get(1).unwrap_or(0);
                    let g: u32 = t.get(2).unwrap_or(0);
                    let b: u32 = t.get(3).unwrap_or(0);
                    [r.min(255) as u8, g.min(255) as u8, b.min(255) as u8]
                };
                Ok(contrast_grade(contrast_ratio(read(a), read(b))))
            })?,
        )?;

        // `rterm.contrast_fg({r, g, b})` — pick a high-contrast text
        // colour (black or white) for a given background. Uses the
        // same `luminance_is_dark` threshold the `theme().is_dark`
        // flag does, so a plugin painting a colored badge on top of
        // the live theme can get readable text without rolling its
        // own math.
        rterm.set(
            "contrast_fg",
            lua.create_function(|lua, bg: Table| {
                let r: u32 = bg.get(1).unwrap_or(0);
                let g: u32 = bg.get(2).unwrap_or(0);
                let b: u32 = bg.get(3).unwrap_or(0);
                let rgb = [r.min(255) as u8, g.min(255) as u8, b.min(255) as u8];
                let pick: [u8; 3] = if luminance_is_dark(rgb) {
                    [255, 255, 255]
                } else {
                    [0, 0, 0]
                };
                let t = lua.create_table()?;
                t.set(1, pick[0])?;
                t.set(2, pick[1])?;
                t.set(3, pick[2])?;
                Ok(t)
            })?,
        )?;

        // `rterm.hex_to_rgb(hex)` — inverse of `rgb_to_hex`. Accepts
        // `#RRGGBB`, `#RGB` (short form, each digit doubled), and the
        // same forms without the leading `#`. Returns a 3-element
        // array `{r, g, b}` or nil for malformed input — plugins
        // reading hex from a config field can `pcall` / nil-check
        // without a regex of their own.
        rterm.set(
            "hex_to_rgb",
            lua.create_function(|lua, hex: String| {
                let bytes = parse_hex_rgb(&hex);
                let Some(rgb) = bytes else { return Ok(None) };
                let t = lua.create_table()?;
                t.set(1, rgb[0])?;
                t.set(2, rgb[1])?;
                t.set(3, rgb[2])?;
                Ok(Some(t))
            })?,
        )?;

        // `rterm.scroll_offset()` — focused pane's scrollback offset (in
        // lines, `0` = following live grid). Convenience around
        // `list_panes()[focused].scroll_offset`.
        let state_for_scroll = Arc::clone(&state);
        rterm.set(
            "scroll_offset",
            lua.create_function(move |_, ()| {
                let off = state_for_scroll
                    .lock()
                    .ok()
                    .and_then(|g| g.panes.iter().find(|p| p.focused).map(|p| p.scroll_offset))
                    .unwrap_or(0);
                Ok(off as u32)
            })?,
        )?;
        // `rterm.scroll_offset_of(tab, pane)` — per-pane variant,
        // 1-based indices match `list_panes`. Returns nil for
        // out-of-range pairs. Plugins watching scrollback motion
        // across a specific pane (e.g. "auto-pause output capture
        // when the user is reading history") use this without
        // iterating `list_panes()` every frame.
        let state_for_scroll_of = Arc::clone(&state);
        rterm.set(
            "scroll_offset_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_scroll_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .map(|p| p.scroll_offset as u32))
            })?,
        )?;
        // `rterm.scroll_offset_by_uid(uid)` — stable-id sibling.
        let state_for_scroll_uid = Arc::clone(&state);
        rterm.set(
            "scroll_offset_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_scroll_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .map(|p| p.scroll_offset as u32))
            })?,
        )?;
        // `rterm.scrollback_len_of(tab, pane)` — per-pane line count
        // currently held in the scrollback ring. Plugins use this
        // for "scrollback fill" status indicators or to decide
        // whether to offer a "save scrollback" prompt before close.
        let state_for_sblen_of = Arc::clone(&state);
        rterm.set(
            "scrollback_len_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_sblen_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .map(|p| p.scrollback_len as u64))
            })?,
        )?;
        // `rterm.scrollback_len_by_uid(uid)` — stable-id sibling.
        let state_for_sblen_uid = Arc::clone(&state);
        rterm.set(
            "scrollback_len_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_sblen_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .map(|p| p.scrollback_len as u64))
            })?,
        )?;

        // `rterm.shell_pid()` — OS process id of the focused pane's shell,
        // or nil when no pane is focused / the platform doesn't report one.
        // Convenience wrapper around `list_panes()[focused].pid`.
        let state_for_pid = Arc::clone(&state);
        rterm.set(
            "shell_pid",
            lua.create_function(move |_, ()| {
                let pid = state_for_pid
                    .lock()
                    .ok()
                    .and_then(|g| g.panes.iter().find(|p| p.focused).and_then(|p| p.pid));
                Ok(pid)
            })?,
        )?;
        // `rterm.shell_pid_of(tab, pane)` — per-pane shell PID
        // (stable for the pane's lifetime, unlike
        // `foreground_pgid_*` which tracks the current command's
        // process group). Plugins want this when sending signals
        // to the shell itself or correlating logs.
        let state_for_pid_of = Arc::clone(&state);
        rterm.set(
            "shell_pid_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_pid_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .and_then(|p| p.pid))
            })?,
        )?;
        let state_for_pid_uid = Arc::clone(&state);
        rterm.set(
            "shell_pid_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_pid_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .and_then(|p| p.pid))
            })?,
        )?;

        // `rterm.alt_screen()` — true when the focused pane is on the
        // alternate screen (vim, less, htop, ...). Shortcut for
        // `list_panes()[focused].alt_screen`; lets plugins gate per-tab
        // behaviour without iterating panes.
        let state_for_alt = Arc::clone(&state);
        rterm.set(
            "alt_screen",
            lua.create_function(move |_, ()| {
                let on_alt = state_for_alt
                    .lock()
                    .ok()
                    .and_then(|g| g.panes.iter().find(|p| p.focused).map(|p| p.alt_screen))
                    .unwrap_or(false);
                Ok(on_alt)
            })?,
        )?;
        // `rterm.dragging_tab()` — 1-based tab index being dragged
        // by the user, or nil. Plugins use this to suspend
        // automatic actions (e.g. "save layout on tab close")
        // while a manual drag is in flight.
        let state_for_drag = Arc::clone(&state);
        rterm.set(
            "dragging_tab",
            lua.create_function(move |_, ()| {
                Ok(state_for_drag.lock().ok().and_then(|g| g.dragging_tab))
            })?,
        )?;

        // `rterm.is_dark()` — convenience shortcut for
        // `rterm.theme().is_dark`. Status-line plugins gate
        // their accent colour choice on this and read it many
        // times per frame; the dedicated getter saves the
        // intermediate table allocation.
        let state_for_dark = Arc::clone(&state);
        rterm.set(
            "is_dark",
            lua.create_function(move |_, ()| {
                let bg = state_for_dark
                    .lock()
                    .ok()
                    .map(|g| g.theme_bg)
                    .unwrap_or([0, 0, 0]);
                Ok(luminance_is_dark(bg))
            })?,
        )?;
        // `rterm.is_light()` — exact inverse of `is_dark()`.
        // Catches the `not rterm.is_dark()` antipattern in
        // plugin code (operator precedence around `not` in Lua
        // is occasionally surprising for users coming from
        // Python / Ruby).
        let state_for_light = Arc::clone(&state);
        rterm.set(
            "is_light",
            lua.create_function(move |_, ()| {
                let bg = state_for_light
                    .lock()
                    .ok()
                    .map(|g| g.theme_bg)
                    .unwrap_or([0, 0, 0]);
                Ok(!luminance_is_dark(bg))
            })?,
        )?;

        // `rterm.reverse_screen()` — focused pane's DECSCNM (?5)
        // state. Sugar over `list_panes()[focused].reverse_screen`.
        let state_for_rev = Arc::clone(&state);
        rterm.set(
            "reverse_screen",
            lua.create_function(move |_, ()| {
                let on = state_for_rev
                    .lock()
                    .ok()
                    .and_then(|g| {
                        g.panes
                            .iter()
                            .find(|p| p.focused)
                            .map(|p| p.reverse_screen)
                    })
                    .unwrap_or(false);
                Ok(on)
            })?,
        )?;
        // `rterm.alt_screen_of(tab, pane)` — per-pane variant. Returns
        // nil for out-of-range pairs. Plugins watching a specific
        // non-focused pane (e.g. a build pane in the corner) check
        // here without sweeping `list_panes()`.
        let state_for_alt_of = Arc::clone(&state);
        rterm.set(
            "alt_screen_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_alt_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .map(|p| p.alt_screen))
            })?,
        )?;
        // `rterm.uid_of(tab, pane)` — translate a (1-based)
        // (tab, pane) index pair into the pane's stable uid.
        // Useful when a plugin wants to capture a uid before
        // remembering "the pane I just spawned" — by the next
        // frame the indices may have shifted (sibling splits).
        // Returns nil for out-of-range pairs.
        let state_for_uid_of = Arc::clone(&state);
        rterm.set(
            "uid_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_uid_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .map(|p| p.uid))
            })?,
        )?;
        // `rterm.indices_of_uid(uid)` — inverse of `uid_of`.
        // Returns a `{tab, pane}` table with 1-based indices, or
        // nil for a stale uid (pane closed since capture).
        // Plugins that captured a uid earlier query this when
        // they need to call an indexed-only API (legacy mutators)
        // on the same pane.
        let state_for_idx_of_uid = Arc::clone(&state);
        rterm.set(
            "indices_of_uid",
            lua.create_function(move |lua, uid: u64| {
                let Ok(state) = state_for_idx_of_uid.lock() else {
                    return Ok(None);
                };
                let Some(p) = state.panes.iter().find(|p| p.uid == uid) else {
                    return Ok(None);
                };
                let t = lua.create_table()?;
                t.set("tab", (p.tab + 1) as u32)?;
                t.set("pane", (p.pane + 1) as u32)?;
                Ok(Some(t))
            })?,
        )?;

        // `rterm.alt_screen_by_uid(uid)` — stable-id sibling.
        let state_for_alt_uid = Arc::clone(&state);
        rterm.set(
            "alt_screen_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_alt_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .map(|p| p.alt_screen))
            })?,
        )?;
        // `rterm.reverse_screen_of(tab, pane)` — DECSCNM (?5)
        // state per pane. Status-line plugins use it to invert
        // their own overlay glyph in sync with the terminal.
        let state_for_rev_of = Arc::clone(&state);
        rterm.set(
            "reverse_screen_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_rev_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .map(|p| p.reverse_screen))
            })?,
        )?;
        let state_for_rev_uid = Arc::clone(&state);
        rterm.set(
            "reverse_screen_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_rev_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .map(|p| p.reverse_screen))
            })?,
        )?;

        // `rterm.last_exit_code()` — last shell exit captured via OSC 133;D.
        // Returns `nil` until the first command has finished.
        let state_for_exit = Arc::clone(&state);
        rterm.set(
            "last_exit_code",
            lua.create_function(move |_, ()| {
                Ok(state_for_exit.lock().ok().and_then(|g| g.last_exit_code))
            })?,
        )?;
        // `rterm.last_exit_code_of(tab, pane)` — per-pane variant.
        // Distinct from the window-global `last_exit_code()`,
        // which only carries the most-recent exit. Plugins
        // tracking many panes (e.g. a CI dashboard) use the
        // per-pane form so an exit in pane B doesn't shadow an
        // older exit they're still rendering for pane A.
        let state_for_exit_of = Arc::clone(&state);
        rterm.set(
            "last_exit_code_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_exit_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .and_then(|p| p.last_exit_code))
            })?,
        )?;
        let state_for_exit_uid = Arc::clone(&state);
        rterm.set(
            "last_exit_code_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_exit_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .and_then(|p| p.last_exit_code))
            })?,
        )?;
        // `rterm.bell_muted_of(tab, pane)` — read-side companion
        // for `set_pane_bell_muted(tab, pane, muted)`. Returns
        // nil for out-of-range. Plugins rendering a "🔕" badge in
        // a custom tab bar query this each frame.
        let state_for_bm_of = Arc::clone(&state);
        rterm.set(
            "bell_muted_of",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_bm_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .map(|p| p.bell_muted))
            })?,
        )?;
        let state_for_bm_uid = Arc::clone(&state);
        rterm.set(
            "bell_muted_by_uid",
            lua.create_function(move |_, uid: u64| {
                let Ok(state) = state_for_bm_uid.lock() else {
                    return Ok(None);
                };
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .map(|p| p.bell_muted))
            })?,
        )?;
        // `rterm.progress_of(tab, pane)` — OSC 9;4 progress per
        // pane as a `{state, state_name, percent}` table. Returns
        // nil for out-of-range or when the pane has no active
        // progress reported. Mirrors how `list_panes()[i].progress`
        // is materialised but as a single-pane lookup.
        let state_for_prog_of = Arc::clone(&state);
        rterm.set(
            "progress_of",
            lua.create_function(move |lua, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_prog_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                let Some((s, pct)) = state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                    .and_then(|p| p.progress)
                else {
                    return Ok(None);
                };
                let t = lua.create_table()?;
                t.set("state", s)?;
                t.set("state_name", progress_state_name(s))?;
                t.set("percent", pct)?;
                Ok(Some(t))
            })?,
        )?;
        let state_for_prog_uid = Arc::clone(&state);
        rterm.set(
            "progress_by_uid",
            lua.create_function(move |lua, uid: u64| {
                let Ok(state) = state_for_prog_uid.lock() else {
                    return Ok(None);
                };
                let Some((s, pct)) = state
                    .panes
                    .iter()
                    .find(|p| p.uid == uid)
                    .and_then(|p| p.progress)
                else {
                    return Ok(None);
                };
                let t = lua.create_table()?;
                t.set("state", s)?;
                t.set("state_name", progress_state_name(s))?;
                t.set("percent", pct)?;
                Ok(Some(t))
            })?,
        )?;

        // `rterm.snapshot()` — single-call rollup of the per-frame state.
        // Skips heavy fields (`grid_text`, full panes/tabs) for cheapness;
        // callers needing those should use the dedicated APIs.
        let state_for_snapshot = Arc::clone(&state);
        rterm.set(
            "snapshot",
            lua.create_function(move |lua, ()| {
                let snap = state_for_snapshot.lock().ok().map(|g| g.clone());
                let t = lua.create_table()?;
                if let Some(s) = snap {
                    if let Some(c) = s.cwd { t.set("cwd", c)?; }
                    if let Some(title) = s.title { t.set("title", title)?; }
                    t.set("cols", s.cols)?;
                    t.set("rows", s.rows)?;
                    t.set("font_size", s.font_size)?;
                    t.set("opacity", s.opacity)?;
                    t.set("tab_count", s.tabs.len())?;
                    t.set("pane_count", s.panes.len())?;
                    // Pane count of the focused tab specifically —
                    // status-line plugins doing "Pane 2/4" displays
                    // need this scoped value (the flat `pane_count`
                    // sums across tabs).
                    let active_tab_pane_count = s
                        .tabs
                        .iter()
                        .find(|tab| tab.focused)
                        .map(|tab| tab.pane_count)
                        .unwrap_or(0);
                    t.set("active_tab_pane_count", active_tab_pane_count)?;
                    if let Some(code) = s.last_exit_code {
                        t.set("last_exit_code", code)?;
                    }
                    // Window focus + 1-based active tab / pane indices
                    // (0 sentinel when nothing is focused). Plus the
                    // focused pane's uid so a single snapshot call lets
                    // a plugin stash a stable identifier for whatever
                    // the user is looking at right now.
                    t.set("window_focused", s.window_focused)?;
                    let active_tab_1based = s
                        .tabs
                        .iter()
                        .find(|tab| tab.focused)
                        .map(|tab| (tab.idx + 1) as u32)
                        .unwrap_or(0);
                    t.set("active_tab", active_tab_1based)?;
                    let focused_pane = s.panes.iter().find(|p| p.focused);
                    let focused_pane_idx = focused_pane
                        .map(|p| (p.pane + 1) as u32)
                        .unwrap_or(0);
                    let focused_pane_uid = focused_pane
                        .map(|p| p.uid)
                        .unwrap_or(0);
                    t.set("active_pane", focused_pane_idx)?;
                    t.set("active_pane_uid", focused_pane_uid)?;
                    // Alt-screen state of the focused pane — common
                    // status-line plugin signal ("vim is open, dim my
                    // git indicator" / "shell prompt, show command
                    // history"). False when no pane is focused.
                    let focused_alt = focused_pane.is_some_and(|p| p.alt_screen);
                    t.set("alt_screen", focused_alt)?;
                }
                Ok(t)
            })?,
        )?;

        let state_for_tab_count = Arc::clone(&state);
        rterm.set(
            "tab_count",
            lua.create_function(move |_, ()| {
                let n = state_for_tab_count
                    .lock()
                    .ok()
                    .map(|g| {
                        // Count distinct tab indices in the pane list.
                        let mut max_tab = 0u32;
                        let mut any = false;
                        for p in &g.panes {
                            any = true;
                            if (p.tab as u32) >= max_tab {
                                max_tab = p.tab as u32;
                            }
                        }
                        if any { max_tab + 1 } else { 0 }
                    })
                    .unwrap_or(0);
                Ok(n)
            })?,
        )?;

        // Bulk-count sugars over the snapshot. Each entry is a
        // (name, predicate) pair driving a small one-line getter:
        //   `rterm.unread_tab_count()`  — tabs.filter(unread)
        //   `rterm.zoomed_tab_count()`  — tabs.filter(zoomed)
        //   `rterm.alt_pane_count()`    — panes.filter(alt_screen)
        //   `rterm.muted_pane_count()`  — panes.filter(bell_muted)
        // Status-line / overlay plugins render badges from these
        // without iterating `tabs()` / `list_panes()` by hand.
        type TabPred = fn(&TabInfo) -> bool;
        let tab_count_entries: [(&str, TabPred); 2] = [
            ("unread_tab_count", |t| t.unread),
            ("zoomed_tab_count", |t| t.zoomed),
        ];
        for (name, project) in tab_count_entries {
            let state_for_cnt = Arc::clone(&state);
            rterm.set(
                name,
                lua.create_function(move |_, ()| {
                    let n = state_for_cnt
                        .lock()
                        .ok()
                        .map(|g| g.tabs.iter().filter(|t| project(t)).count() as u32)
                        .unwrap_or(0);
                    Ok(n)
                })?,
            )?;
        }
        type PanePred = fn(&PaneInfo) -> bool;
        let pane_count_entries: [(&str, PanePred); 3] = [
            ("alt_pane_count", |p| p.alt_screen),
            ("muted_pane_count", |p| p.bell_muted),
            // `focused_pane_count` — should be 0 or 1 in steady
            // state. Plugins / tests use it as a sanity check
            // (a snapshot with 2+ focused panes is a regression).
            ("focused_pane_count", |p| p.focused),
        ];
        for (name, project) in pane_count_entries {
            let state_for_cnt = Arc::clone(&state);
            rterm.set(
                name,
                lua.create_function(move |_, ()| {
                    let n = state_for_cnt
                        .lock()
                        .ok()
                        .map(|g| g.panes.iter().filter(|p| project(p)).count() as u32)
                        .unwrap_or(0);
                    Ok(n)
                })?,
            )?;
        }

        let state_for_pane_count = Arc::clone(&state);
        rterm.set(
            "pane_count",
            lua.create_function(move |_, tab: Option<u32>| {
                let count = state_for_pane_count
                    .lock()
                    .ok()
                    .map(|g| {
                        let target_tab = match tab {
                            Some(t) => t.saturating_sub(1) as usize,
                            None => g
                                .panes
                                .iter()
                                .find(|p| p.focused)
                                .map(|p| p.tab)
                                .unwrap_or(0),
                        };
                        g.panes.iter().filter(|p| p.tab == target_tab).count()
                    })
                    .unwrap_or(0) as u32;
                Ok(count)
            })?,
        )?;

        let state_for_tabs = Arc::clone(&state);
        rterm.set(
            "tabs",
            lua.create_function(move |lua, ()| {
                let tabs = state_for_tabs
                    .lock()
                    .ok()
                    .map(|g| g.tabs.clone())
                    .unwrap_or_default();
                let arr = lua.create_table()?;
                for (i, t) in tabs.iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("idx", t.idx + 1)?;
                    entry.set("focused", t.focused)?;
                    entry.set("pane_count", t.pane_count)?;
                    entry.set("focused_pane", t.focused_pane + 1)?;
                    entry.set("focused_pane_uid", t.focused_pane_uid)?;
                    entry.set("zoomed", t.zoomed)?;
                    if let Some(ct) = t.custom_title.as_ref() {
                        entry.set("custom_title", ct.clone())?;
                    }
                    entry.set("idle_ms", t.idle_ms)?;
                    entry.set("unread", t.unread)?;
                    if let Some((state, pct)) = t.progress {
                        let prog = lua.create_table()?;
                        prog.set("state", state)?;
                        prog.set("state_name", progress_state_name(state))?;
                        prog.set("percent", pct)?;
                        entry.set("progress", prog)?;
                    }
                    arr.set(i + 1, entry)?;
                }
                Ok(arr)
            })?,
        )?;

        // `rterm.terminal_text()` (no args) returns the focused pane's
        // visible grid text — historical behaviour. With `(tab, pane)`
        // 1-based indices (matching `list_panes`), returns the matching
        // pane's text. Returns `nil` for an out-of-range pair.
        // `rterm.cursor()` — focused pane's cursor as a table
        // `{ row = N, col = N, visible = bool }`. 1-based to match the
        // terminal-side convention. Returns nil when no pane is focused
        // (rare race: between tab/pane spawn and the first snapshot).
        let state_for_cursor = Arc::clone(&state);
        rterm.set(
            "cursor",
            lua.create_function(move |lua, ()| {
                let Ok(state) = state_for_cursor.lock() else {
                    return Ok(None);
                };
                let Some(p) = state.panes.iter().find(|p| p.focused) else {
                    return Ok(None);
                };
                let t = lua.create_table()?;
                t.set("row", p.cursor_row)?;
                t.set("col", p.cursor_col)?;
                t.set("visible", p.cursor_visible)?;
                t.set("shape", p.cursor_shape.clone())?;
                t.set("blink", p.cursor_blink)?;
                Ok(Some(t))
            })?,
        )?;

        // `rterm.cursor_of(tab, pane)` — per-pane variant of `cursor()`.
        // 1-based indices match `list_panes`. Returns nil when the
        // `(tab, pane)` pair is out of range. Useful for status-line
        // plugins that show "row:col" for a non-focused pane (e.g.
        // monitoring the cursor in a background editor pane).
        let state_for_cursor_of = Arc::clone(&state);
        rterm.set(
            "cursor_of",
            lua.create_function(move |lua, (tab, pane): (u32, u32)| {
                let Ok(state) = state_for_cursor_of.lock() else {
                    return Ok(None);
                };
                let tab_idx = tab.saturating_sub(1) as usize;
                let pane_idx = pane.saturating_sub(1) as usize;
                let Some(p) = state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab_idx && p.pane == pane_idx)
                else {
                    return Ok(None);
                };
                let t = lua.create_table()?;
                t.set("row", p.cursor_row)?;
                t.set("col", p.cursor_col)?;
                t.set("visible", p.cursor_visible)?;
                t.set("shape", p.cursor_shape.clone())?;
                t.set("blink", p.cursor_blink)?;
                Ok(Some(t))
            })?,
        )?;

        // `rterm.cursor_by_uid(uid)` — stable-id sibling of
        // `cursor_of(tab, pane)`. Returns `{row, col, visible}` or nil
        // for unknown uid. Plugins that capture a pane uid once
        // (`find_pane{uid=…}`) and want to keep watching its cursor
        // through reorders / splits use this instead of recomputing
        // `(tab, pane)` each frame.
        let state_for_cursor_by_uid = Arc::clone(&state);
        rterm.set(
            "cursor_by_uid",
            lua.create_function(move |lua, uid: u64| {
                let Ok(state) = state_for_cursor_by_uid.lock() else {
                    return Ok(None);
                };
                let Some(p) = state.panes.iter().find(|p| p.uid == uid) else {
                    return Ok(None);
                };
                let t = lua.create_table()?;
                t.set("row", p.cursor_row)?;
                t.set("col", p.cursor_col)?;
                t.set("visible", p.cursor_visible)?;
                t.set("shape", p.cursor_shape.clone())?;
                t.set("blink", p.cursor_blink)?;
                Ok(Some(t))
            })?,
        )?;

        let state_for_text = Arc::clone(&state);
        rterm.set(
            "terminal_text",
            lua.create_function(move |_, args: mlua::Variadic<u32>| {
                let Ok(state) = state_for_text.lock() else {
                    return Ok(None);
                };
                if args.is_empty() {
                    return Ok(Some(state.grid_text.clone()));
                }
                if args.len() < 2 {
                    return Ok(None);
                }
                let tab = args[0].saturating_sub(1) as usize;
                let pane = args[1].saturating_sub(1) as usize;
                Ok(state
                    .panes
                    .iter()
                    .find(|p| p.tab == tab && p.pane == pane)
                    .map(|p| p.text.clone()))
            })?,
        )?;

        // `rterm.terminal_text_by_uid(uid)` — sibling of
        // `terminal_text(tab, pane)` that addresses by stable uid.
        // Returns the pane's visible grid text or nil when the uid no
        // longer points at a live pane.
        let state_for_text_uid = Arc::clone(&state);
        rterm.set(
            "terminal_text_by_uid",
            lua.create_function(move |_, uid: u64| {
                Ok(state_for_text_uid
                    .lock()
                    .ok()
                    .and_then(|g| {
                        g.panes
                            .iter()
                            .find(|p| p.uid == uid)
                            .map(|p| p.text.clone())
                    }))
            })?,
        )?;

        // `rterm.scrollback_text(max_lines?)` — focused pane's recent
        // scrollback as a `\n`-joined string. Renderer caps the snapshot
        // at a fixed line count (currently 500); when `max_lines` is
        // supplied, the result is tail-sliced to that many lines so a
        // plugin asking for the last 20 doesn't have to walk the full
        // tail itself. Empty when the focused pane has no scrollback or
        // is on the alt screen.
        let state_for_scrollback = Arc::clone(&state);
        rterm.set(
            "scrollback_text",
            lua.create_function(move |_, max_lines: Option<u32>| {
                let Ok(state) = state_for_scrollback.lock() else {
                    return Ok(String::new());
                };
                let text = state.scrollback_text.clone();
                let Some(n) = max_lines else {
                    return Ok(text);
                };
                if n == 0 || text.is_empty() {
                    return Ok(String::new());
                }
                // Tail-slice in bytes (cheap for ASCII shell output,
                // correct for UTF-8 since we split on `\n` which is a
                // single byte and never appears inside a multi-byte
                // codepoint).
                let want = n as usize;
                let total = text.split('\n').count();
                if total <= want {
                    return Ok(text);
                }
                let skip = total - want;
                let tail: String = text
                    .split('\n')
                    .skip(skip)
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(tail)
            })?,
        )?;

        // `rterm.scrollback_text_by_uid(uid, max_lines?)` — uid-addressed
        // sibling of `scrollback_text_of(tab, pane, ...)`. Plugins that
        // stashed a pane uid earlier can read its capped tail without
        // re-resolving the index pair. Empty string when the uid no
        // longer points at a live pane.
        let state_for_scrollback_by_uid = Arc::clone(&state);
        rterm.set(
            "scrollback_text_by_uid",
            lua.create_function(
                move |_, (uid, max_lines): (u64, Option<u32>)| {
                    let Ok(state) = state_for_scrollback_by_uid.lock() else {
                        return Ok(String::new());
                    };
                    let Some(p) = state.panes.iter().find(|p| p.uid == uid)
                    else {
                        return Ok(String::new());
                    };
                    let text = p.scrollback_tail.clone();
                    let Some(n) = max_lines else {
                        return Ok(text);
                    };
                    if n == 0 || text.is_empty() {
                        return Ok(String::new());
                    }
                    let want = n as usize;
                    let total = text.split('\n').count();
                    if total <= want {
                        return Ok(text);
                    }
                    let skip = total - want;
                    let tail: String = text
                        .split('\n')
                        .skip(skip)
                        .collect::<Vec<_>>()
                        .join("\n");
                    Ok(tail)
                },
            )?,
        )?;

        // `rterm.scrollback_text_of(tab, pane, max_lines?)` — per-pane
        // variant of `scrollback_text`. Uses each pane's smaller
        // capped tail (set by the renderer at a lower line count to
        // keep total per-frame allocation bounded as the pane count
        // grows). Returns empty string when (tab, pane) misses or the
        // pane is on the alt screen.
        let state_for_scrollback_of = Arc::clone(&state);
        rterm.set(
            "scrollback_text_of",
            lua.create_function(
                move |_, (tab, pane, max_lines): (u32, u32, Option<u32>)| {
                    let tab = tab.saturating_sub(1) as usize;
                    let pane = pane.saturating_sub(1) as usize;
                    let Ok(state) = state_for_scrollback_of.lock() else {
                        return Ok(String::new());
                    };
                    let Some(p) = state
                        .panes
                        .iter()
                        .find(|p| p.tab == tab && p.pane == pane)
                    else {
                        return Ok(String::new());
                    };
                    let text = p.scrollback_tail.clone();
                    let Some(n) = max_lines else {
                        return Ok(text);
                    };
                    if n == 0 || text.is_empty() {
                        return Ok(String::new());
                    }
                    let want = n as usize;
                    let total = text.split('\n').count();
                    if total <= want {
                        return Ok(text);
                    }
                    let skip = total - want;
                    let tail: String = text
                        .split('\n')
                        .skip(skip)
                        .collect::<Vec<_>>()
                        .join("\n");
                    Ok(tail)
                },
            )?,
        )?;

        // `rterm.find_pane({ title?, cwd?, foreground_process?,
        // case_insensitive? })` — first pane in DFS order whose
        // `PaneInfo` matches every supplied field (substring). Default
        // is case-sensitive ASCII compare; `case_insensitive = true`
        // lowercases both sides before comparing. Returns
        // `{ tab = N, pane = M }` (1-based, matching `list_panes`) or
        // nil. Plugins use this for "send Esc to whichever pane is
        // running vim" / "scroll the pane whose cwd is the project root"
        // workflows without iterating `list_panes()` themselves.
        let state_for_find = Arc::clone(&state);
        rterm.set(
            "find_pane",
            lua.create_function(move |lua, opts: Option<Table>| {
                let title_q = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("title").ok().flatten());
                let cwd_q = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("cwd").ok().flatten());
                let fg_q = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("foreground_process").ok().flatten());
                // Exact-match by stable uid is the cheapest path when a
                // plugin already stashed an identifier (no substring
                // search) — short-circuits the loop below.
                let uid_q = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("uid").ok().flatten());
                let case_insensitive = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<bool>>("case_insensitive").ok().flatten())
                    .unwrap_or(false);
                // All-empty / missing opts → no predicate, return nil so
                // plugins don't silently get pane 1:1 back.
                if title_q.is_none() && cwd_q.is_none() && fg_q.is_none() && uid_q.is_none() {
                    return Ok(None);
                }
                // Lowercase the needles once when case-insensitive so the
                // hot loop only does a per-haystack lowercase.
                let prep = |s: Option<String>| -> Option<String> {
                    s.map(|q| if case_insensitive { q.to_lowercase() } else { q })
                };
                let title_q = prep(title_q);
                let cwd_q = prep(cwd_q);
                let fg_q = prep(fg_q);
                let matches = |haystack: &str, needle: &str| {
                    if case_insensitive {
                        haystack.to_lowercase().contains(needle)
                    } else {
                        haystack.contains(needle)
                    }
                };
                let Ok(state) = state_for_find.lock() else {
                    return Ok(None);
                };
                let hit = state.panes.iter().find(|p| {
                    if let Some(want) = uid_q {
                        if p.uid != want {
                            return false;
                        }
                    }
                    if let Some(q) = title_q.as_deref() {
                        if !matches(&p.title, q) {
                            return false;
                        }
                    }
                    if let Some(q) = cwd_q.as_deref() {
                        if !p.cwd.as_deref().is_some_and(|c| matches(c, q)) {
                            return false;
                        }
                    }
                    if let Some(q) = fg_q.as_deref() {
                        if !p
                            .foreground_process
                            .as_deref()
                            .map(|f| matches(f, q))
                            .unwrap_or(false)
                        {
                            return false;
                        }
                    }
                    true
                });
                let Some(p) = hit else { return Ok(None) };
                let t = lua.create_table()?;
                t.set("tab", (p.tab + 1) as u32)?;
                t.set("pane", (p.pane + 1) as u32)?;
                t.set("uid", p.uid)?;
                // Surface focused/title/cwd too so plugins can decide
                // "is the matched pane already the focused one?" or
                // "what was the matched pane's title?" without a
                // follow-up `list_panes()` walk. Cheap copies of values
                // we already had to inspect for the match.
                t.set("focused", p.focused)?;
                t.set("title", p.title.clone())?;
                if let Some(c) = p.cwd.as_ref() {
                    t.set("cwd", c.clone())?;
                }
                Ok(Some(t))
            })?,
        )?;

        // `rterm.find_tab({ title?, case_insensitive? })` — first tab
        // whose `custom_title` matches the substring `title`. Default is
        // case-sensitive ASCII compare; `case_insensitive = true` lowercases
        // both sides. Returns `{ idx, focused, focused_pane, focused_pane_uid,
        // custom_title }` (1-based) or nil. Use case: "switch to the tab the
        // user named 'logs'" via `rterm.focus_tab(rterm.find_tab({title="logs"}).idx)`.
        let state_for_find_tab = Arc::clone(&state);
        rterm.set(
            "find_tab",
            lua.create_function(move |lua, opts: Option<Table>| {
                let title_q = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("title").ok().flatten());
                let case_insensitive = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<bool>>("case_insensitive").ok().flatten())
                    .unwrap_or(false);
                let Some(needle) = title_q else { return Ok(None) };
                let needle = if case_insensitive {
                    needle.to_lowercase()
                } else {
                    needle
                };
                let Ok(state) = state_for_find_tab.lock() else {
                    return Ok(None);
                };
                let hit = state.tabs.iter().find(|t| {
                    let haystack = t.custom_title.as_deref().unwrap_or("");
                    let haystack_norm = if case_insensitive {
                        haystack.to_lowercase()
                    } else {
                        haystack.to_string()
                    };
                    !needle.is_empty() && haystack_norm.contains(&needle)
                });
                let Some(t) = hit else { return Ok(None) };
                let entry = lua.create_table()?;
                entry.set("idx", t.idx + 1)?;
                entry.set("focused", t.focused)?;
                entry.set("focused_pane", t.focused_pane + 1)?;
                entry.set("focused_pane_uid", t.focused_pane_uid)?;
                if let Some(ct) = t.custom_title.as_ref() {
                    entry.set("custom_title", ct.clone())?;
                }
                Ok(Some(entry))
            })?,
        )?;

        // `rterm.copy_pane(tab, pane)` — yank that pane's visible text to
        // the system clipboard via the same channel as `rterm.copy(text)`.
        // Returns `true` on success, `false` when the pane doesn't exist
        // or its grid is empty. Spares plugins from composing
        // `rterm.copy(rterm.terminal_text(tab, pane))` by hand and avoids
        // the awkward `nil`-check that requires.
        let copy_for_pane = Arc::clone(&pending_copy);
        let state_for_copy_pane = Arc::clone(&state);
        rterm.set(
            "copy_pane",
            lua.create_function(move |_, (tab, pane): (u32, u32)| {
                let tab = tab.saturating_sub(1) as usize;
                let pane = pane.saturating_sub(1) as usize;
                let text = state_for_copy_pane
                    .lock()
                    .ok()
                    .and_then(|g| {
                        g.panes
                            .iter()
                            .find(|p| p.tab == tab && p.pane == pane)
                            .map(|p| p.text.clone())
                    });
                let Some(t) = text else { return Ok(false) };
                if t.is_empty() {
                    return Ok(false);
                }
                if let Ok(mut slot) = copy_for_pane.lock() {
                    *slot = Some(t);
                }
                Ok(true)
            })?,
        )?;

        // `rterm.copy_pane_by_uid(uid)` — sibling of `copy_pane(tab,
        // pane)` that addresses by stable uid. Returns `true` on
        // success, `false` when the uid points at no live pane or its
        // grid is empty. Pure snapshot read — no App round-trip needed.
        let copy_for_pane_uid = Arc::clone(&pending_copy);
        let state_for_copy_pane_uid = Arc::clone(&state);
        rterm.set(
            "copy_pane_by_uid",
            lua.create_function(move |_, uid: u64| {
                let text = state_for_copy_pane_uid
                    .lock()
                    .ok()
                    .and_then(|g| {
                        g.panes
                            .iter()
                            .find(|p| p.uid == uid)
                            .map(|p| p.text.clone())
                    });
                let Some(t) = text else { return Ok(false) };
                if t.is_empty() {
                    return Ok(false);
                }
                if let Ok(mut slot) = copy_for_pane_uid.lock() {
                    *slot = Some(t);
                }
                Ok(true)
            })?,
        )?;

        let state_for_panes = Arc::clone(&state);
        rterm.set(
            "list_panes",
            lua.create_function(move |lua, ()| {
                let panes = state_for_panes
                    .lock()
                    .ok()
                    .map(|g| g.panes.clone())
                    .unwrap_or_default();
                let arr = lua.create_table()?;
                for (i, p) in panes.iter().enumerate() {
                    let t = lua.create_table()?;
                    t.set("tab", p.tab + 1)?; // 1-based for Lua idiom
                    t.set("pane", p.pane + 1)?;
                    t.set("uid", p.uid)?;
                    t.set("title", p.title.clone())?;
                    t.set("focused", p.focused)?;
                    t.set("idle_ms", p.idle_ms)?;
                    t.set("scroll_offset", p.scroll_offset)?;
                    t.set("alt_screen", p.alt_screen)?;
                    t.set("reverse_screen", p.reverse_screen)?;
                    if let Some(c) = p.cwd.as_ref() {
                        t.set("cwd", c.clone())?;
                    }
                    t.set("cols", p.cols)?;
                    t.set("rows", p.rows)?;
                    t.set("cursor_row", p.cursor_row)?;
                    t.set("cursor_col", p.cursor_col)?;
                    t.set("scrollback_len", p.scrollback_len)?;
                    t.set("cursor_visible", p.cursor_visible)?;
                    t.set("cursor_shape", p.cursor_shape.clone())?;
                    t.set("cursor_blink", p.cursor_blink)?;
                    t.set("mouse_mode", p.mouse_mode.clone())?;
                    t.set("prompt_marks", p.prompt_marks)?;
                    t.set("command_marks", p.command_marks)?;
                    if let Some(pid) = p.pid {
                        t.set("pid", pid)?;
                    }
                    if let Some(pgid) = p.foreground_pgid {
                        t.set("foreground_pgid", pgid)?;
                    }
                    if let Some(name) = p.foreground_process.as_ref() {
                        t.set("foreground_process", name.clone())?;
                    }
                    t.set("bell_muted", p.bell_muted)?;
                    if let Some(code) = p.last_exit_code {
                        t.set("last_exit_code", code)?;
                    }
                    if let Some((state, pct)) = p.progress {
                        // Expose as a nested table so plugins can branch
                        // on `if pane.progress and pane.progress.state_name == "error"`.
                        let prog = lua.create_table()?;
                        prog.set("state", state)?;
                        prog.set("state_name", progress_state_name(state))?;
                        prog.set("percent", pct)?;
                        t.set("progress", prog)?;
                    }
                    arr.set(i + 1, t)?;
                }
                Ok(arr)
            })?,
        )?;

        lua.globals().set("rterm", rterm)?;

        Ok(Self {
            lua,
            handlers,
            actions,
            pending_tab_titles,
            pending_window_title,
            pending_pane_titles,
            pending_pane_titles_by_uid,
            pending_scrollback_limit,
            pending_tab_silence_ms,
            pending_cursor_blink,
            pending_show_scrollbar,
            pending_scroll_on_output,
            pending_bell_visual,
            pending_bell_urgent,
            pending_font_family,
            pending_guake,
            pending_pane_bell_mute,
            pending_pane_bell_mute_by_uid,
            pending_slow_command_ms,
            pending_routed_input,
            pending_routed_input_by_uid,
            pending_attention,
            cmd_tx,
            cmd_rx,
            pending_focus,
            pending_focus_by_uid,
            pending_tab_focus,
            pending_copy,
            clipboard_reader,
            config_dir,
            shell_program,
            cache_dir,
            builtin_actions,
            builtin_action_labels,
            builtin_events,
            pending_scroll_to_line,
            pending_start_search,
            pending_font_size,
            pending_opacity,
            pending_bell,
            pending_palette,
            pending_theme,
            state,
            match_rules,
            exec_deadline,
        })
    }

    /// Run `f` (which invokes Lua user code) under the instruction-hook
    /// watchdog: arm the deadline, call, disarm. Once the budget is
    /// exceeded the hook aborts the chunk with a Lua RuntimeError,
    /// which surfaces through the normal per-handler error logging —
    /// the host and every other handler keep working.
    fn with_exec_budget<R>(&self, budget: Duration, f: impl FnOnce() -> R) -> R {
        self.exec_deadline.arm(budget);
        let out = f();
        self.exec_deadline.disarm();
        out
    }

    /// Test `line` against every registered match rule. Returns
    /// `(rule_name, capture_groups)` for each rule that fired, in
    /// registration order. For substring rules `capture_groups` is empty;
    /// for regex rules it's the numbered groups (1..) of the FIRST match
    /// on the line — group 0 (the whole match) is intentionally omitted
    /// since the App also forwards the full line. Missing optional groups
    /// (`(.+)?` that didn't capture) become empty strings so positional
    /// indexing stays stable.
    pub fn match_output_line(&self, line: &str) -> Vec<(String, Vec<String>)> {
        let Ok(rules) = self.match_rules.lock() else { return Vec::new() };
        let mut out = Vec::new();
        for r in rules.iter() {
            match &r.kind {
                MatchKind::Substring(needle) => {
                    if line.contains(needle.as_str()) {
                        out.push((r.name.clone(), Vec::new()));
                    }
                }
                MatchKind::Regex(re) => {
                    if let Some(caps) = re.captures(line) {
                        let groups: Vec<String> = caps
                            .iter()
                            .skip(1)
                            .map(|m| m.map(|x| x.as_str().to_string()).unwrap_or_default())
                            .collect();
                        out.push((r.name.clone(), groups));
                    }
                }
            }
        }
        out
    }

    /// Snapshot the list of registered match-rule names for tests / introspection.
    pub fn match_rule_names(&self) -> Vec<String> {
        self.match_rules
            .lock()
            .map(|g| g.iter().map(|r| r.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Take the latest `rterm.bell()` request (clears it).
    pub fn take_pending_bell(&self) -> bool {
        self.pending_bell
            .lock()
            .map(|mut g| std::mem::replace(&mut *g, false))
            .unwrap_or(false)
    }

    /// Take the latest plugin-supplied palette snapshot from `set_palette`.
    pub fn take_pending_palette(&self) -> Option<PluginPalette> {
        take_slot(&self.pending_palette)
    }

    /// Take the latest built-in theme request from `rterm.set_theme(name)`.
    /// Returns the canonical theme name; App resolves it to a palette.
    pub fn take_pending_theme(&self) -> Option<String> {
        take_slot(&self.pending_theme)
    }

    /// The list of theme names the Lua API will accept in
    /// `rterm.set_theme(name)`. Exposed so external tests (rterm-app)
    /// can pin this against the renderer's `palette::builtin_themes()`
    /// — they have to stay in lockstep.
    pub fn known_theme_names(&self) -> Vec<String> {
        self.lua
            .load("return rterm.themes()")
            .eval::<Vec<String>>()
            .unwrap_or_default()
    }

    /// Take the latest font-size request from `rterm.set_font_size(size)`.
    pub fn take_pending_font_size(&self) -> Option<f32> {
        take_slot(&self.pending_font_size)
    }

    /// Take the latest opacity request from `rterm.set_opacity(value)`.
    /// Returns a value already clamped to `0.0..=1.0`.
    pub fn take_pending_opacity(&self) -> Option<f32> {
        take_slot(&self.pending_opacity)
    }


    /// Take the latest logical line target from `rterm.scroll_to_line(line)`.
    pub fn take_pending_scroll_to_line(&self) -> Option<usize> {
        take_slot(&self.pending_scroll_to_line)
    }

    /// Take the latest `rterm.start_search` request as `(query, regex_mode)`.
    pub fn take_pending_start_search(&self) -> Option<(String, bool)> {
        take_slot(&self.pending_start_search)
    }

    /// Install the clipboard reader used by `rterm.read_clipboard()`. Called
    /// once at startup by the host (rterm-app), which owns the arboard
    /// dependency.
    pub fn set_clipboard_reader(&self, reader: ClipboardReader) {
        if let Ok(mut g) = self.clipboard_reader.lock() {
            *g = Some(reader);
        }
    }

    /// Set the path Lua sees from `rterm.config_dir()`.
    pub fn set_config_dir(&self, path: impl Into<String>) {
        if let Ok(mut g) = self.config_dir.lock() {
            *g = path.into();
        }
    }

    /// Set the value Lua sees from `rterm.shell()`. Called by main with
    /// the resolved shell program path/name at startup.
    pub fn set_shell_program(&self, name: impl Into<String>) {
        if let Ok(mut g) = self.shell_program.lock() {
            *g = name.into();
        }
    }

    /// Set the path Lua sees from `rterm.cache_dir()`.
    pub fn set_cache_dir(&self, path: impl Into<String>) {
        if let Ok(mut g) = self.cache_dir.lock() {
            *g = path.into();
        }
    }

    /// Replace the list returned by `rterm.builtin_events()`. The App
    /// pushes the curated event-name list at startup.
    pub fn set_builtin_events(&self, names: Vec<String>) {
        if let Ok(mut g) = self.builtin_events.lock() {
            *g = names;
        }
    }

    /// Replace the list returned by `rterm.builtin_actions()`. The App
    /// pushes the current `AppAction::ALL` names at startup.
    pub fn set_builtin_actions(&self, names: Vec<String>) {
        if let Ok(mut g) = self.builtin_actions.lock() {
            *g = names;
        }
    }

    /// Replace the `name -> label` map returned by
    /// `rterm.builtin_action_label(name)`. Each entry is one row from
    /// `AppAction::name_label_pairs()`. Empty input clears the map.
    pub fn set_builtin_action_labels(&self, pairs: Vec<(String, String)>) {
        if let Ok(mut g) = self.builtin_action_labels.lock() {
            g.clear();
            g.extend(pairs);
        }
    }


    /// Take the latest focus request from `rterm.focus_pane(...)`.
    pub fn take_pending_focus(&self) -> Option<(usize, usize)> {
        take_slot(&self.pending_focus)
    }

    /// Take the latest uid from `rterm.focus_pane_by_uid(uid)`. App
    /// resolves uid → live `(tab, pane)` at drain.
    pub fn take_pending_focus_by_uid(&self) -> Option<u64> {
        take_slot(&self.pending_focus_by_uid)
    }

    /// Take the latest tab index from `rterm.focus_tab(idx)`.
    pub fn take_pending_tab_focus(&self) -> Option<usize> {
        take_slot(&self.pending_tab_focus)
    }

    /// Take the latest clipboard text from `rterm.copy(text)`.
    pub fn take_pending_copy(&self) -> Option<String> {
        take_slot(&self.pending_copy)
    }


    /// Take the latest plugin-requested attention ping (then clear).
    pub fn take_pending_attention(&self) -> bool {
        self.pending_attention
            .lock()
            .map(|mut g| std::mem::replace(&mut *g, false))
            .unwrap_or(false)
    }

    /// Drain every pending plugin → app/renderer command from the
    /// unified channel. Callers `match` on `PluginCmd` variants to
    /// dispatch — there's no per-variant `drain_pending_X` anymore
    /// (those queues are being folded into this channel one at a
    /// time; matched arms grow as the migration progresses).
    ///
    /// Order across variants is preserved: a plugin that fires
    /// `RunAction("split")` immediately followed by `Notify("done")`
    /// will see them in that order at the renderer side.
    pub fn drain_pending_commands(&self) -> Vec<rterm_core::PluginCmd> {
        self.cmd_rx
            .lock()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default()
    }

    /// Drain queued addressed input from `rterm.send_to_pane(...)`. Each
    /// entry is `((tab_idx, pane_idx), bytes)` with 0-based indices.
    pub fn drain_pending_routed_input(&self) -> Vec<((usize, usize), Vec<u8>)> {
        self.pending_routed_input
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Drain queued `(uid, payload)` byte streams from
    /// `rterm.send_to_pane_by_uid`. App resolves uid → live `(tab, pane)`.
    pub fn drain_pending_routed_input_by_uid(&self) -> Vec<(u64, Vec<u8>)> {
        self.pending_routed_input_by_uid
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Take the most recent scrollback-limit override (if any), clearing it.
    pub fn take_pending_scrollback_limit(&self) -> Option<usize> {
        take_slot(&self.pending_scrollback_limit)
    }

    /// Publish a new scrollback limit (e.g. from the TOML watcher) so the
    /// App applies it on the next frame.
    pub fn set_scrollback_limit_override(&self, n: usize) {
        if let Ok(mut g) = self.pending_scrollback_limit.lock() {
            *g = Some(n);
        }
    }

    /// Most-recent `terminal.tab_silence_ms` override (if any), cleared.
    pub fn take_pending_tab_silence_ms(&self) -> Option<u64> {
        take_slot(&self.pending_tab_silence_ms)
    }

    /// Publish a new `tab_silence_ms` value (from TOML watcher or Lua) so
    /// the App applies it on the next frame.
    pub fn set_tab_silence_ms_override(&self, n: u64) {
        if let Ok(mut g) = self.pending_tab_silence_ms.lock() {
            *g = Some(n);
        }
    }

    /// Publish a new font size, reusing the same pending channel as
    /// `rterm.set_font_size(N)` so the config watcher can apply
    /// `font.size` updates without restart. Non-finite values are
    /// already filtered by the renderer's clamp.
    pub fn set_font_size_override(&self, size: f32) {
        if let Ok(mut g) = self.pending_font_size.lock() {
            *g = Some(size);
        }
    }

    /// Re-use the same pending channel as `rterm.set_opacity(N)`. Called
    /// by the config watcher on `config.toml` reload so a `window.opacity`
    /// edit takes effect live. Non-finite is dropped (the renderer's
    /// clamp would panic).
    pub fn set_opacity_override(&self, value: f32) {
        if !value.is_finite() {
            return;
        }
        if let Ok(mut g) = self.pending_opacity.lock() {
            *g = Some(value.clamp(0.0, 1.0));
        }
    }

    /// Pending `terminal.cursor_blink` override from the config watcher.
    pub fn take_pending_cursor_blink(&self) -> Option<bool> {
        take_slot(&self.pending_cursor_blink)
    }
    pub fn set_cursor_blink_override(&self, v: bool) {
        if let Ok(mut g) = self.pending_cursor_blink.lock() {
            *g = Some(v);
        }
    }
    pub fn take_pending_show_scrollbar(&self) -> Option<bool> {
        take_slot(&self.pending_show_scrollbar)
    }
    pub fn set_show_scrollbar_override(&self, v: bool) {
        if let Ok(mut g) = self.pending_show_scrollbar.lock() {
            *g = Some(v);
        }
    }
    pub fn take_pending_scroll_on_output(&self) -> Option<bool> {
        take_slot(&self.pending_scroll_on_output)
    }
    pub fn set_scroll_on_output_override(&self, v: bool) {
        if let Ok(mut g) = self.pending_scroll_on_output.lock() {
            *g = Some(v);
        }
    }
    pub fn take_pending_bell_visual(&self) -> Option<bool> {
        take_slot(&self.pending_bell_visual)
    }
    pub fn set_bell_visual_override(&self, v: bool) {
        if let Ok(mut g) = self.pending_bell_visual.lock() {
            *g = Some(v);
        }
    }
    pub fn take_pending_bell_urgent(&self) -> Option<bool> {
        take_slot(&self.pending_bell_urgent)
    }
    pub fn set_bell_urgent_override(&self, v: bool) {
        if let Ok(mut g) = self.pending_bell_urgent.lock() {
            *g = Some(v);
        }
    }
    /// Hot-reloadable `[font].family` override. Empty string =
    /// "auto-pick system monospace default".
    pub fn take_pending_font_family(&self) -> Option<String> {
        take_slot(&self.pending_font_family)
    }
    pub fn set_font_family_override(&self, name: String) {
        if let Ok(mut g) = self.pending_font_family.lock() {
            *g = Some(name);
        }
    }
    /// Hot-reloadable `[guake]` snapshot. `Some(None)` = disable;
    /// `Some(Some((enabled, position, height_pct, width_pct)))` =
    /// install. The outer Option is what the renderer drains —
    /// `Some` means "user changed this", `None` means "no change".
    #[allow(clippy::type_complexity)]
    pub fn take_pending_guake(
        &self,
    ) -> Option<Option<(bool, String, u8, u8)>> {
        take_slot(&self.pending_guake)
    }
    /// App-side override, normally driven from a config hot-reload.
    /// Pass `None` to deactivate guake mid-session (e.g. user flipped
    /// `[guake] enabled = false`).
    pub fn set_guake_override(
        &self,
        guake: Option<(bool, String, u8, u8)>,
    ) {
        if let Ok(mut g) = self.pending_guake.lock() {
            *g = Some(guake);
        }
    }

    /// Push a full config snapshot to the renderer in one call. Centralises
    /// the field list so adding a new hot-reloadable config knob touches
    /// THIS function plus the matching `take_pending_X` drain — instead of
    /// one update line in `rterm-app`'s reload watcher per field.
    ///
    /// Internally still calls the per-field `set_X_override` setters; their
    /// sub-mutexes are short-lived (single-write each), so an interleaved
    /// renderer drain may see a partial snapshot for a few microseconds.
    /// That's the same behaviour the old multi-call site produced — neither
    /// is atomic across all fields, and the renderer applies whatever it
    /// drains at the next frame.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn apply_config_snapshot(
        &self,
        scrollback: usize,
        tab_silence_ms: u64,
        cursor_blink: bool,
        show_scrollbar: bool,
        scroll_on_output: bool,
        bell_visual: bool,
        bell_urgent: bool,
        slow_command_ms: u64,
        guake: Option<(bool, String, u8, u8)>,
        font_size: f32,
        font_family: String,
        opacity: f32,
    ) {
        self.set_scrollback_limit_override(scrollback);
        self.set_tab_silence_ms_override(tab_silence_ms);
        self.set_cursor_blink_override(cursor_blink);
        self.set_show_scrollbar_override(show_scrollbar);
        self.set_scroll_on_output_override(scroll_on_output);
        self.set_bell_visual_override(bell_visual);
        self.set_bell_urgent_override(bell_urgent);
        self.set_slow_command_ms_override(slow_command_ms);
        self.set_guake_override(guake);
        self.set_font_size_override(font_size);
        self.set_font_family_override(font_family);
        self.set_opacity_override(opacity);
    }
    /// Drain queued per-pane bell-mute requests from
    /// `rterm.set_pane_bell_muted(tab, pane, muted)`. Indices are 0-based
    /// (the Lua API converts from its 1-based form). The App writes each
    /// entry to the matching pane's `bell_muted` atomic.
    pub fn drain_pending_pane_bell_mute(&self) -> Vec<(usize, usize, bool)> {
        self.pending_pane_bell_mute
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }
    /// Drain `(uid, muted)` toggles queued by
    /// `rterm.set_pane_bell_muted_by_uid`. App resolves uid → live
    /// (tab, pane) via the tab tree; entries pointing at vanished
    /// panes are silently dropped.
    pub fn drain_pending_pane_bell_mute_by_uid(&self) -> Vec<(u64, bool)> {
        self.pending_pane_bell_mute_by_uid
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }
    pub fn take_pending_slow_command_ms(&self) -> Option<u64> {
        take_slot(&self.pending_slow_command_ms)
    }
    pub fn set_slow_command_ms_override(&self, v: u64) {
        if let Ok(mut g) = self.pending_slow_command_ms.lock() {
            *g = Some(v);
        }
    }

    /// Drain queued tab-title overrides from `rterm.set_tab_title(...)`
    /// and `rterm.set_tab_title_by_index(...)`. Each entry is
    /// `(Option<tab_0based>, name)`. `None` means "active tab" (the App
    /// resolves it at apply time).
    pub fn drain_pending_tab_titles(&self) -> Vec<(Option<usize>, String)> {
        self.pending_tab_titles
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Take the latest `rterm.set_window_title` override (then clear).
    /// Outer `Some` = update was requested this frame; inner `None` clears
    /// any existing override, `Some(name)` sets it.
    pub fn take_pending_window_title(&self) -> Option<Option<String>> {
        take_slot(&self.pending_window_title)
    }

    /// Drain queued `(tab, pane, title)` overrides from `rterm.set_pane_title`.
    pub fn drain_pending_pane_titles(&self) -> Vec<(usize, usize, String)> {
        self.pending_pane_titles
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Drain queued `(uid, title)` overrides from
    /// `rterm.set_pane_title_by_uid`. App resolves uid → live (tab, pane).
    pub fn drain_pending_pane_titles_by_uid(&self) -> Vec<(u64, String)> {
        self.pending_pane_titles_by_uid
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Push a fresh terminal snapshot (cwd, title, size) for Lua queries.
    pub fn set_state(&self, s: TerminalState) {
        if let Ok(mut g) = self.state.lock() {
            *g = s;
        }
    }

    pub fn action_names(&self) -> Vec<String> {
        let map = self.actions.lock().unwrap();
        let mut names: Vec<String> = map.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn run_action(&self, name: &str) -> Result<()> {
        // Resolve to a Function with the lock briefly held, then drop
        // it before invoking. Mirrors `emit`'s deadlock-safety pattern:
        // if the Lua action body calls `rterm.register_action(...)` or
        // `rterm.unregister_action(...)` (both of which lock `actions`),
        // holding the lock during `f.call` would deadlock.
        let f: Function = {
            let map = self.actions.lock().unwrap();
            let Some(key) = map.get(name) else {
                return Ok(());
            };
            self.lua.registry_value(key)?
        };
        if let Err(e) = self.with_exec_budget(HANDLER_EXEC_BUDGET, || f.call::<Value>(())) {
            tracing::warn!(target: "rterm::plugin", "action '{name}' failed: {e}");
        }
        Ok(())
    }

    pub fn load_script(&self, path: &Path) -> Result<()> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("reading plugin {}", path.display()))?;
        let chunk = self.lua.load(&src).set_name(path.to_string_lossy());
        self.with_exec_budget(SCRIPT_EXEC_BUDGET, || chunk.exec())
            .with_context(|| format!("evaluating {}", path.display()))?;
        Ok(())
    }

    /// Parse-only check for a Lua source file — useful for `--check` to
    /// detect syntax errors without actually executing side effects (event
    /// handler registration, etc.).
    pub fn validate_script(path: &Path) -> Result<()> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let lua = Lua::new();
        // `into_function` compiles the chunk; we discard the result so
        // user globals / handlers stay untouched.
        lua.load(&src)
            .set_name(path.to_string_lossy())
            .into_function()
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(())
    }

    pub fn load_dir(&self, dir: &Path) -> Result<usize> {
        if !dir.exists() {
            return Ok(0);
        }
        // Sort plugin files by path so load order is deterministic
        // across runs. Without this, `read_dir`'s OS-defined order
        // varied between hot-reloads, which makes register-action
        // last-write-wins behave unpredictably for users who name
        // overlapping actions across plugins.
        let mut paths: Vec<_> = std::fs::read_dir(dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lua"))
            .collect();
        paths.sort();
        let mut count = 0;
        for path in paths {
            // Per-file recovery: one plugin with a syntax error must
            // not silently disable every alphabetically-later plugin.
            // With hot-reload this used to fire on every mid-edit save
            // of an early-sorting file. The failure is logged loudly;
            // the count reports successful loads only.
            match self.load_script(&path) {
                Ok(()) => count += 1,
                Err(e) => {
                    tracing::warn!(
                        target: "rterm::plugin",
                        "skipping plugin {}: {e:#}",
                        path.display(),
                    );
                }
            }
        }
        Ok(count)
    }

    /// Clear every registered handler, palette action AND output-match
    /// rule. Used before re-running init.lua and plugins on hot-reload
    /// so registrations don't compound. Match rules are included —
    /// re-exec re-registers the ones still in the source, exactly like
    /// handlers; leaving them made a DELETED `rterm.add_match` rule
    /// keep firing (and keep holding one of the 64 slots) until
    /// restart.
    pub fn reset_handlers(&self) {
        if let Ok(mut map) = self.handlers.lock() {
            map.clear();
        }
        if let Ok(mut map) = self.actions.lock() {
            map.clear();
        }
        if let Ok(mut rules) = self.match_rules.lock() {
            rules.clear();
        }
    }

    /// Dispatch a named event to all registered handlers. The payload is passed
    /// as a single Lua argument; widen this once we have real event types.
    pub fn emit(&self, event: &str, payload: impl mlua::IntoLua + Clone) -> Result<()> {
        // Resolve handler Functions inside a brief lock scope, then
        // release before invoking them. This avoids a deadlock when a
        // handler calls back into `rterm.on(...)` / `rterm.off(...)`
        // (both of which lock the same Mutex), and lets the Lua side
        // mutate the handler list during a callback safely.
        let functions: Vec<Function> = {
            let map = self.handlers.lock().unwrap();
            let Some(keys) = map.get(event) else { return Ok(()) };
            if keys.is_empty() {
                return Ok(());
            }
            // `registry_value` is a cheap lookup that yields a new
            // Function ref — cloning the Vec lets the handler list
            // mutate freely while we iterate.
            keys.iter()
                .map(|k| self.lua.registry_value::<Function>(k))
                .collect::<mlua::Result<_>>()?
        };
        for f in functions {
            // Budget armed PER HANDLER so one slow handler can't eat
            // the others' time slice.
            let res =
                self.with_exec_budget(HANDLER_EXEC_BUDGET, || f.call::<Value>(payload.clone()));
            if let Err(e) = res {
                tracing::warn!(target: "rterm::plugin", "handler for {event} failed: {e}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_budget_aborts_runaway_lua_and_host_survives() {
        // The instruction hook must turn `while true do end` into a
        // Lua error once the armed deadline passes — without it a
        // buggy handler froze the render thread forever. Uses a tiny
        // budget directly so the test doesn't sit through the real
        // 2-second handler budget.
        let host = PluginHost::new().expect("host inits");
        let res = host.with_exec_budget(Duration::from_millis(50), || {
            host.lua.load("while true do end").exec()
        });
        let err = res.expect_err("infinite loop must be aborted");
        assert!(
            err.to_string().contains("execution budget"),
            "unexpected error: {err}"
        );
        // The watchdog disarmed and the VM is healthy: ordinary code
        // (including loops that finish) runs fine afterwards.
        host.lua
            .load("local s = 0; for i = 1, 100000 do s = s + i end; _G.sum = s")
            .exec()
            .expect("normal code still runs after an abort");
        let sum: u64 = host.lua.globals().get("sum").unwrap();
        assert_eq!(sum, 5_000_050_000);
    }

    #[test]
    fn routed_input_queue_is_capped() {
        // A single Lua chunk pushing far past the cap must not grow
        // the queue without bound; overflow is rejected (newest
        // dropped) so the queued stream stays contiguous.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"for i = 1, 5000 do rterm.send_to_pane(1, 1, "x") end"#)
            .exec()
            .unwrap();
        let drained = host.drain_pending_routed_input();
        assert_eq!(drained.len(), PLUGIN_QUEUE_CAP);
        // After a drain there is room again.
        host.lua
            .load(r#"rterm.send_to_pane(1, 1, "y")"#)
            .exec()
            .unwrap();
        assert_eq!(host.drain_pending_routed_input().len(), 1);
    }

    #[test]
    fn reset_handlers_clears_match_rules_too() {
        // Hot-reload re-executes every script; rules still in the
        // source re-register. Leaving old ones in place made a rule
        // DELETED from the source keep firing (and keep holding one
        // of the 64 slots) until restart.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.add_match("ghost", "needle")"#)
            .exec()
            .unwrap();
        assert_eq!(host.match_rule_names(), vec!["ghost".to_string()]);
        host.reset_handlers();
        assert!(
            host.match_rule_names().is_empty(),
            "match rules must not survive a hot-reload reset"
        );
    }

    #[test]
    fn load_dir_recovers_past_a_broken_plugin() {
        // `aaa.lua` has a syntax error; `bbb.lua` must still load. The
        // old `?` propagation aborted the loop on the first failure,
        // silently disabling every later plugin after a mid-edit save.
        let dir = std::env::temp_dir().join(format!(
            "rterm-test-load-dir-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aaa.lua"), "this is not lua ((").unwrap();
        std::fs::write(dir.join("bbb.lua"), "_G.loaded_ok = true").unwrap();

        let host = PluginHost::new().expect("host inits");
        let count = host.load_dir(&dir).expect("dir read works");
        assert_eq!(count, 1, "only the good plugin counts as loaded");
        let ok: bool = host.lua.globals().get("loaded_ok").unwrap();
        assert!(ok, "the good plugin actually executed");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rterm_run_action_can_re_register_during_callback() {
        // Same deadlock pattern as emit, but on the action map. An
        // action body calling `rterm.register_action` / `unregister_action`
        // used to deadlock since `run_action` held the actions Mutex
        // through the Lua call. Verify the snapshot-and-release fix.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                _G.ran = 0
                rterm.register_action("self_rebind", function()
                    _G.ran = _G.ran + 1
                    rterm.register_action("other", function() _G.ran = _G.ran + 1 end)
                end)
            "#,
            )
            .exec()
            .unwrap();
        host.run_action("self_rebind").expect("must not deadlock");
        let ran: u64 = host.lua.globals().get("ran").unwrap();
        assert_eq!(ran, 1);
        // The new action is callable on the next run_action.
        host.run_action("other").unwrap();
        let ran: u64 = host.lua.globals().get("ran").unwrap();
        assert_eq!(ran, 2);
    }

    #[test]
    fn rterm_emit_handler_can_reregister_during_callback() {
        // A real-world plugin pattern: a handler registers another
        // handler from inside the callback (e.g. one-shot listener
        // re-arming itself, or a setup-phase handler that swaps to a
        // steady-state handler). The old emit implementation held the
        // handler-list Mutex during `f.call()`, deadlocking the moment
        // the Lua code called `rterm.on(...)` again.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                _G.reentry_count = 0
                rterm.on("ping", function(_)
                    _G.reentry_count = _G.reentry_count + 1
                    -- This used to deadlock; must now succeed.
                    rterm.on("ping", function(_) _G.reentry_count = _G.reentry_count + 1 end)
                end)
            "#,
            )
            .exec()
            .unwrap();
        // First emit: 1 handler fires (the seed), it registers a 2nd.
        host.emit("ping", "x".to_string()).expect("emit must not deadlock");
        let after_first: u64 = host
            .lua
            .globals()
            .get::<u64>("reentry_count")
            .unwrap();
        assert_eq!(after_first, 1);
        // Second emit: 2 handlers fire (the seed runs again + the
        // newly-registered one).
        host.emit("ping", "x".to_string()).unwrap();
        let after_second: u64 = host
            .lua
            .globals()
            .get::<u64>("reentry_count")
            .unwrap();
        assert!(
            after_second >= 3,
            "expected at least 3 cumulative invocations, got {after_second}",
        );
    }

    #[test]
    fn rterm_on_registers_handler_and_emit_calls_it() {
        // Smoke test for the core Lua surface: a plugin can register a
        // handler via `rterm.on(event, fn)`, and `host.emit(event, ...)`
        // dispatches to it. Without this lifeline test, a refactor that
        // breaks the registration plumbing would surface only at runtime
        // (no handler fires → plugin appears silently broken).
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                _G.last_payload = nil
                rterm.on("ping", function(p) _G.last_payload = p end)
            "#,
            )
            .exec()
            .expect("inline plugin loads");
        // Handler count via the public `handler_count` Lua API mirrors
        // the registry.
        let n: u64 = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("handler_count")
            .unwrap()
            .call("ping")
            .unwrap();
        assert_eq!(n, 1);
        host.emit("ping", "hello".to_string()).expect("emit ok");
        let got: String = host
            .lua
            .globals()
            .get("last_payload")
            .expect("payload set");
        assert_eq!(got, "hello");
    }

    #[test]
    fn handler_count_returns_zero_for_unknown_event() {
        let host = PluginHost::new().expect("host inits");
        let n: u64 = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("handler_count")
            .unwrap()
            .call("never-registered")
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn rterm_off_clears_all_handlers_for_event() {
        // Plugins can detach via `rterm.off(event)` — returns the count
        // of removed handlers, then a subsequent emit must not call them.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                _G.fired = 0
                rterm.on("ev", function() _G.fired = _G.fired + 1 end)
                rterm.on("ev", function() _G.fired = _G.fired + 1 end)
            "#,
            )
            .exec()
            .unwrap();
        host.emit("ev", mlua::Value::Nil).unwrap();
        let fired: i64 = host.lua.globals().get("fired").unwrap();
        assert_eq!(fired, 2, "both handlers should have fired");

        // Detach.
        let removed: u64 = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("off")
            .unwrap()
            .call("ev")
            .unwrap();
        assert_eq!(removed, 2);
        // Subsequent emit must not invoke anything.
        host.emit("ev", mlua::Value::Nil).unwrap();
        let fired: i64 = host.lua.globals().get("fired").unwrap();
        assert_eq!(fired, 2, "handlers should stay off after rterm.off");
    }

    #[test]
    fn send_input_queues_bytes_for_focused_pane() {
        // `rterm.send_input("foo")` should buffer bytes that the App
        // drains and writes to the focused pane's PTY. Plugins use this
        // for macros / paste-like flows; if the queue isn't actually
        // wired up, the bytes silently vanish.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.send_input("hi"); rterm.send_input("there")"#)
            .exec()
            .unwrap();
        let queued: Vec<Vec<u8>> = host
            .drain_pending_commands()
            .into_iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::SendInput(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(queued, vec![b"hi".to_vec(), b"there".to_vec()]);
        // Second drain returns empty — items are consumed, not cloned.
        assert!(host.drain_pending_commands().is_empty());
    }

    #[test]
    fn send_to_pane_routes_to_one_based_indices() {
        // `rterm.send_to_pane(tab, pane, payload)` — Lua uses 1-based
        // indices to match `list_panes()`; the host translates to the
        // 0-based form the App actually consumes.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.send_to_pane(2, 3, "payload")"#)
            .exec()
            .unwrap();
        let routed = host.drain_pending_routed_input();
        assert_eq!(routed.len(), 1);
        let ((tab, pane), bytes) = &routed[0];
        assert_eq!(*tab, 1);
        assert_eq!(*pane, 2);
        assert_eq!(bytes, &b"payload".to_vec());
    }

    #[test]
    fn rterm_copy_paste_notify_queue_correctly() {
        // These three Lua helpers all funnel into per-call queues that
        // the App drains each frame. They're the back-half of the input
        // /output bridge, so a regression that misnames a global would
        // silently drop user actions (`paste` would mute, `notify`
        // would never reach the OS).
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.copy("hello world")
                rterm.paste("incoming")
                rterm.notify("ping")
            "#,
            )
            .exec()
            .unwrap();
        assert_eq!(host.take_pending_copy().as_deref(), Some("hello world"));
        assert!(host.take_pending_copy().is_none(), "copy is one-shot");
        let cmds = host.drain_pending_commands();
        let pastes: Vec<Vec<u8>> = cmds
            .iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::Paste(b) => Some(b.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(pastes, vec![b"incoming".to_vec()]);
        let notifications: Vec<String> = cmds
            .into_iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::Notify(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(notifications, vec!["ping".to_string()]);
    }

    #[test]
    fn rterm_run_action_queues_canonical_name() {
        // Plugins invoke built-in actions via `rterm.run_action(name)`.
        // The host queues the name, then the App's tick translates it
        // to an AppAction. Failure mode would be a plugin pressing
        // "next_tab" and nothing happening — easy to miss in manual
        // testing.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.run_action("next_tab"); rterm.run_action("close_pane")"#)
            .exec()
            .unwrap();
        let queued: Vec<_> = host
            .drain_pending_commands()
            .into_iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::RunAction(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(queued, vec!["next_tab".to_string(), "close_pane".to_string()]);
    }

    #[test]
    fn state_propagates_to_lua_getters() {
        // set_state is what the App calls once per frame to publish the
        // current snapshot. Lua-side getters read from the same Arc, so
        // a regression in the wiring would mean plugins see a stale or
        // default snapshot forever (think: a `font_size`-display plugin
        // stuck on 13 after the user bumped it to 16).
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            cwd: Some("/tmp/proj".to_string()),
            title: Some("zsh - rterm".to_string()),
            cols: 100,
            rows: 32,
            panes: vec![],
            tabs: vec![],
            grid_text: String::new(),
            font_size: 14.5,
            font_family: "JetBrains Mono".to_string(),
            cell_width: 8.4,
            line_height: 18.0,
            tab_silence_ms: 3000,
            slow_command_ms: 7500,
            scroll_on_output: true,
            show_scrollbar: false,
            bell_visual: true,
            bell_urgent: false,
            cursor_blink: true,
            named_palette: {
                let mut p = [[0u8; 3]; 16];
                // Distinct per-slot pattern so the test catches an
                // index swap or off-by-one in the snapshot pipeline.
                for (i, c) in p.iter_mut().enumerate() {
                    *c = [(i as u8) * 16, (i as u8) * 8, i as u8];
                }
                p
            },
            dragging_tab: None,
            scrollback_limit: 20000,
            selection_text: Some("hello".to_string()),
            opacity: 0.95,
            window_focused: true,
            last_exit_code: Some(0),
            prompt_mark_lines: vec![10, 20],
            command_mark_lines: vec![15],
            theme_fg: [220, 220, 220],
            theme_bg: [10, 12, 18],
            theme_cursor: [255, 204, 102],
            scrollback_text: String::new(),
            search_active: true,
            search_query: "build".to_string(),
            search_match_index: 2,
            search_match_total: 5,
            search_regex_mode: true,
            active_theme: "dracula".to_string(),
        };
        host.set_state(snapshot);

        let rterm: Table = host.lua.globals().get("rterm").unwrap();
        let cwd: Option<String> = rterm.get::<Function>("cwd").unwrap().call(()).unwrap();
        assert_eq!(cwd.as_deref(), Some("/tmp/proj"));
        let font_size: f32 = rterm.get::<Function>("font_size").unwrap().call(()).unwrap();
        assert!((font_size - 14.5).abs() < 1e-3);
        let font_family: String = rterm.get::<Function>("font_family").unwrap().call(()).unwrap();
        assert_eq!(font_family, "JetBrains Mono");
        let sel: Option<String> = rterm.get::<Function>("selection").unwrap().call(()).unwrap();
        assert_eq!(sel.as_deref(), Some("hello"));
        let silence_ms: u64 = rterm
            .get::<Function>("tab_silence_ms")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(silence_ms, 3000);
        let slow_ms: u64 = rterm
            .get::<Function>("slow_command_ms")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(slow_ms, 7500);
        // Bool config getters — each must reflect the pushed value
        // rather than the default. Mixed true/false above so a
        // wire-swap (e.g. read bell_visual returning bell_urgent)
        // shows up here.
        for (name, expected) in [
            ("scroll_on_output", true),
            ("show_scrollbar", false),
            ("bell_visual", true),
            ("bell_urgent", false),
            ("cursor_blink", true),
        ] {
            let v: bool = rterm
                .get::<Function>(name)
                .unwrap()
                .call(())
                .unwrap();
            assert_eq!(v, expected, "{name} should reflect pushed value");
        }
        // named_palette returns a 16-element array of {r,g,b} triples.
        // The test snapshot uses a per-slot distinct pattern so an
        // index swap would surface as a mismatched RGB triple.
        let arr: mlua::Table = rterm
            .get::<Function>("named_palette")
            .unwrap()
            .call(())
            .unwrap();
        for i in 0..16u8 {
            let entry: mlua::Table = arr.get(i as i64 + 1).unwrap();
            let r: u8 = entry.get(1).unwrap();
            let g: u8 = entry.get(2).unwrap();
            let b: u8 = entry.get(3).unwrap();
            assert_eq!(
                (r, g, b),
                (i * 16, i * 8, i),
                "named_palette slot {i} should match pushed pattern",
            );
        }
        let exit: Option<i32> = rterm
            .get::<Function>("last_exit_code")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(exit, Some(0));
        // Search-state surface — all four getters should reflect the
        // pushed values rather than their defaults.
        let active: bool = rterm
            .get::<Function>("is_search_active")
            .unwrap()
            .call(())
            .unwrap();
        assert!(active);
        let q: String = rterm
            .get::<Function>("search_query")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(q, "build");
        let regex: bool = rterm
            .get::<Function>("search_regex_mode")
            .unwrap()
            .call(())
            .unwrap();
        assert!(regex);
        let matches: Table = rterm
            .get::<Function>("search_matches")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(matches.get::<u32>(1).unwrap(), 2);
        assert_eq!(matches.get::<u32>(2).unwrap(), 5);
    }

    #[test]
    fn rterm_new_tab_and_split_route_through_queues() {
        // `rterm.new_tab(cwd?)` and `rterm.split("h"|"v", cwd?)` are how
        // plugins programmatically reshape the layout. Each call must
        // append exactly one entry to its queue — duplicates would cause
        // the App to spawn twice per call.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.new_tab("/tmp/a")
                rterm.new_tab()
                rterm.split("h", "/tmp/b")
                rterm.split("v")
            "#,
            )
            .exec()
            .unwrap();
        let cmds = host.drain_pending_commands();
        let new_tabs: Vec<Option<String>> = cmds
            .iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::NewTab(cwd) => Some(cwd.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(new_tabs.len(), 2);
        assert_eq!(new_tabs[0].as_deref(), Some("/tmp/a"));
        assert_eq!(new_tabs[1], None);
        let splits: Vec<(String, Option<String>)> = cmds
            .into_iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::Split(d, cwd) => Some((d, cwd)),
                _ => None,
            })
            .collect();
        assert_eq!(splits.len(), 2);
        assert_eq!(splits[0].0, "h");
        assert_eq!(splits[0].1.as_deref(), Some("/tmp/b"));
        assert_eq!(splits[1].0, "v");
        assert_eq!(splits[1].1, None);
    }

    #[test]
    fn builtin_events_and_actions_round_trip() {
        // The App installs the surface lists at startup. Plugins use
        // `rterm.builtin_events()` to e.g. attach a logger to every
        // event, and `rterm.builtin_actions()` to populate a command
        // palette. Verify both setters round-trip via their Lua getters.
        let host = PluginHost::new().expect("host inits");
        host.set_builtin_events(vec!["alpha".into(), "beta".into()]);
        host.set_builtin_actions(vec!["one".into(), "two".into(), "three".into()]);

        let rterm: Table = host.lua.globals().get("rterm").unwrap();
        let events: Vec<String> = rterm
            .get::<Function>("builtin_events")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(events, vec!["alpha".to_string(), "beta".to_string()]);
        let actions: Vec<String> = rterm
            .get::<Function>("builtin_actions")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(
            actions,
            vec!["one".to_string(), "two".to_string(), "three".to_string()],
        );

        // Replacing the lists is what hot-reload does — old contents
        // must be fully gone, no merging.
        host.set_builtin_events(vec!["gamma".into()]);
        let events: Vec<String> = rterm
            .get::<Function>("builtin_events")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(events, vec!["gamma".to_string()]);
    }

    #[test]
    fn set_opacity_override_shares_channel_with_lua_setter() {
        // Watcher and Lua both drive runtime opacity through the same
        // pending slot. Out-of-range from either source is clamped at the
        // boundary so the renderer sees only valid input.
        let host = PluginHost::new().expect("host inits");
        host.set_opacity_override(0.75);
        host.lua.load(r#"rterm.set_opacity(0.4)"#).exec().unwrap();
        // Lua's call landed after the override → Lua value wins.
        assert_eq!(host.take_pending_opacity(), Some(0.4));
        assert_eq!(host.take_pending_opacity(), None);

        // Out-of-range from the watcher is also clamped.
        host.set_opacity_override(-1.0);
        assert_eq!(host.take_pending_opacity(), Some(0.0));
        host.set_opacity_override(f32::NAN);
        assert_eq!(host.take_pending_opacity(), None);
    }

    #[test]
    fn set_font_size_override_shares_channel_with_lua_setter() {
        // The watcher's `set_font_size_override(N)` and the Lua-side
        // `rterm.set_font_size(N)` must hit the same pending slot so
        // either source can drive a runtime change. A second push from
        // either side clobbers the first (last-write-wins is the right
        // policy for a setter).
        let host = PluginHost::new().expect("host inits");
        host.set_font_size_override(14.0);
        host.lua.load(r#"rterm.set_font_size(18)"#).exec().unwrap();
        // Lua's call landed after the override → Lua value wins.
        assert_eq!(host.take_pending_font_size(), Some(18.0));
        assert_eq!(host.take_pending_font_size(), None);
    }

    #[test]
    fn rterm_terminal_text_returns_focused_grid() {
        // The snapshot's `grid_text` carries the focused pane's visible
        // text. Plugins use this for screen-scraping (e.g. "what's the
        // current shell prompt?"). Round-trip through the Lua getter.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            grid_text: "line 1\nline 2\nline 3".to_string(),
            ..TerminalState::default()
        });
        let text: String = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("terminal_text")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(text, "line 1\nline 2\nline 3");
    }

    #[test]
    fn rterm_snapshot_includes_focus_and_uid_fields() {
        // Pin the newly-added snapshot fields (window_focused,
        // active_tab, active_pane_uid). A focused tab + focused pane
        // in the input should surface their 1-based / uid values; no
        // focus → 0 sentinels.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            window_focused: true,
            tabs: vec![
                TabInfo { idx: 0, focused: false, pane_count: 2, ..TabInfo::default() },
                TabInfo { idx: 1, focused: true, pane_count: 3, ..TabInfo::default() },
            ],
            panes: vec![
                PaneInfo { tab: 1, pane: 0, uid: 555, focused: true, ..PaneInfo::default() },
            ],
            ..TerminalState::default()
        });
        let active_tab: u32 = host
            .lua
            .load(r#"return rterm.snapshot().active_tab"#)
            .eval()
            .unwrap();
        assert_eq!(active_tab, 2, "1-based focused tab index");
        let active_pane: u32 = host
            .lua
            .load(r#"return rterm.snapshot().active_pane"#)
            .eval()
            .unwrap();
        assert_eq!(active_pane, 1, "1-based focused pane index");
        let uid: u64 = host
            .lua
            .load(r#"return rterm.snapshot().active_pane_uid"#)
            .eval()
            .unwrap();
        assert_eq!(uid, 555);
        // alt_screen reflects the focused pane's state (default = primary).
        let alt: bool = host
            .lua
            .load(r#"return rterm.snapshot().alt_screen"#)
            .eval()
            .unwrap();
        assert!(!alt);
        // Focused tab has pane_count = 3 set above.
        let n: usize = host
            .lua
            .load(r#"return rterm.snapshot().active_tab_pane_count"#)
            .eval()
            .unwrap();
        assert_eq!(n, 3);
        let window_focused: bool = host
            .lua
            .load(r#"return rterm.snapshot().window_focused"#)
            .eval()
            .unwrap();
        assert!(window_focused);
        // No-focus path: sentinels of 0.
        host.set_state(TerminalState::default());
        let active_tab: u32 = host
            .lua
            .load(r#"return rterm.snapshot().active_tab"#)
            .eval()
            .unwrap();
        assert_eq!(active_tab, 0);
        let uid: u64 = host
            .lua
            .load(r#"return rterm.snapshot().active_pane_uid"#)
            .eval()
            .unwrap();
        assert_eq!(uid, 0);
    }

    #[test]
    fn rterm_now_ms_returns_unix_epoch_milliseconds() {
        // Sanity check that `now_ms()` returns a plausibly-current
        // Unix-epoch millisecond count and that two successive calls
        // are non-decreasing. We avoid pinning an exact value (CI
        // clocks vary) but compare against Rust's own SystemTime to
        // catch a major drift like ms/sec mix-up.
        let host = PluginHost::new().expect("host inits");
        let lua_ms: u64 = host
            .lua
            .load(r#"return rterm.now_ms()"#)
            .eval()
            .unwrap();
        let rust_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // The two timestamps should be within 1 second of each other
        // (most test runs return in <10 ms; the slack covers CI box jitter).
        let diff = lua_ms.abs_diff(rust_ms);
        assert!(
            diff < 1_000,
            "now_ms diverged from SystemTime: lua={lua_ms} rust={rust_ms} diff={diff}",
        );
        // And it's monotonic across the same call site.
        let second: u64 = host
            .lua
            .load(r#"return rterm.now_ms()"#)
            .eval()
            .unwrap();
        assert!(second >= lua_ms);
    }

    #[test]
    fn rterm_session_uptime_ms_grows_monotonically() {
        // Trivial sanity: a fresh host reports a tiny positive uptime,
        // and a subsequent call returns something at least as large.
        // (Wall-clock asserts are flaky in CI; a monotonic comparison
        // and an upper bound that any reasonable test environment will
        // clear is the right balance.)
        let host = PluginHost::new().expect("host inits");
        let first: u64 = host
            .lua
            .load(r#"return rterm.session_uptime_ms()"#)
            .eval()
            .unwrap();
        // Plugin construction takes some ms; first call ≤ 30s is a safe
        // upper bound even on a heavily loaded test box.
        assert!(first < 30_000, "first uptime suspiciously large: {first}");
        std::thread::sleep(std::time::Duration::from_millis(15));
        let later: u64 = host
            .lua
            .load(r#"return rterm.session_uptime_ms()"#)
            .eval()
            .unwrap();
        assert!(
            later >= first,
            "uptime went backwards: first={first} later={later}",
        );
    }

    #[test]
    fn rterm_builtin_action_labels_returns_full_map_in_one_call() {
        // Bulk variant — populates an action palette without N round-
        // trips. Empty map is exposed as an empty Lua table (not nil)
        // so `for k, v in pairs(...)` iteration just produces no
        // entries instead of crashing.
        let host = PluginHost::new().expect("host inits");
        host.set_builtin_action_labels(vec![
            ("new_tab".to_string(), "New tab".to_string()),
            ("close_pane".to_string(), "Close pane".to_string()),
            ("opacity_reset".to_string(), "Opacity: reset".to_string()),
        ]);
        let count: i32 = host
            .lua
            .load(
                r#"
                local t = rterm.builtin_action_labels()
                local n = 0
                for _, _ in pairs(t) do n = n + 1 end
                return n
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(count, 3);
        let label: String = host
            .lua
            .load(r#"return rterm.builtin_action_labels().opacity_reset"#)
            .eval()
            .unwrap();
        assert_eq!(label, "Opacity: reset");
        // Empty map after clear → empty Lua table.
        host.set_builtin_action_labels(Vec::new());
        let count: i32 = host
            .lua
            .load(
                r#"
                local t = rterm.builtin_action_labels()
                local n = 0
                for _, _ in pairs(t) do n = n + 1 end
                return n
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn rterm_builtin_action_label_returns_pushed_label_or_nil() {
        // The App pushes name→label pairs at startup; the Lua getter
        // surfaces the label for a canonical name or nil for anything
        // unknown (plugin actions, typos). Empty input clears the map.
        let host = PluginHost::new().expect("host inits");
        host.set_builtin_action_labels(vec![
            ("new_tab".to_string(), "New tab".to_string()),
            ("close_pane".to_string(), "Close pane".to_string()),
        ]);
        let hit: Option<String> = host
            .lua
            .load(r#"return rterm.builtin_action_label("new_tab")"#)
            .eval()
            .unwrap();
        assert_eq!(hit.as_deref(), Some("New tab"));
        let miss: Option<String> = host
            .lua
            .load(r#"return rterm.builtin_action_label("nonexistent")"#)
            .eval()
            .unwrap();
        assert!(miss.is_none());
        // Empty input clears.
        host.set_builtin_action_labels(Vec::new());
        let after_clear: Option<String> = host
            .lua
            .load(r#"return rterm.builtin_action_label("new_tab")"#)
            .eval()
            .unwrap();
        assert!(after_clear.is_none());
    }

    #[test]
    fn rterm_active_pane_uid_returns_focused_pane_uid_or_zero() {
        // With a focused pane in the snapshot, the getter returns its
        // uid. With no focused pane, it returns 0 — the documented
        // "no pane" sentinel (monotonic counter starts at 1).
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 111,
                    focused: false,
                    ..PaneInfo::default()
                },
                PaneInfo {
                    tab: 0,
                    pane: 1,
                    uid: 222,
                    focused: true,
                    ..PaneInfo::default()
                },
            ],
            ..TerminalState::default()
        });
        let uid: u64 = host
            .lua
            .load(r#"return rterm.active_pane_uid()"#)
            .eval()
            .unwrap();
        assert_eq!(uid, 222);

        // No focused pane → 0 sentinel.
        host.set_state(TerminalState {
            panes: vec![PaneInfo {
                tab: 0,
                pane: 0,
                uid: 111,
                focused: false,
                ..PaneInfo::default()
            }],
            ..TerminalState::default()
        });
        let uid: u64 = host
            .lua
            .load(r#"return rterm.active_pane_uid()"#)
            .eval()
            .unwrap();
        assert_eq!(uid, 0);
    }

    #[test]
    fn rterm_terminal_text_by_uid_resolves_to_pane_text() {
        // Pure snapshot read: with three panes carrying distinct uids,
        // `terminal_text_by_uid` returns the matching pane's text or nil
        // when no live pane carries the uid.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 11,
                    text: "alpha".to_string(),
                    ..PaneInfo::default()
                },
                PaneInfo {
                    tab: 0,
                    pane: 1,
                    uid: 22,
                    text: "beta".to_string(),
                    ..PaneInfo::default()
                },
            ],
            ..TerminalState::default()
        });
        let getter: Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("terminal_text_by_uid")
            .unwrap();
        let hit: Option<String> = getter.call::<Option<String>>(22u64).unwrap();
        assert_eq!(hit.as_deref(), Some("beta"));
        // Unknown uid → nil.
        let miss: Option<String> = getter.call::<Option<String>>(999u64).unwrap();
        assert!(miss.is_none());
    }

    #[test]
    fn rterm_copy_pane_by_uid_pushes_text_to_pending_copy() {
        // `copy_pane_by_uid` resolves uid → snapshot text and pushes it
        // into the same pending_copy slot the regular `rterm.copy(text)`
        // uses. Returns false when the uid misses or the pane text is
        // empty (no-op so plugins don't accidentally wipe the clipboard).
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            panes: vec![PaneInfo {
                tab: 0,
                pane: 0,
                uid: 42,
                text: "build output".to_string(),
                ..PaneInfo::default()
            }],
            ..TerminalState::default()
        });
        let ok: bool = host
            .lua
            .load(r#"return rterm.copy_pane_by_uid(42)"#)
            .eval()
            .unwrap();
        assert!(ok);
        assert_eq!(host.take_pending_copy().as_deref(), Some("build output"));
        // Unknown uid → false, nothing queued.
        let miss: bool = host
            .lua
            .load(r#"return rterm.copy_pane_by_uid(999)"#)
            .eval()
            .unwrap();
        assert!(!miss);
        assert!(host.take_pending_copy().is_none());
    }

    #[test]
    fn rterm_send_to_pane_by_uid_queues_bytes_with_uid() {
        // FIFO queue of `(uid, bytes)` pairs. Payload is UTF-8 from Lua;
        // we store the raw bytes so escape sequences round-trip
        // verbatim. App walks live panes per uid and writes.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.send_to_pane_by_uid(101, "ls\r")
                rterm.send_to_pane_by_uid(202, "pwd\r")
            "#,
            )
            .exec()
            .unwrap();
        let q = host.drain_pending_routed_input_by_uid();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].0, 101);
        assert_eq!(q[0].1, b"ls\r");
        assert_eq!(q[1].0, 202);
        assert_eq!(q[1].1, b"pwd\r");
        assert!(host.drain_pending_routed_input_by_uid().is_empty());
    }

    #[test]
    fn rterm_kill_pane_by_uid_appends_to_queue() {
        // FIFO queue (unlike single-slot focus). Each call must append
        // one entry — App walks live panes per uid and flips `alive` on
        // the match.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.kill_pane_by_uid(11)
                rterm.kill_pane_by_uid(22)
                rterm.kill_pane_by_uid(33)
            "#,
            )
            .exec()
            .unwrap();
        let q: Vec<u64> = host
            .drain_pending_commands()
            .into_iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::KillPaneByUid(u) => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(q, vec![11, 22, 33]);
        // Drain is destructive; second call is empty.
        assert!(host.drain_pending_commands().is_empty());
    }

    #[test]
    fn rterm_focus_pane_by_uid_queues_uid_for_app_resolution() {
        // The uid-addressed focus call publishes to its own pending slot
        // (separate from focus_pane(tab, pane)). App walks live panes
        // to resolve the uid at drain time — at the plugin layer we
        // verify the slot receives the value and is last-write-wins.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.focus_pane_by_uid(7)
                rterm.focus_pane_by_uid(42)
            "#,
            )
            .exec()
            .unwrap();
        // Last call wins (single-slot setter).
        assert_eq!(host.take_pending_focus_by_uid(), Some(42));
        assert_eq!(host.take_pending_focus_by_uid(), None);
    }

    #[test]
    fn rterm_set_pane_title_by_uid_queues_through_dedicated_channel() {
        // The uid-addressed setter publishes to a separate pending queue
        // from `set_pane_title(tab, pane, ...)`. The App walks live panes
        // to resolve the uid — but at the plugin layer we just verify
        // the queue gets the right `(uid, title)` pairs.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.set_pane_title_by_uid(42, "build")
                rterm.set_pane_title_by_uid(99, "logs")
                rterm.set_pane_title_by_uid(42, "")  -- clear
            "#,
            )
            .exec()
            .unwrap();
        let q = host.drain_pending_pane_titles_by_uid();
        assert_eq!(q.len(), 3);
        assert_eq!(q[0], (42, "build".to_string()));
        assert_eq!(q[1], (99, "logs".to_string()));
        assert_eq!(q[2], (42, String::new()));
        // Drain is destructive; second call returns nothing.
        assert!(host.drain_pending_pane_titles_by_uid().is_empty());
    }

    #[test]
    fn rterm_list_panes_exposes_uid_field() {
        // PaneInfo.uid round-trips through the Lua getter so plugins
        // can capture the stable identifier alongside the index pair.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            panes: vec![PaneInfo {
                tab: 0,
                pane: 0,
                uid: 12345,
                ..PaneInfo::default()
            }],
            ..TerminalState::default()
        });
        let uid: u64 = host
            .lua
            .load(r#"return rterm.list_panes()[1].uid"#)
            .eval()
            .unwrap();
        assert_eq!(uid, 12345);
    }

    #[test]
    fn rterm_scrollback_text_by_uid_returns_tail_or_empty() {
        // Mirror of `scrollback_text_of` but addressed by uid. Two
        // panes with distinct uids — query by uid → matching tail;
        // unknown uid → empty.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 100,
                    scrollback_tail: "x1\nx2\nx3".to_string(),
                    ..PaneInfo::default()
                },
                PaneInfo {
                    tab: 0,
                    pane: 1,
                    uid: 200,
                    scrollback_tail: "y1\ny2\ny3\ny4".to_string(),
                    ..PaneInfo::default()
                },
            ],
            ..TerminalState::default()
        });
        let f: Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("scrollback_text_by_uid")
            .unwrap();
        // Hit by uid 200, tail-slice to 2 lines.
        let s: String = f.call::<String>((200u64, 2u32)).unwrap();
        assert_eq!(s, "y3\ny4");
        // Hit by uid 100, no max_lines → full tail.
        let s: String = f.call::<String>(100u64).unwrap();
        assert_eq!(s, "x1\nx2\nx3");
        // Unknown uid → empty.
        let s: String = f.call::<String>(999u64).unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn rterm_scrollback_text_of_targets_specific_pane() {
        // Per-pane variant of `scrollback_text`. Two panes carry
        // distinct tails; the (tab, pane) addressing picks the right
        // one. Unknown (tab, pane) → empty string. max_lines tail-
        // slices like the focused variant.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 1,
                    scrollback_tail: "a1\na2\na3".to_string(),
                    ..PaneInfo::default()
                },
                PaneInfo {
                    tab: 0,
                    pane: 1,
                    uid: 2,
                    scrollback_tail: "b1\nb2\nb3\nb4\nb5".to_string(),
                    ..PaneInfo::default()
                },
            ],
            ..TerminalState::default()
        });
        let f: Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("scrollback_text_of")
            .unwrap();
        // Hit, no max_lines → full tail.
        let s: String = f.call::<String>((1u32, 1u32)).unwrap();
        assert_eq!(s, "a1\na2\na3");
        // Tail-slice to last 2 lines of pane (1,2).
        let s: String = f.call::<String>((1u32, 2u32, 2u32)).unwrap();
        assert_eq!(s, "b4\nb5");
        // Unknown pane → empty.
        let s: String = f.call::<String>((9u32, 9u32)).unwrap();
        assert_eq!(s, "");
    }

    #[test]
    fn rterm_scrollback_text_tail_slices_to_max_lines() {
        // The renderer publishes a `\n`-joined scrollback string; the
        // Lua function tail-slices to the requested line count. Empty
        // snapshot returns empty regardless of max_lines.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            scrollback_text: "a\nb\nc\nd\ne".to_string(),
            ..TerminalState::default()
        });
        let scrollback: Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("scrollback_text")
            .unwrap();
        // No arg → full snapshot.
        let all: String = scrollback.call::<String>(()).unwrap();
        assert_eq!(all, "a\nb\nc\nd\ne");
        // Tail of 2 lines.
        let tail: String = scrollback.call::<String>(2u32).unwrap();
        assert_eq!(tail, "d\ne");
        // Asking for more than we have returns the full snapshot.
        let bigger: String = scrollback.call::<String>(100u32).unwrap();
        assert_eq!(bigger, "a\nb\nc\nd\ne");
        // 0 → empty (caller asked for nothing).
        let zero: String = scrollback.call::<String>(0u32).unwrap();
        assert_eq!(zero, "");
        // Empty snapshot stays empty.
        host.set_state(TerminalState::default());
        let empty: String = scrollback.call::<String>(()).unwrap();
        assert_eq!(empty, "");
    }

    #[test]
    fn rterm_theme_returns_live_palette_rgb() {
        // `rterm.theme()` exposes the renderer's live default fg / bg /
        // cursor colours so status-line plugins can match the running
        // palette. Plugins that listened on the `theme` event re-read
        // `theme()` to pick up the new triple after a swap.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            theme_fg: [220, 220, 220],
            theme_bg: [10, 12, 18],
            theme_cursor: [255, 204, 102],
            ..TerminalState::default()
        });
        let theme: Table = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("theme")
            .unwrap()
            .call(())
            .unwrap();
        let fg: Table = theme.get("fg").unwrap();
        assert_eq!(fg.get::<u8>(1).unwrap(), 220);
        assert_eq!(fg.get::<u8>(2).unwrap(), 220);
        assert_eq!(fg.get::<u8>(3).unwrap(), 220);
        let bg: Table = theme.get("bg").unwrap();
        assert_eq!(bg.get::<u8>(1).unwrap(), 10);
        assert_eq!(bg.get::<u8>(2).unwrap(), 12);
        assert_eq!(bg.get::<u8>(3).unwrap(), 18);
        let cur: Table = theme.get("cursor").unwrap();
        assert_eq!(cur.get::<u8>(1).unwrap(), 255);
        assert_eq!(cur.get::<u8>(2).unwrap(), 204);
        assert_eq!(cur.get::<u8>(3).unwrap(), 102);
    }

    #[test]
    fn rterm_find_pane_by_uid_returns_exact_match() {
        // Adding `uid` as a search criterion lets plugins do
        // `find_pane({uid=N})` without iterating list_panes(). Exact
        // match (not substring) — uids are opaque integers, no need
        // for case_insensitive either.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 100,
                    title: "alpha".to_string(),
                    ..PaneInfo::default()
                },
                PaneInfo {
                    tab: 0,
                    pane: 1,
                    uid: 200,
                    title: "beta".to_string(),
                    ..PaneInfo::default()
                },
            ],
            ..TerminalState::default()
        });
        let find: mlua::Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("find_pane")
            .unwrap();
        let opts = host.lua.create_table().unwrap();
        opts.set("uid", 200u64).unwrap();
        let res: Table = find.call(opts).unwrap();
        assert_eq!(res.get::<u32>("tab").unwrap(), 1);
        assert_eq!(res.get::<u32>("pane").unwrap(), 2);
        assert_eq!(res.get::<u64>("uid").unwrap(), 200);
        // Unknown uid → nil.
        let opts = host.lua.create_table().unwrap();
        opts.set("uid", 999u64).unwrap();
        let miss: mlua::Value = find.call(opts).unwrap();
        assert!(matches!(miss, mlua::Value::Nil));
    }

    #[test]
    fn rterm_find_tab_substring_matches_custom_title() {
        // `find_tab({title=...})` returns the first tab whose
        // custom_title contains the substring (case-sensitive by
        // default). Tabs without a custom_title never match — we don't
        // synthesize a name from focused-pane title because that's
        // already exposed via list_panes / find_pane.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            tabs: vec![
                TabInfo {
                    idx: 0,
                    focused: false,
                    custom_title: Some("shell".to_string()),
                    focused_pane_uid: 100,
                    ..TabInfo::default()
                },
                TabInfo {
                    idx: 1,
                    focused: true,
                    custom_title: Some("Build Logs".to_string()),
                    focused_pane_uid: 200,
                    ..TabInfo::default()
                },
                TabInfo {
                    idx: 2,
                    focused: false,
                    custom_title: None, // unnamed — never matches
                    focused_pane_uid: 300,
                    ..TabInfo::default()
                },
            ],
            ..TerminalState::default()
        });
        let find: mlua::Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("find_tab")
            .unwrap();
        // Substring hit: "log" matches "Build Logs" via the second tab.
        let res: mlua::Table = find
            .call(host.lua.create_table_from([("title", "ogs")]).unwrap())
            .unwrap();
        assert_eq!(res.get::<u32>("idx").unwrap(), 2, "Lua surface is 1-based");
        assert_eq!(res.get::<u64>("focused_pane_uid").unwrap(), 200);
        assert_eq!(res.get::<String>("custom_title").unwrap(), "Build Logs");
        // Case-sensitive miss → nil.
        let miss: mlua::Value = find
            .call(host.lua.create_table_from([("title", "logs")]).unwrap())
            .unwrap();
        assert!(matches!(miss, mlua::Value::Nil));
        // Case-insensitive hit on the same query.
        let ci = host.lua.create_table().unwrap();
        ci.set("title", "logs").unwrap();
        ci.set("case_insensitive", true).unwrap();
        let res: mlua::Table = find.call(ci).unwrap();
        assert_eq!(res.get::<u32>("idx").unwrap(), 2);
        // Empty / missing title opt → nil (avoids returning tab #1 by
        // accident).
        let nil1: mlua::Value = find.call(host.lua.create_table().unwrap()).unwrap();
        assert!(matches!(nil1, mlua::Value::Nil));
    }

    #[test]
    fn rterm_find_pane_substring_and_match_returns_first_hit() {
        // find_pane matches every supplied field as a case-sensitive
        // substring. With three panes set up, a query for "vim" picks
        // the first whose foreground_process contains it.
        let host = PluginHost::new().expect("host inits");
        let mk = |tab, pane, title: &str, cwd: Option<&str>, fg: Option<&str>| PaneInfo {
            tab,
            pane,
            title: title.to_string(),
            cwd: cwd.map(String::from),
            foreground_process: fg.map(String::from),
            ..PaneInfo::default()
        };
        host.set_state(TerminalState {
            panes: vec![
                mk(0, 0, "bash", Some("/home/u"), Some("bash")),
                mk(0, 1, "editor", Some("/home/u/proj"), Some("vim")),
                mk(1, 0, "logs", Some("/var/log"), Some("tail")),
            ],
            ..TerminalState::default()
        });
        let find: mlua::Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("find_pane")
            .unwrap();
        // Foreground-process hit on the second pane.
        let res: Table = find
            .call(host.lua.create_table_from([("foreground_process", "vim")]).unwrap())
            .unwrap();
        assert_eq!(res.get::<u32>("tab").unwrap(), 1);
        assert_eq!(res.get::<u32>("pane").unwrap(), 2);
        // Result also carries the matched pane's title and cwd so
        // plugins don't have to walk `list_panes()` to confirm they
        // hit the right one.
        assert_eq!(res.get::<String>("title").unwrap(), "editor");
        assert_eq!(res.get::<String>("cwd").unwrap(), "/home/u/proj");
        assert!(!res.get::<bool>("focused").unwrap());

        // Multi-criteria AND: cwd substring + foreground match.
        let opts2 = host
            .lua
            .create_table_from([("cwd", "proj"), ("foreground_process", "vim")])
            .unwrap();
        let res2: Table = find.call(opts2).unwrap();
        assert_eq!(res2.get::<u32>("tab").unwrap(), 1);
        assert_eq!(res2.get::<u32>("pane").unwrap(), 2);

        // No match → nil.
        let opts3 = host
            .lua
            .create_table_from([("foreground_process", "emacs")])
            .unwrap();
        let res3: mlua::Value = find.call(opts3).unwrap();
        assert!(matches!(res3, mlua::Value::Nil));

        // Empty opts (no predicate) → nil (don't return a misleading 1:1).
        let empty: mlua::Value = find.call(host.lua.create_table().unwrap()).unwrap();
        assert!(matches!(empty, mlua::Value::Nil));
    }

    #[test]
    fn rterm_find_pane_case_insensitive_opt() {
        // `case_insensitive = true` lowercases both haystack and needle
        // so e.g. a "VIM" foreground (caps from /proc/comm of an
        // unusual binary) still matches "vim". Default-case is unchanged.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            panes: vec![PaneInfo {
                tab: 0,
                pane: 0,
                title: "EDITOR".to_string(),
                foreground_process: Some("Vim".to_string()),
                ..PaneInfo::default()
            }],
            ..TerminalState::default()
        });
        let find: mlua::Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("find_pane")
            .unwrap();
        // Default (case-sensitive): "vim" misses "Vim".
        let sensitive = host
            .lua
            .create_table_from([("foreground_process", "vim")])
            .unwrap();
        let miss: mlua::Value = find.call(sensitive).unwrap();
        assert!(matches!(miss, mlua::Value::Nil));
        // With case_insensitive=true the same query hits.
        let opts = host.lua.create_table().unwrap();
        opts.set("foreground_process", "vim").unwrap();
        opts.set("case_insensitive", true).unwrap();
        let hit: Table = find.call(opts).unwrap();
        assert_eq!(hit.get::<u32>("tab").unwrap(), 1);
        assert_eq!(hit.get::<u32>("pane").unwrap(), 1);
        // Title field also lowercases.
        let opts_title = host.lua.create_table().unwrap();
        opts_title.set("title", "editor").unwrap();
        opts_title.set("case_insensitive", true).unwrap();
        let hit2: Table = find.call(opts_title).unwrap();
        assert_eq!(hit2.get::<u32>("tab").unwrap(), 1);
    }

    #[test]
    fn rterm_copy_pane_routes_text_through_the_copy_channel() {
        // copy_pane(tab, pane) → look up text in the snapshot, push it
        // through the same `pending_copy` slot as `rterm.copy(text)`.
        // Returns true on hit, false on miss / empty pane.
        let host = PluginHost::new().expect("host inits");
        let p = PaneInfo {
            tab: 0,
            pane: 0,
            text: "yank me".to_string(),
            ..PaneInfo::default()
        };
        host.set_state(TerminalState {
            panes: vec![p],
            ..TerminalState::default()
        });
        let copy_pane: mlua::Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("copy_pane")
            .unwrap();
        // Successful hit returns true and queues the text.
        let ok: bool = copy_pane.call((1u32, 1u32)).unwrap();
        assert!(ok);
        assert_eq!(host.take_pending_copy().as_deref(), Some("yank me"));
        // Drained.
        assert!(host.take_pending_copy().is_none());
        // Miss returns false and leaves the slot empty.
        let miss: bool = copy_pane.call((9u32, 9u32)).unwrap();
        assert!(!miss);
        assert!(host.take_pending_copy().is_none());
        // Empty-text pane also returns false (no point pushing "" to clip).
        host.set_state(TerminalState {
            panes: vec![PaneInfo {
                tab: 0,
                pane: 0,
                text: String::new(),
                ..PaneInfo::default()
            }],
            ..TerminalState::default()
        });
        let empty: bool = copy_pane.call((1u32, 1u32)).unwrap();
        assert!(!empty);
        assert!(host.take_pending_copy().is_none());
    }

    #[test]
    fn rterm_cursor_returns_focused_pane_cursor() {
        // `rterm.cursor()` reads the focused pane's snapshot — `row` /
        // `col` are 1-based (matching `list_panes`) and `visible`
        // reflects DEC ?25. Nil when no pane is focused.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    focused: false,
                    cursor_row: 1,
                    cursor_col: 1,
                    cursor_visible: false,
                    ..PaneInfo::default()
                },
                PaneInfo {
                    tab: 0,
                    pane: 1,
                    focused: true,
                    cursor_row: 7,
                    cursor_col: 14,
                    cursor_visible: true,
                    ..PaneInfo::default()
                },
            ],
            ..TerminalState::default()
        });
        let cur: mlua::Table = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("cursor")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(cur.get::<u16>("row").unwrap(), 7);
        assert_eq!(cur.get::<u16>("col").unwrap(), 14);
        assert!(cur.get::<bool>("visible").unwrap());
        // No focused pane → nil.
        host.set_state(TerminalState {
            panes: vec![PaneInfo {
                tab: 0,
                pane: 0,
                focused: false,
                ..PaneInfo::default()
            }],
            ..TerminalState::default()
        });
        let nil: mlua::Value = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("cursor")
            .unwrap()
            .call(())
            .unwrap();
        assert!(matches!(nil, mlua::Value::Nil));
    }

    #[test]
    fn rterm_terminal_text_with_tab_pane_args_looks_up_specific_pane() {
        // `rterm.terminal_text(tab, pane)` returns that pane's text from
        // the snapshot — independent of which pane is currently focused.
        // Plugins use this for "log the last shell prompt in every
        // pane"-style workflows.
        let host = PluginHost::new().expect("host inits");
        let p1 = PaneInfo {
            tab: 0,
            pane: 0,
            text: "first pane line 1\nfirst pane line 2".to_string(),
            ..PaneInfo::default()
        };
        let p2 = PaneInfo {
            tab: 1,
            pane: 2,
            text: "second pane".to_string(),
            ..PaneInfo::default()
        };
        host.set_state(TerminalState {
            grid_text: "focused-only".to_string(),
            panes: vec![p1, p2],
            ..TerminalState::default()
        });
        let term_text: mlua::Function = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get("terminal_text")
            .unwrap();
        // No args → focused (compat).
        let focused: String = term_text.call(()).unwrap();
        assert_eq!(focused, "focused-only");
        // (1, 1) → first pane (1-based).
        let a: String = term_text.call((1u32, 1u32)).unwrap();
        assert_eq!(a, "first pane line 1\nfirst pane line 2");
        // (2, 3) → second pane (1-based).
        let b: String = term_text.call((2u32, 3u32)).unwrap();
        assert_eq!(b, "second pane");
        // Out-of-range → nil. (1, 99) doesn't exist.
        let missing: mlua::Value = term_text.call((1u32, 99u32)).unwrap();
        assert!(matches!(missing, mlua::Value::Nil));
    }

    #[test]
    fn rterm_tab_silence_ms_zero_means_disabled() {
        // `set_tab_silence_ms(0)` is the documented way for plugins to
        // suppress the `tab.silence` event. The Lua getter must round-
        // trip the literal 0 so plugins can branch on `if rterm.tab_silence_ms() == 0`.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            tab_silence_ms: 0,
            ..TerminalState::default()
        });
        let n: u64 = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("tab_silence_ms")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn rterm_lua_setters_for_terminal_toggles() {
        // Lua-side counterparts to set_*_override. Plugins use these for
        // e.g. presentation-mode toggles. Each Lua call must reach the
        // matching pending slot and not cross-pollute siblings.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.set_cursor_blink(false)
                rterm.set_show_scrollbar(true)
                rterm.set_scroll_on_output(false)
            "#,
            )
            .exec()
            .unwrap();
        assert_eq!(host.take_pending_cursor_blink(), Some(false));
        assert_eq!(host.take_pending_show_scrollbar(), Some(true));
        assert_eq!(host.take_pending_scroll_on_output(), Some(false));
    }

    #[test]
    fn terminal_toggle_overrides_round_trip() {
        // Hot-reload publishes these via `set_*_override` on every TOML
        // file change. Verify each setter populates its pending slot and
        // a take consumes it. Catches a wire-swap (e.g. cursor_blink
        // value landing in show_scrollbar's slot).
        let host = PluginHost::new().expect("host inits");

        // Initial state — all None.
        assert_eq!(host.take_pending_cursor_blink(), None);
        assert_eq!(host.take_pending_show_scrollbar(), None);
        assert_eq!(host.take_pending_scroll_on_output(), None);

        host.set_cursor_blink_override(false);
        host.set_show_scrollbar_override(true);
        host.set_scroll_on_output_override(false);

        assert_eq!(host.take_pending_cursor_blink(), Some(false));
        assert_eq!(host.take_pending_show_scrollbar(), Some(true));
        assert_eq!(host.take_pending_scroll_on_output(), Some(false));

        // Drained — second take returns None for all three.
        assert_eq!(host.take_pending_cursor_blink(), None);
        assert_eq!(host.take_pending_show_scrollbar(), None);
        assert_eq!(host.take_pending_scroll_on_output(), None);

        // Last-write-wins per slot.
        host.set_cursor_blink_override(true);
        host.set_cursor_blink_override(false);
        assert_eq!(host.take_pending_cursor_blink(), Some(false));
    }

    #[test]
    fn rterm_lua_setters_for_bell_toggles() {
        // Lua surface for the new bell channels mirrors the existing
        // cursor_blink / show_scrollbar shape. Verify each call lands
        // in its own slot.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.set_bell_visual(false)
                rterm.set_bell_urgent(true)
            "#,
            )
            .exec()
            .unwrap();
        assert_eq!(host.take_pending_bell_visual(), Some(false));
        assert_eq!(host.take_pending_bell_urgent(), Some(true));
        // Drained.
        assert_eq!(host.take_pending_bell_visual(), None);
        assert_eq!(host.take_pending_bell_urgent(), None);
    }

    #[test]
    fn rterm_set_pane_bell_muted_queues_target_with_zero_based_indices() {
        // Plugins call `rterm.set_pane_bell_muted(1, 1, true)` using
        // 1-based indices (matching `list_panes()`). The queue stores
        // 0-based pairs so the App's drain doesn't need to know about
        // the wire format. Each call appends an entry — last-wins on
        // duplicate targets is the App's responsibility (apply in order).
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.set_pane_bell_muted(1, 1, true)
                rterm.set_pane_bell_muted(2, 3, false)
                rterm.set_pane_bell_muted(1, 1, false)
            "#,
            )
            .exec()
            .unwrap();
        let drained = host.drain_pending_pane_bell_mute();
        assert_eq!(
            drained,
            vec![(0, 0, true), (1, 2, false), (0, 0, false)],
        );
        // Drained.
        assert!(host.drain_pending_pane_bell_mute().is_empty());
    }

    #[test]
    fn set_pane_bell_muted_by_uid_queues_through_dedicated_channel() {
        // Sibling of `set_pane_bell_muted` that takes a stable
        // uid. The App resolves uid → (tab, pane). Test pins the
        // queue ordering and that the indexed and uid channels
        // are distinct (an entry pushed by one mustn't drain via
        // the other).
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.set_pane_bell_muted_by_uid(42, true)
                rterm.set_pane_bell_muted_by_uid(99, false)
                rterm.set_pane_bell_muted_by_uid(42, false)
            "#,
            )
            .exec()
            .unwrap();
        // Indexed channel must NOT have these.
        assert!(host.drain_pending_pane_bell_mute().is_empty());
        let drained = host.drain_pending_pane_bell_mute_by_uid();
        assert_eq!(
            drained,
            vec![(42, true), (99, false), (42, false)],
        );
        // Drained.
        assert!(host.drain_pending_pane_bell_mute_by_uid().is_empty());
    }

    #[test]
    fn slow_command_ms_override_round_trip_via_lua_and_setter() {
        // Two write paths land in the same pending slot — the config
        // watcher's `set_*_override` and the Lua `rterm.set_slow_command_ms`
        // helper. Pin that they share the channel and drain once.
        let host = PluginHost::new().expect("host inits");
        assert_eq!(host.take_pending_slow_command_ms(), None);
        host.set_slow_command_ms_override(7_500);
        assert_eq!(host.take_pending_slow_command_ms(), Some(7_500));
        // Drained.
        assert_eq!(host.take_pending_slow_command_ms(), None);
        // Lua path.
        host.lua
            .load("rterm.set_slow_command_ms(0)")
            .exec()
            .unwrap();
        assert_eq!(host.take_pending_slow_command_ms(), Some(0));
    }

    #[test]
    fn bell_overrides_round_trip_via_set_override() {
        // Config watcher path: TOML reload publishes the new bell.*
        // toggle values via `set_bell_*_override` for the renderer to
        // pick up on the next frame. Same drain-once semantics as the
        // other terminal toggles.
        let host = PluginHost::new().expect("host inits");
        assert_eq!(host.take_pending_bell_visual(), None);
        assert_eq!(host.take_pending_bell_urgent(), None);

        host.set_bell_visual_override(false);
        host.set_bell_urgent_override(false);
        assert_eq!(host.take_pending_bell_visual(), Some(false));
        assert_eq!(host.take_pending_bell_urgent(), Some(false));

        // Last-write-wins per slot; independence from sibling slots.
        host.set_bell_visual_override(true);
        host.set_bell_visual_override(false);
        assert_eq!(host.take_pending_bell_visual(), Some(false));
        assert_eq!(host.take_pending_bell_urgent(), None);
    }

    #[test]
    fn rterm_scroll_to_line_pinning() {
        // `rterm.scroll_to_line(N)` queues an absolute scroll target.
        // Pairs with `prompt_marks()` for "jump to my Nth prompt"
        // plugins. Last-write-wins (single pending slot), drained once.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.scroll_to_line(10); rterm.scroll_to_line(42)"#)
            .exec()
            .unwrap();
        // Last call clobbers — only one slot.
        assert_eq!(host.take_pending_scroll_to_line(), Some(42));
        // Drained.
        assert_eq!(host.take_pending_scroll_to_line(), None);
        // Zero is a legitimate value (top of scrollback) — must not be
        // silently mapped to None.
        host.lua.load(r#"rterm.scroll_to_line(0)"#).exec().unwrap();
        assert_eq!(host.take_pending_scroll_to_line(), Some(0));
    }

    #[test]
    fn rterm_bell_is_one_shot() {
        // `rterm.bell()` is the plugin equivalent of OSC 7 / `\a`. The
        // App reads the flag once per frame and flashes the surface;
        // multiple calls in one frame still only fire once (no flicker
        // amplification). Tests the boolean one-shot semantics.
        let host = PluginHost::new().expect("host inits");
        // Calling twice still drains as one "bell happened".
        host.lua.load(r#"rterm.bell(); rterm.bell(); rterm.bell()"#).exec().unwrap();
        assert!(host.take_pending_bell());
        // Second drain — already consumed.
        assert!(!host.take_pending_bell());
        // Re-ring and re-drain.
        host.lua.load(r#"rterm.bell()"#).exec().unwrap();
        assert!(host.take_pending_bell());
    }

    #[test]
    fn rterm_kill_pane_and_kill_tab_route_one_based() {
        // `kill_pane(tab, pane)` and `kill_tab(idx)` — destructive ops.
        // Both consume 1-based Lua indices and translate to 0-based on
        // the queue. An off-by-one here would close the wrong pane —
        // worth pinning.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.kill_pane(1, 2)
                rterm.kill_pane(3, 1)
                rterm.kill_tab(4)
            "#,
            )
            .exec()
            .unwrap();
        let cmds = host.drain_pending_commands();
        let kills: Vec<(usize, usize)> = cmds
            .iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::KillPane(t, p) => Some((*t, *p)),
                _ => None,
            })
            .collect();
        assert_eq!(kills, vec![(0, 1), (2, 0)]);
        let tab_kills: Vec<usize> = cmds
            .iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::KillTab(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(tab_kills, vec![3]);
        // Saturating-sub guards against an accidental 0 — clamped to 0,
        // not a usize underflow.
        host.lua.load(r#"rterm.kill_tab(0); rterm.kill_pane(0, 0)"#).exec().unwrap();
        let cmds2 = host.drain_pending_commands();
        let tab_kills_zero: Vec<usize> = cmds2
            .iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::KillTab(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(tab_kills_zero, vec![0]);
        let kills_zero: Vec<(usize, usize)> = cmds2
            .into_iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::KillPane(t, p) => Some((t, p)),
                _ => None,
            })
            .collect();
        assert_eq!(kills_zero, vec![(0, 0)]);
    }

    #[test]
    fn rterm_path_setters_round_trip_to_lua_getters() {
        // The App calls `host.set_config_dir(...)`, `set_cache_dir(...)`,
        // and `set_shell_program(...)` once at startup so plugins can
        // discover where to drop persistent state and which shell is in
        // use. Each setter must be reflected by the matching Lua getter,
        // and an unset value returns `nil` (not the empty string).
        let host = PluginHost::new().expect("host inits");
        let rterm: Table = host.lua.globals().get("rterm").unwrap();

        // Unset → nil.
        let v: mlua::Value = rterm.get::<Function>("config_dir").unwrap().call(()).unwrap();
        assert!(matches!(v, mlua::Value::Nil));
        let v: mlua::Value = rterm.get::<Function>("cache_dir").unwrap().call(()).unwrap();
        assert!(matches!(v, mlua::Value::Nil));
        let v: mlua::Value = rterm.get::<Function>("shell").unwrap().call(()).unwrap();
        assert!(matches!(v, mlua::Value::Nil));

        // After install — values round-trip.
        host.set_config_dir("/home/u/.config/rterm");
        host.set_cache_dir("/home/u/.cache/rterm");
        host.set_shell_program("/bin/zsh");

        let s: String = rterm.get::<Function>("config_dir").unwrap().call(()).unwrap();
        assert_eq!(s, "/home/u/.config/rterm");
        let s: String = rterm.get::<Function>("cache_dir").unwrap().call(()).unwrap();
        assert_eq!(s, "/home/u/.cache/rterm");
        let s: String = rterm.get::<Function>("shell").unwrap().call(()).unwrap();
        assert_eq!(s, "/bin/zsh");

        // `rterm.config_path()` joins config_dir with "config.toml".
        let p: String = rterm.get::<Function>("config_path").unwrap().call(()).unwrap();
        assert!(p.ends_with("config.toml"), "got {p:?}");
        assert!(p.starts_with("/home/u/.config/rterm"), "got {p:?}");
    }

    #[test]
    fn rterm_read_clipboard_routes_through_injected_reader() {
        // The App injects a `ClipboardReader` closure that reads the
        // system clipboard. Lua calls `rterm.read_clipboard()` and gets
        // back what the reader returned — `nil` when no reader is
        // installed (so the test env without arboard doesn't crash).
        let host = PluginHost::new().expect("host inits");

        // No reader installed → returns nil.
        let v: mlua::Value = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("read_clipboard")
            .unwrap()
            .call(())
            .unwrap();
        assert!(matches!(v, mlua::Value::Nil));

        // Inject a stub reader and verify Lua sees its output.
        host.set_clipboard_reader(Arc::new(|| Some("from clipboard".to_string())));
        let s: Option<String> = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("read_clipboard")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(s.as_deref(), Some("from clipboard"));

        // Reader returns None (e.g. clipboard read failed) → Lua gets nil.
        host.set_clipboard_reader(Arc::new(|| None));
        let v: mlua::Value = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("read_clipboard")
            .unwrap()
            .call(())
            .unwrap();
        assert!(matches!(v, mlua::Value::Nil));
    }

    #[test]
    fn register_action_round_trips_through_host() {
        // Plugin-registered actions are how the command palette gets
        // user-defined entries. Lifecycle: `register_action` adds it,
        // `action_names` lists it (sorted), the host `run_action` calls
        // its Lua body, and `unregister_action` removes it.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                _G.bag = {}
                rterm.register_action("alpha", function() table.insert(_G.bag, "a") end)
                rterm.register_action("beta",  function() table.insert(_G.bag, "b") end)
            "#,
            )
            .exec()
            .unwrap();

        let names = host.action_names();
        // The order isn't guaranteed by HashMap, but every registered
        // name must show up.
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));

        host.run_action("alpha").unwrap();
        host.run_action("beta").unwrap();
        host.run_action("alpha").unwrap();
        let bag: mlua::Table = host.lua.globals().get("bag").unwrap();
        assert_eq!(bag.get::<String>(1).unwrap(), "a");
        assert_eq!(bag.get::<String>(2).unwrap(), "b");
        assert_eq!(bag.get::<String>(3).unwrap(), "a");

        // Unknown action — run_action is a no-op (no panic).
        host.run_action("nonexistent").unwrap();

        // Unregister — Lua `list_actions` reflects it immediately.
        host.lua
            .load(r#"_G.removed = rterm.unregister_action("alpha")"#)
            .exec()
            .unwrap();
        let removed: bool = host.lua.globals().get("removed").unwrap();
        assert!(removed);
        let names_after = host.action_names();
        assert!(!names_after.contains(&"alpha".to_string()));
        assert!(names_after.contains(&"beta".to_string()));
    }

    #[test]
    fn rterm_start_search_accepts_optional_args() {
        // `rterm.start_search()` with no args → opens empty overlay,
        // regex off. With (query) only → regex defaults to false. With
        // (query, true) → regex on. Plugins use the no-arg form for "let
        // the user start typing"; pinning the defaults catches an
        // accidental swap in arg order.
        let host = PluginHost::new().expect("host inits");

        host.lua.load(r#"rterm.start_search()"#).exec().unwrap();
        assert_eq!(host.take_pending_start_search(), Some((String::new(), false)));

        host.lua.load(r#"rterm.start_search("foo")"#).exec().unwrap();
        assert_eq!(
            host.take_pending_start_search(),
            Some(("foo".to_string(), false)),
        );

        host.lua
            .load(r#"rterm.start_search("bar", true)"#)
            .exec()
            .unwrap();
        assert_eq!(
            host.take_pending_start_search(),
            Some(("bar".to_string(), true)),
        );
    }

    #[test]
    fn rterm_set_font_size_drops_non_finite() {
        // NaN / Infinity from Lua (e.g. `0/0`, `1/0`) must NOT propagate
        // — the renderer's clamp panics on non-finite inputs, so the
        // Lua boundary drops them. Pending slot stays None.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.set_font_size(0/0); rterm.set_font_size(1/0)"#)
            .exec()
            .unwrap();
        assert_eq!(host.take_pending_font_size(), None);
        // Sanity: finite values still propagate.
        host.lua.load(r#"rterm.set_font_size(14)"#).exec().unwrap();
        assert_eq!(host.take_pending_font_size(), Some(14.0));
    }

    #[test]
    fn rterm_set_font_size_clamps_into_pending() {
        // Plugins set font size at runtime via `rterm.set_font_size(N)`.
        // The Lua call is async — value lands in a pending slot drained
        // by the App. Verify the most-recent value wins (only one slot)
        // and the drain leaves it empty.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.set_font_size(11); rterm.set_font_size(17)"#)
            .exec()
            .unwrap();
        let size = host.take_pending_font_size();
        // Only one slot, so the later call clobbers the earlier — that's
        // the right behaviour for a setter (idempotent).
        assert_eq!(size, Some(17.0));
        assert!(host.take_pending_font_size().is_none());
    }

    #[test]
    fn rterm_set_opacity_drops_non_finite() {
        // Same NaN/Inf guard as set_font_size — the renderer's clamp
        // panics on non-finite, so the boundary drops them silently.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.set_opacity(0/0); rterm.set_opacity(1/0)"#)
            .exec()
            .unwrap();
        assert_eq!(host.take_pending_opacity(), None);
        host.lua.load(r#"rterm.set_opacity(0.5)"#).exec().unwrap();
        assert_eq!(host.take_pending_opacity(), Some(0.5));
    }

    #[test]
    fn rterm_set_opacity_clamps_into_pending() {
        // The renderer accepts only `0.0..=1.0`; clamp at the boundary so
        // out-of-range plugin input becomes a no-op visually rather than
        // an invisible window (negative) or a panic (NaN already handled).
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.set_opacity(-1.5); rterm.set_opacity(2.0)"#)
            .exec()
            .unwrap();
        assert_eq!(host.take_pending_opacity(), Some(1.0));
        assert!(host.take_pending_opacity().is_none());

        host.lua.load(r#"rterm.set_opacity(-0.3)"#).exec().unwrap();
        assert_eq!(host.take_pending_opacity(), Some(0.0));
    }

    #[test]
    fn rterm_title_setters_route_correctly() {
        // Three independent title overrides — each must drain via its
        // own channel. A bug merging them (e.g. set_tab_title leaking
        // into the window-title path) would have status-line plugins
        // accidentally renaming the window.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.set_tab_title("My Tab")
                rterm.set_pane_title(1, 2, "build log")
                rterm.set_window_title("rterm — running")
            "#,
            )
            .exec()
            .unwrap();
        let tab_titles = host.drain_pending_tab_titles();
        assert_eq!(tab_titles, vec![(None, "My Tab".to_string())]);
        let pane_titles = host.drain_pending_pane_titles();
        // 1-based → 0-based translation.
        assert_eq!(pane_titles, vec![(0, 1, "build log".to_string())]);
        // Window title uses Option<Option<String>> — outer Some means
        // "update requested", inner Some means new value.
        let win = host.take_pending_window_title();
        assert_eq!(win, Some(Some("rterm — running".to_string())));
    }

    #[test]
    fn rterm_set_tab_title_by_index_routes_with_zero_based_index() {
        // `set_tab_title_by_index(2, "build")` targets the 2nd tab
        // (1-based → 0-based on the way in). Empty name clears any
        // existing override. The drain preserves push order so the
        // App can apply them left-to-right.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.set_tab_title_by_index(2, "build")
                rterm.set_tab_title_by_index(3, "")   -- clear
                rterm.set_tab_title("active")          -- target active tab
            "#,
            )
            .exec()
            .unwrap();
        let titles = host.drain_pending_tab_titles();
        assert_eq!(
            titles,
            vec![
                (Some(1), "build".to_string()),
                (Some(2), "".to_string()),
                (None, "active".to_string()),
            ],
        );
        assert!(host.drain_pending_tab_titles().is_empty());
    }

    #[test]
    fn rterm_focus_helpers_route_correctly() {
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.focus_tab(3)
                rterm.focus_pane(2, 4)
            "#,
            )
            .exec()
            .unwrap();
        // 1-based → 0-based.
        assert_eq!(host.take_pending_tab_focus(), Some(2));
        assert_eq!(host.take_pending_focus(), Some((1, 3)));
    }

    #[test]
    fn rterm_attention_and_bell_one_shots() {
        // `rterm.attention()` is the plugin equivalent of OSC 9 bell —
        // pings the taskbar exactly once until drained. Same boolean
        // semantics for the App-side bell flag.
        let host = PluginHost::new().expect("host inits");
        host.lua.load(r#"rterm.attention()"#).exec().unwrap();
        assert!(host.take_pending_attention(), "first take returns true");
        assert!(!host.take_pending_attention(), "second take returns false");
    }

    #[test]
    fn rterm_scroll_and_open_url_queue_correctly() {
        // `rterm.scroll(delta)` accumulates into a FIFO (each call adds
        // a step). `rterm.open_url(url)` is FIFO too, one entry per call.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.scroll(-3)
                rterm.scroll(5)
                rterm.open_url("https://example.com")
                rterm.open_url("file:///tmp/x")
            "#,
            )
            .exec()
            .unwrap();
        let cmds = host.drain_pending_commands();
        let scrolls: Vec<i32> = cmds
            .iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::Scroll(d) => Some(*d),
                _ => None,
            })
            .collect();
        assert_eq!(scrolls, vec![-3, 5]);
        let urls: Vec<String> = cmds
            .into_iter()
            .filter_map(|c| match c {
                rterm_core::PluginCmd::OpenUrl(u) => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(
            urls,
            vec!["https://example.com".to_string(), "file:///tmp/x".to_string()],
        );
    }

    #[test]
    fn rterm_set_scrollback_and_set_tab_silence_ms_route() {
        // Two related setters both feed pending Option<...> slots used
        // by the App on the next frame. Last-write-wins, drained once.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.set_scrollback(50000)
                rterm.set_tab_silence_ms(2500)
            "#,
            )
            .exec()
            .unwrap();
        assert_eq!(host.take_pending_scrollback_limit(), Some(50_000));
        assert_eq!(host.take_pending_tab_silence_ms(), Some(2_500));
        // Drained — both should now be empty.
        assert_eq!(host.take_pending_scrollback_limit(), None);
        assert_eq!(host.take_pending_tab_silence_ms(), None);
    }

    #[test]
    fn rterm_set_palette_packs_named_array_correctly() {
        // `rterm.set_palette{ default_fg = {r,g,b}, default_bg = {...},
        // named = { [1] = {...}, ..., [16] = {...} } }`. The named field
        // requires *all 16* slots — partial tables fall back to None so
        // the App keeps its current palette intact.
        let host = PluginHost::new().expect("host inits");
        // Build a full 1..=16 named array where slot i has [i, i, i] for
        // easy round-trip assertions.
        let mut named_lua = String::from("named = {\n");
        for i in 1..=16 {
            named_lua.push_str(&format!("    [{i}] = {{{i}, {i}, {i}}},\n"));
        }
        named_lua.push_str("},\n");
        let script = format!(
            r#"
                rterm.set_palette({{
                    default_fg = {{200, 200, 200}},
                    cursor     = {{255, 204, 102}},
                    {named_lua}
                }})
            "#,
        );
        host.lua.load(&script).exec().unwrap();
        let p = host.take_pending_palette().expect("palette queued");
        assert_eq!(p.default_fg, Some([200, 200, 200]));
        assert_eq!(p.default_bg, None);
        assert_eq!(p.cursor, Some([255, 204, 102]));
        let named = p.named.expect("full 1..=16 named array populated");
        for (i, slot) in named.iter().enumerate() {
            // Lua sent {i+1, i+1, i+1} for 1-based slot i+1.
            let expected = (i as u8) + 1;
            assert_eq!(*slot, [expected, expected, expected], "slot {i}");
        }

        // Partial named table → named field stays None. Drained slot.
        host.lua
            .load(r#"rterm.set_palette({ named = { [1] = {9,9,9} } })"#)
            .exec()
            .unwrap();
        let p = host.take_pending_palette().expect("palette queued");
        assert!(p.named.is_none(), "incomplete named array should reject");
    }

    #[test]
    fn rterm_list_panes_returns_one_based_indexed_entries() {
        // Mirror of `rterm_tabs_returns_*` for the per-pane snapshot.
        // Plugins build their entire model of "what panes exist" off
        // this, so a missing field is a silent feature-loss.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    title: "zsh".to_string(),
                    focused: true,
                    idle_ms: 5,
                    scroll_offset: 0,
                    alt_screen: false,
                    reverse_screen: false,
                    cwd: Some("/home/u".to_string()),
                    cols: 100,
                    rows: 32,
                    cursor_row: 10,
                    cursor_col: 5,
                    scrollback_len: 1234,
                    cursor_visible: true,
                    cursor_shape: "block".to_string(),
                    cursor_blink: true,
                    mouse_mode: "off".to_string(),
                    prompt_marks: 3,
                    command_marks: 2,
                    pid: Some(99),
                    foreground_pgid: Some(123),
                    foreground_process: Some("vim".to_string()),
                    bell_muted: true,
                    last_exit_code: Some(0),
                    progress: Some((1, 42)),
                    text: "pane-a contents".to_string(),
                    scrollback_tail: String::new(),
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    title: "vim".to_string(),
                    focused: false,
                    idle_ms: 800,
                    scroll_offset: 7,
                    alt_screen: true,
                    reverse_screen: true,
                    cwd: None,
                    cols: 80,
                    rows: 24,
                    cursor_row: 1,
                    cursor_col: 1,
                    scrollback_len: 0,
                    cursor_visible: false,
                    cursor_shape: "bar".to_string(),
                    cursor_blink: false,
                    mouse_mode: "any".to_string(),
                    prompt_marks: 0,
                    command_marks: 0,
                    pid: None,
                    foreground_pgid: None,
                    foreground_process: None,
                    bell_muted: false,
                    last_exit_code: None,
                    progress: None,
                    text: "pane-b second".to_string(),
                    scrollback_tail: String::new(),
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let arr: mlua::Table = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("list_panes")
            .unwrap()
            .call(())
            .unwrap();

        // First pane — 1-based indices, populated cwd + pid.
        let first: mlua::Table = arr.get(1).unwrap();
        assert_eq!(first.get::<u32>("tab").unwrap(), 1);
        assert_eq!(first.get::<u32>("pane").unwrap(), 1);
        assert_eq!(first.get::<String>("cwd").unwrap(), "/home/u");
        assert_eq!(first.get::<u32>("pid").unwrap(), 99);
        assert_eq!(first.get::<String>("mouse_mode").unwrap(), "off");
        // DECSCUSR-driven shape/blink — first pane is at the "block,
        // blinking" default; the second was constructed as "bar,
        // non-blinking" to pin both code paths.
        assert_eq!(first.get::<String>("cursor_shape").unwrap(), "block");
        assert!(first.get::<bool>("cursor_blink").unwrap());
        assert!(first.get::<bool>("focused").unwrap());
        assert!(!first.get::<bool>("alt_screen").unwrap());

        // Second pane — alt screen, no cwd / pid.
        let second: mlua::Table = arr.get(2).unwrap();
        assert_eq!(second.get::<u32>("tab").unwrap(), 2);
        assert_eq!(second.get::<u32>("pane").unwrap(), 3);
        assert!(second.get::<bool>("alt_screen").unwrap());
        assert_eq!(second.get::<String>("cursor_shape").unwrap(), "bar");
        assert!(!second.get::<bool>("cursor_blink").unwrap());
        let cwd: mlua::Value = second.get("cwd").unwrap();
        assert!(matches!(cwd, mlua::Value::Nil));
        let pid: mlua::Value = second.get("pid").unwrap();
        assert!(matches!(pid, mlua::Value::Nil));
        assert_eq!(second.get::<u32>("scroll_offset").unwrap(), 7);

        // Foreground process fields surface on the first pane (Linux PTY
        // path) and stay absent (nil) on the second when the backend
        // doesn't report a foreground pgid. Plugins watching for "the
        // user is running vim in pane 1" rely on this.
        assert_eq!(first.get::<u32>("foreground_pgid").unwrap(), 123);
        assert_eq!(
            first.get::<String>("foreground_process").unwrap(),
            "vim",
        );
        assert!(first.get::<bool>("bell_muted").unwrap());
        // Per-pane last_exit_code surfaces as an integer key when set
        // and is absent (nil) when no shell command has completed yet.
        assert_eq!(first.get::<i32>("last_exit_code").unwrap(), 0);
        // progress surfaces as a nested table when set.
        let prog: mlua::Table = first.get("progress").unwrap();
        assert_eq!(prog.get::<u8>("state").unwrap(), 1);
        assert_eq!(prog.get::<u8>("percent").unwrap(), 42);
        let pgid: mlua::Value = second.get("foreground_pgid").unwrap();
        assert!(matches!(pgid, mlua::Value::Nil));
        let proc: mlua::Value = second.get("foreground_process").unwrap();
        assert!(matches!(proc, mlua::Value::Nil));
        assert!(!second.get::<bool>("bell_muted").unwrap());
        let exit: mlua::Value = second.get("last_exit_code").unwrap();
        assert!(matches!(exit, mlua::Value::Nil));
        // No progress reported → key absent on the Lua side.
        let prog2: mlua::Value = second.get("progress").unwrap();
        assert!(matches!(prog2, mlua::Value::Nil));
    }

    #[test]
    fn rterm_cursor_exposes_shape_and_blink() {
        // `rterm.cursor()` now surfaces DECSCUSR shape + blink so
        // plugins (status lines, on-screen-keyboard previews) can
        // render the right cursor glyph instead of guessing.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![PaneInfo {
                tab: 0,
                pane: 0,
                cursor_row: 4,
                cursor_col: 9,
                cursor_visible: true,
                cursor_shape: "underline".to_string(),
                cursor_blink: false,
                focused: true,
                ..Default::default()
            }],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let (shape, blink): (String, bool) = host
            .lua
            .load(
                r#"
                local c = rterm.cursor()
                return c.shape, c.blink
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(shape, "underline");
        assert!(!blink);
    }

    #[test]
    fn rterm_cursor_of_returns_per_pane_cursor_or_nil() {
        // `rterm.cursor_of(tab, pane)` is the per-pane variant of
        // `rterm.cursor()`. Same `{row, col, visible}` shape; nil for
        // out-of-range pairs. Indices are 1-based to match `list_panes`.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    cursor_row: 4,
                    cursor_col: 9,
                    cursor_visible: true,
                    cursor_shape: "underline".to_string(),
                    cursor_blink: false,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    cursor_row: 17,
                    cursor_col: 3,
                    cursor_visible: false,
                    cursor_shape: "bar".to_string(),
                    cursor_blink: true,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        // Flatten in Lua: row, col, visible, shape, blink.
        let one: Vec<String> = host
            .lua
            .load(
                r#"
                local t = rterm.cursor_of(1, 1)
                return { tostring(t.row), tostring(t.col), tostring(t.visible),
                         t.shape, tostring(t.blink) }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(one, vec!["4", "9", "true", "underline", "false"]);

        // Second pane lives at (tab=1, pane=2) → 1-based (2, 3).
        let two: Vec<String> = host
            .lua
            .load(
                r#"
                local t = rterm.cursor_of(2, 3)
                return { tostring(t.row), tostring(t.col), tostring(t.visible),
                         t.shape, tostring(t.blink) }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(two, vec!["17", "3", "false", "bar", "true"]);

        let miss_is_nil: bool = host
            .lua
            .load(r#"return rterm.cursor_of(9, 9) == nil"#)
            .eval()
            .unwrap();
        assert!(miss_is_nil);
    }

    #[test]
    fn rterm_cursor_by_uid_returns_cursor_or_nil() {
        // `cursor_by_uid` mirrors `cursor_of` but indexes by stable
        // pane uid — survives reorders / pane closes.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    cursor_row: 4,
                    cursor_col: 9,
                    cursor_visible: true,
                    cursor_shape: "underline".to_string(),
                    cursor_blink: false,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    cursor_row: 17,
                    cursor_col: 3,
                    cursor_visible: false,
                    cursor_shape: "bar".to_string(),
                    cursor_blink: true,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let one: Vec<String> = host
            .lua
            .load(
                r#"
                local t = rterm.cursor_by_uid(101)
                return { tostring(t.row), tostring(t.col), tostring(t.visible),
                         t.shape, tostring(t.blink) }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(one, vec!["4", "9", "true", "underline", "false"]);

        let two: Vec<String> = host
            .lua
            .load(
                r#"
                local t = rterm.cursor_by_uid(202)
                return { tostring(t.row), tostring(t.col), tostring(t.visible),
                         t.shape, tostring(t.blink) }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(two, vec!["17", "3", "false", "bar", "true"]);

        let miss_is_nil: bool = host
            .lua
            .load(r#"return rterm.cursor_by_uid(9999) == nil"#)
            .eval()
            .unwrap();
        assert!(miss_is_nil);
    }

    #[test]
    fn rterm_size_lookup_returns_per_pane_dimensions_or_nil() {
        // `size_of(tab, pane)` and `size_by_uid(uid)` expose per-pane
        // grid dimensions. Status-line / layout plugins use these to
        // adapt output to a specific pane's width (e.g. truncate a
        // long command for a narrow split).
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    cols: 100,
                    rows: 32,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    cols: 40,
                    rows: 18,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let (c1, r1): (u16, u16) = host
            .lua
            .load(
                r#"
                local t = rterm.size_of(1, 1)
                return t.cols, t.rows
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!((c1, r1), (100, 32));

        // Second pane lives at (tab=1, pane=2) → 1-based (2, 3).
        let (c2, r2): (u16, u16) = host
            .lua
            .load(
                r#"
                local t = rterm.size_of(2, 3)
                return t.cols, t.rows
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!((c2, r2), (40, 18));

        // uid-addressed lookup matches.
        let (c3, r3): (u16, u16) = host
            .lua
            .load(
                r#"
                local t = rterm.size_by_uid(202)
                return t.cols, t.rows
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!((c3, r3), (40, 18));

        // Misses on both indexed and uid lookups → nil.
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.size_of(9, 9) == nil
                   and rterm.size_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_progress_lookup_returns_state_name_percent_or_nil() {
        // `progress_of(tab, pane)` and `_by_uid(uid)` return a
        // `{state, state_name, percent}` table mirroring the
        // shape in `list_panes()[i].progress`. Plugins watching
        // a specific build pane query this each frame for badge
        // updates without iterating the full list.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    progress: Some((1, 42)),
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    progress: Some((2, 100)), // error
                    focused: false,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 2,
                    pane: 0,
                    uid: 303,
                    progress: None,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let (s1, n1, p1): (u8, String, u8) = host
            .lua
            .load(
                r#"
                local t = rterm.progress_of(1, 1)
                return t.state, t.state_name, t.percent
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!((s1, n1.as_str(), p1), (1, "set", 42));

        let (s2, n2, p2): (u8, String, u8) = host
            .lua
            .load(
                r#"
                local t = rterm.progress_by_uid(202)
                return t.state, t.state_name, t.percent
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!((s2, n2.as_str(), p2), (2, "error", 100));

        // Pane with no progress reported → nil.
        let none: bool = host
            .lua
            .load(r#"return rterm.progress_of(3, 1) == nil"#)
            .eval()
            .unwrap();
        assert!(none);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.progress_of(9, 9) == nil
                   and rterm.progress_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_bell_muted_lookup_returns_per_pane_or_nil() {
        // Read-side companion to `set_pane_bell_muted`. Mixed
        // true/false values per pane so a wire-swap shows up.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    bell_muted: true,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    bell_muted: false,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: bool = host
            .lua
            .load(r#"return rterm.bell_muted_of(1, 1)"#)
            .eval()
            .unwrap();
        assert!(v1);
        let v2: bool = host
            .lua
            .load(r#"return rterm.bell_muted_of(2, 3)"#)
            .eval()
            .unwrap();
        assert!(!v2);
        let v2u: bool = host
            .lua
            .load(r#"return rterm.bell_muted_by_uid(202)"#)
            .eval()
            .unwrap();
        assert!(!v2u);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.bell_muted_of(9, 9) == nil
                   and rterm.bell_muted_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_last_exit_code_lookup_returns_per_pane_or_nil() {
        // Per-pane variant distinct from the window-global
        // `last_exit_code()`. Catches a regression where a
        // plugin watching many panes can't disambiguate which
        // pane an exit belongs to. Pinned both populated (incl.
        // a negative code) and nil paths.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    last_exit_code: Some(0),
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    last_exit_code: Some(-1),
                    focused: false,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 2,
                    pane: 0,
                    uid: 303,
                    last_exit_code: None, // no command finished yet
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: i32 = host
            .lua
            .load(r#"return rterm.last_exit_code_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(v1, 0);
        let v2: i32 = host
            .lua
            .load(r#"return rterm.last_exit_code_of(2, 3)"#)
            .eval()
            .unwrap();
        assert_eq!(v2, -1);
        let v2u: i32 = host
            .lua
            .load(r#"return rterm.last_exit_code_by_uid(202)"#)
            .eval()
            .unwrap();
        assert_eq!(v2u, -1);
        // Pane that has not finished a command → nil.
        let none_exit: bool = host
            .lua
            .load(r#"return rterm.last_exit_code_of(3, 1) == nil"#)
            .eval()
            .unwrap();
        assert!(none_exit);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.last_exit_code_of(9, 9) == nil
                   and rterm.last_exit_code_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_shell_pid_lookup_returns_per_pane_or_nil() {
        // `shell_pid_of(tab, pane)` and `_by_uid(uid)` —
        // per-pane shell PID. Stable for the pane's lifetime
        // (distinct from `foreground_pgid_*` which tracks the
        // running command's pgrp). Pin both populated and nil.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    pid: Some(99),
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    pid: None,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: u32 = host
            .lua
            .load(r#"return rterm.shell_pid_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(v1, 99);
        let v1u: u32 = host
            .lua
            .load(r#"return rterm.shell_pid_by_uid(101)"#)
            .eval()
            .unwrap();
        assert_eq!(v1u, 99);
        let none_pid: bool = host
            .lua
            .load(r#"return rterm.shell_pid_of(2, 3) == nil"#)
            .eval()
            .unwrap();
        assert!(none_pid);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.shell_pid_of(9, 9) == nil
                   and rterm.shell_pid_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_foreground_pgid_lookup_returns_per_pane_or_nil() {
        // `foreground_pgid_of(tab, pane)` and `_by_uid(uid)` —
        // surfaces the foreground process-group PID per pane.
        // Linux-only; backends without `tcgetpgrp` (Windows)
        // report `None`. Pin both populated and nil cases.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    foreground_pgid: Some(12345),
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    foreground_pgid: None,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: u32 = host
            .lua
            .load(r#"return rterm.foreground_pgid_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(v1, 12345);
        let v1u: u32 = host
            .lua
            .load(r#"return rterm.foreground_pgid_by_uid(101)"#)
            .eval()
            .unwrap();
        assert_eq!(v1u, 12345);
        let none_pgid: bool = host
            .lua
            .load(r#"return rterm.foreground_pgid_of(2, 3) == nil"#)
            .eval()
            .unwrap();
        assert!(none_pgid);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.foreground_pgid_of(9, 9) == nil
                   and rterm.foreground_pgid_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_foreground_process_lookup_returns_per_pane_or_nil() {
        // `foreground_process_of(tab, pane)` and `_by_uid(uid)` —
        // surfaces a per-pane "currently running" name on Linux.
        // Backends without `tcgetpgrp` (Windows) leave the field
        // `None`; the test pins both the populated and the nil
        // path so the wire-up stays in lockstep with the
        // Linux-only render fallback.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    foreground_process: Some("vim".to_string()),
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    foreground_process: None,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: String = host
            .lua
            .load(r#"return rterm.foreground_process_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(v1, "vim");
        let v2u: String = host
            .lua
            .load(r#"return rterm.foreground_process_by_uid(101)"#)
            .eval()
            .unwrap();
        assert_eq!(v2u, "vim");
        // Pane without a reported foreground process → nil.
        let none_fp: bool = host
            .lua
            .load(r#"return rterm.foreground_process_of(2, 3) == nil"#)
            .eval()
            .unwrap();
        assert!(none_fp);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.foreground_process_of(9, 9) == nil
                   and rterm.foreground_process_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_title_lookup_returns_per_pane_or_nil() {
        // `title_of(tab, pane)` and `_by_uid(uid)` mirror the
        // window-scoped `title()`. Distinct semantic: the
        // per-pane field is always a string (empty if unset),
        // never nil unless the pair is out of range.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    title: "zsh".to_string(),
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    title: "vim — main.rs".to_string(),
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: String = host
            .lua
            .load(r#"return rterm.title_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(v1, "zsh");
        let v2: String = host
            .lua
            .load(r#"return rterm.title_of(2, 3)"#)
            .eval()
            .unwrap();
        assert_eq!(v2, "vim — main.rs");
        let v3: String = host
            .lua
            .load(r#"return rterm.title_by_uid(202)"#)
            .eval()
            .unwrap();
        assert_eq!(v3, "vim — main.rs");
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.title_of(9, 9) == nil
                   and rterm.title_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_cwd_lookup_returns_per_pane_or_nil() {
        // `cwd_of(tab, pane)` and `_by_uid(uid)` mirror the focused
        // `cwd()`. nil is returned for both out-of-range and when
        // a pane has no advertised cwd yet (different semantic but
        // same observable nil — that's intentional, plugins
        // typically need to fall back the same way for both).
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    cwd: Some("/home/u/proj".to_string()),
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    cwd: None, // shell never sent one
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: String = host
            .lua
            .load(r#"return rterm.cwd_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(v1, "/home/u/proj");
        let v1u: String = host
            .lua
            .load(r#"return rterm.cwd_by_uid(101)"#)
            .eval()
            .unwrap();
        assert_eq!(v1u, "/home/u/proj");
        // Pane without an advertised cwd → nil.
        let no_cwd: bool = host
            .lua
            .load(r#"return rterm.cwd_of(2, 3) == nil"#)
            .eval()
            .unwrap();
        assert!(no_cwd);
        // Out-of-range → nil.
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.cwd_of(9, 9) == nil
                   and rterm.cwd_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_indices_of_uid_roundtrips_uid_of() {
        // The pair of (`uid_of`, `indices_of_uid`) round-trips for
        // any live pane. The test snapshot uses two panes at
        // different (tab, pane) coordinates so the byte order of
        // the returned `{tab, pane}` table is pinned.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let (t1, p1): (u32, u32) = host
            .lua
            .load(
                r#"
                local idx = rterm.indices_of_uid(101)
                return idx.tab, idx.pane
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!((t1, p1), (1, 1));
        let (t2, p2): (u32, u32) = host
            .lua
            .load(
                r#"
                local idx = rterm.indices_of_uid(202)
                return idx.tab, idx.pane
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!((t2, p2), (2, 3));
        // Round-trip via uid_of.
        let round_trip: u64 = host
            .lua
            .load(
                r#"
                local idx = rterm.indices_of_uid(202)
                return rterm.uid_of(idx.tab, idx.pane)
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(round_trip, 202);
        // Stale uid → nil.
        let miss: bool = host
            .lua
            .load(r#"return rterm.indices_of_uid(9999) == nil"#)
            .eval()
            .unwrap();
        assert!(miss);
    }

    #[test]
    fn rterm_uid_of_translates_indices_or_returns_nil() {
        // `uid_of(tab, pane)` is what plugins reach for when they
        // want a stable handle to a freshly-spawned pane before
        // sibling splits / reorders shift the index.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let u1: u64 = host
            .lua
            .load(r#"return rterm.uid_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(u1, 101);
        let u2: u64 = host
            .lua
            .load(r#"return rterm.uid_of(2, 3)"#)
            .eval()
            .unwrap();
        assert_eq!(u2, 202);
        let miss: bool = host
            .lua
            .load(r#"return rterm.uid_of(9, 9) == nil"#)
            .eval()
            .unwrap();
        assert!(miss);
    }

    #[test]
    fn rterm_dragging_tab_returns_index_or_nil() {
        // None state → nil. Some(tab) → 1-based integer.
        let host = PluginHost::new().expect("host inits");
        let nil_at_start: bool = host
            .lua
            .load(r#"return rterm.dragging_tab() == nil"#)
            .eval()
            .unwrap();
        assert!(nil_at_start);
        // Push a snapshot with dragging_tab = Some(3).
        host.set_state(TerminalState {
            dragging_tab: Some(3),
            ..TerminalState::default()
        });
        let idx: u32 = host
            .lua
            .load(r#"return rterm.dragging_tab()"#)
            .eval()
            .unwrap();
        assert_eq!(idx, 3);
    }

    #[test]
    fn rterm_is_dark_matches_theme_is_dark() {
        // The dedicated getter must agree with `rterm.theme().is_dark`
        // for any pushed bg. Test once on a dark bg and once on a
        // light bg.
        let host = PluginHost::new().expect("host inits");
        // Dark bg.
        host.set_state(TerminalState {
            theme_bg: [10, 12, 18],
            ..TerminalState::default()
        });
        let (dark_short, dark_long): (bool, bool) = host
            .lua
            .load(
                r#"
                return rterm.is_dark(), rterm.theme().is_dark
            "#,
            )
            .eval()
            .unwrap();
        assert!(dark_short);
        assert_eq!(dark_short, dark_long);
        // Light bg.
        host.set_state(TerminalState {
            theme_bg: [240, 240, 240],
            ..TerminalState::default()
        });
        let (light_short, light_long): (bool, bool) = host
            .lua
            .load(
                r#"
                return rterm.is_dark(), rterm.theme().is_dark
            "#,
            )
            .eval()
            .unwrap();
        assert!(!light_short);
        assert_eq!(light_short, light_long);
        // is_light is the exact inverse.
        let light: bool = host
            .lua
            .load(r#"return rterm.is_light()"#)
            .eval()
            .unwrap();
        assert!(light, "light bg → is_light() = true");
    }

    #[test]
    fn rterm_reverse_screen_lookup_returns_per_pane_or_nil() {
        // DECSCNM (?5) state surfaces per pane via the same
        // `_of` / `_by_uid` pair pattern as alt_screen.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    reverse_screen: true,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    reverse_screen: false,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);
        let v1: bool = host
            .lua
            .load(r#"return rterm.reverse_screen_of(1, 1)"#)
            .eval()
            .unwrap();
        assert!(v1);
        let v2u: bool = host
            .lua
            .load(r#"return rterm.reverse_screen_by_uid(202)"#)
            .eval()
            .unwrap();
        assert!(!v2u);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.reverse_screen_of(9, 9) == nil
                   and rterm.reverse_screen_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_alt_screen_lookup_returns_per_pane_or_nil() {
        // `alt_screen_of(tab, pane)` and `_by_uid(uid)` mirror the
        // focused-only `alt_screen()`. Tests both lookup forms +
        // miss-on-OOB. Distinct true/false values per pane catch
        // a wire-swap.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    alt_screen: false,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    alt_screen: true,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: bool = host
            .lua
            .load(r#"return rterm.alt_screen_of(1, 1)"#)
            .eval()
            .unwrap();
        assert!(!v1);
        let v2: bool = host
            .lua
            .load(r#"return rterm.alt_screen_of(2, 3)"#)
            .eval()
            .unwrap();
        assert!(v2);
        let v3: bool = host
            .lua
            .load(r#"return rterm.alt_screen_by_uid(202)"#)
            .eval()
            .unwrap();
        assert!(v3);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.alt_screen_of(9, 9) == nil
                   and rterm.alt_screen_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_scrollback_len_lookup_returns_per_pane_or_nil() {
        // `scrollback_len_of(tab, pane)` and `_by_uid(uid)` — per-pane
        // ring-line count. Plugins drive a "scrollback fill" badge
        // from this without iterating list_panes() each frame.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    scrollback_len: 1234,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    scrollback_len: 5678,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: u64 = host
            .lua
            .load(r#"return rterm.scrollback_len_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(v1, 1234);
        let v2: u64 = host
            .lua
            .load(r#"return rterm.scrollback_len_of(2, 3)"#)
            .eval()
            .unwrap();
        assert_eq!(v2, 5678);
        let v3: u64 = host
            .lua
            .load(r#"return rterm.scrollback_len_by_uid(202)"#)
            .eval()
            .unwrap();
        assert_eq!(v3, 5678);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.scrollback_len_of(9, 9) == nil
                   and rterm.scrollback_len_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_scroll_offset_lookup_returns_per_pane_or_nil() {
        // `scroll_offset_of(tab, pane)` and `_by_uid(uid)` mirror
        // the cursor_of / idle_of pattern. Plugins gating overlays
        // on "is this pane in scrollback?" use these without
        // sweeping `list_panes()` every frame.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    scroll_offset: 0,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    scroll_offset: 75,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: u32 = host
            .lua
            .load(r#"return rterm.scroll_offset_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(v1, 0);
        let v2: u32 = host
            .lua
            .load(r#"return rterm.scroll_offset_of(2, 3)"#)
            .eval()
            .unwrap();
        assert_eq!(v2, 75);
        let v3: u32 = host
            .lua
            .load(r#"return rterm.scroll_offset_by_uid(202)"#)
            .eval()
            .unwrap();
        assert_eq!(v3, 75);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.scroll_offset_of(9, 9) == nil
                   and rterm.scroll_offset_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_idle_lookup_returns_per_pane_ms_or_nil() {
        // `idle_of(tab, pane)` and `idle_by_uid(uid)` expose
        // `idle_ms` for any pane — the canonical signal for
        // "monitor-silence" plugins (notify when a build pane has
        // been quiet for >N ms).
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    uid: 101,
                    idle_ms: 250,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 2,
                    uid: 202,
                    idle_ms: 7500,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let v1: u64 = host
            .lua
            .load(r#"return rterm.idle_of(1, 1)"#)
            .eval()
            .unwrap();
        assert_eq!(v1, 250);
        // Second pane at (tab=1, pane=2) → 1-based (2, 3).
        let v2: u64 = host
            .lua
            .load(r#"return rterm.idle_of(2, 3)"#)
            .eval()
            .unwrap();
        assert_eq!(v2, 7500);
        let v3: u64 = host
            .lua
            .load(r#"return rterm.idle_by_uid(202)"#)
            .eval()
            .unwrap();
        assert_eq!(v3, 7500);
        let misses: bool = host
            .lua
            .load(
                r#"
                return rterm.idle_of(9, 9) == nil
                   and rterm.idle_by_uid(9999) == nil
            "#,
            )
            .eval()
            .unwrap();
        assert!(misses);
    }

    #[test]
    fn rterm_focused_pane_count_is_zero_or_one_in_steady_state() {
        // `focused_pane_count()` returns 0 (no tabs) or 1 (one
        // focused pane). Anything else is a snapshot regression.
        // The test snapshot deliberately includes one focused +
        // one non-focused pane and pins count == 1.
        let host = PluginHost::new().expect("host inits");
        let n0: u32 = host
            .lua
            .load(r#"return rterm.focused_pane_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n0, 0);
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    focused: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 0,
                    pane: 1,
                    focused: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);
        let n: u32 = host
            .lua
            .load(r#"return rterm.focused_pane_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn rterm_muted_pane_count_counts_only_muted() {
        // Mirror `alt_pane_count` for the bell-muted field.
        let host = PluginHost::new().expect("host inits");
        let n0: u32 = host
            .lua
            .load(r#"return rterm.muted_pane_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n0, 0);
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    bell_muted: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 0,
                    pane: 1,
                    bell_muted: false,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 0,
                    bell_muted: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 2,
                    pane: 0,
                    bell_muted: true,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);
        let n: u32 = host
            .lua
            .load(r#"return rterm.muted_pane_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn rterm_alt_pane_count_counts_only_alt() {
        // Mirror `unread_tab_count` / `zoomed_tab_count` for the
        // per-pane `alt_screen` field.
        let host = PluginHost::new().expect("host inits");
        let n0: u32 = host
            .lua
            .load(r#"return rterm.alt_pane_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n0, 0);
        let snapshot = TerminalState {
            panes: vec![
                PaneInfo {
                    tab: 0,
                    pane: 0,
                    alt_screen: true,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 0,
                    pane: 1,
                    alt_screen: false,
                    ..Default::default()
                },
                PaneInfo {
                    tab: 1,
                    pane: 0,
                    alt_screen: true,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);
        let n: u32 = host
            .lua
            .load(r#"return rterm.alt_pane_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn rterm_zoomed_tab_count_counts_only_zoomed() {
        // Mirror `unread_tab_count` for the `zoomed` field.
        let host = PluginHost::new().expect("host inits");
        let n0: u32 = host
            .lua
            .load(r#"return rterm.zoomed_tab_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n0, 0);
        let snapshot = TerminalState {
            tabs: vec![
                TabInfo {
                    idx: 0,
                    zoomed: true,
                    ..Default::default()
                },
                TabInfo {
                    idx: 1,
                    zoomed: false,
                    ..Default::default()
                },
                TabInfo {
                    idx: 2,
                    zoomed: true,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);
        let n: u32 = host
            .lua
            .load(r#"return rterm.zoomed_tab_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn rterm_unread_tab_count_counts_only_unread() {
        // `rterm.unread_tab_count()` — convenience sugar that
        // returns the count of tabs with `unread = true`. Pin
        // 0 (no unread), then several mixed cases.
        let host = PluginHost::new().expect("host inits");
        // Empty snapshot → 0.
        let n0: u32 = host
            .lua
            .load(r#"return rterm.unread_tab_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n0, 0);

        let snapshot = TerminalState {
            tabs: vec![
                TabInfo {
                    idx: 0,
                    unread: false,
                    ..Default::default()
                },
                TabInfo {
                    idx: 1,
                    unread: true,
                    ..Default::default()
                },
                TabInfo {
                    idx: 2,
                    unread: true,
                    ..Default::default()
                },
                TabInfo {
                    idx: 3,
                    unread: false,
                    ..Default::default()
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let n: u32 = host
            .lua
            .load(r#"return rterm.unread_tab_count()"#)
            .eval()
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn rterm_tabs_returns_one_based_indexed_entries() {
        // `rterm.tabs()` returns a 1-based array of tables — Lua's
        // natural indexing. Each entry must surface every TabInfo
        // field the plugin host advertises; missing one would break
        // status-line plugins silently.
        let host = PluginHost::new().expect("host inits");
        let snapshot = TerminalState {
            tabs: vec![
                TabInfo {
                    idx: 0,
                    focused: true,
                    pane_count: 2,
                    focused_pane: 1,
                    focused_pane_uid: 501,
                    zoomed: false,
                    custom_title: Some("editor".to_string()),
                    idle_ms: 12,
                    unread: false,
                    progress: Some((1, 73)),
                },
                TabInfo {
                    idx: 1,
                    focused: false,
                    pane_count: 1,
                    focused_pane: 0,
                    focused_pane_uid: 0,
                    zoomed: true,
                    custom_title: None,
                    idle_ms: 9999,
                    unread: true,
                    progress: None,
                },
            ],
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        let arr: mlua::Table = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("tabs")
            .unwrap()
            .call(())
            .unwrap();
        // Lua tables are 1-based — entry #1 is the first tab.
        let first: mlua::Table = arr.get(1).unwrap();
        let idx: u32 = first.get("idx").unwrap();
        assert_eq!(idx, 1, "Lua surface is 1-based");
        let title: String = first.get("custom_title").unwrap();
        assert_eq!(title, "editor");
        let uid: u64 = first.get("focused_pane_uid").unwrap();
        assert_eq!(uid, 501, "focused_pane_uid must surface on the Lua table");
        let focused: bool = first.get("focused").unwrap();
        assert!(focused);
        let unread: bool = first.get("unread").unwrap();
        assert!(!unread);

        let second: mlua::Table = arr.get(2).unwrap();
        let unread: bool = second.get("unread").unwrap();
        assert!(unread);
        let zoomed: bool = second.get("zoomed").unwrap();
        assert!(zoomed);
        // Tab with no custom_title omits the key — Lua returns nil.
        let custom: mlua::Value = second.get("custom_title").unwrap();
        assert!(matches!(custom, mlua::Value::Nil));
        // First tab's progress surfaces as a nested table; second tab
        // has no progress so the key is absent.
        let prog: mlua::Table = first.get("progress").unwrap();
        assert_eq!(prog.get::<u8>("state").unwrap(), 1);
        assert_eq!(prog.get::<u8>("percent").unwrap(), 73);
        let prog2: mlua::Value = second.get("progress").unwrap();
        assert!(matches!(prog2, mlua::Value::Nil));
    }

    #[test]
    fn rterm_platform_returns_consts() {
        // `rterm.platform()` is what plugins gate macOS-specific
        // shortcuts on (Cmd vs Ctrl). It must surface the three
        // std::env::consts strings as table fields.
        let host = PluginHost::new().expect("host inits");
        let p: mlua::Table = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("platform")
            .unwrap()
            .call(())
            .unwrap();
        let os: String = p.get("os").unwrap();
        // Must match the build target — easy assertion: just non-empty.
        assert!(!os.is_empty(), "OS string is empty");
        let family: String = p.get("family").unwrap();
        assert!(matches!(family.as_str(), "unix" | "windows" | "wasm"));
        let arch: String = p.get("arch").unwrap();
        assert!(!arch.is_empty(), "arch is empty");
        // `target_os` / `target_arch` are aliases shared with
        // `version_info()`. Confirm they always match the short
        // forms so a future refactor that diverges them surfaces
        // here rather than in plugin code.
        let target_os: String = p.get("target_os").unwrap();
        assert_eq!(target_os, os, "target_os must alias os");
        let target_arch: String = p.get("target_arch").unwrap();
        assert_eq!(target_arch, arch, "target_arch must alias arch");
        // `wsl` is a bool — false on non-Linux, derived from the kernel
        // release string on Linux. We don't pin a specific value here
        // (CI runners aren't WSL) but verify the field exists and parses.
        let wsl: bool = p.get("wsl").unwrap();
        // Sanity: outside Linux it must be false.
        if !cfg!(target_os = "linux") {
            assert!(!wsl);
        }
    }

    #[test]
    fn rterm_version_returns_crate_version() {
        // Plugins use `rterm.version()` to gate behaviour on terminal
        // version. Round-trip against CARGO_PKG_VERSION ensures the
        // string the host hands out matches what the crate metadata
        // promises — easy to break if someone hardcodes a version.
        let host = PluginHost::new().expect("host inits");
        let v: String = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("version")
            .unwrap()
            .call(())
            .unwrap();
        assert_eq!(v, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn reset_handlers_drops_every_registration() {
        // `host.reset_handlers()` is what the config-watcher calls on
        // hot-reload — without it, every `init.lua` re-execution would
        // stack more copies of the same handler, multiplying side effects
        // (a `notify` plugin would fire two/three/N times after each
        // reload). Make sure the global registry actually clears.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.on("ev", function() end)"#)
            .exec()
            .unwrap();
        let before: u64 = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("handler_count")
            .unwrap()
            .call("ev")
            .unwrap();
        assert_eq!(before, 1);
        host.reset_handlers();
        let after: u64 = host
            .lua
            .globals()
            .get::<Table>("rterm")
            .unwrap()
            .get::<Function>("handler_count")
            .unwrap()
            .call("ev")
            .unwrap();
        assert_eq!(after, 0);
    }

    #[test]
    fn add_match_substring_fires_on_contained_text() {
        // Substring mode is the default — no `opts` table. The rule fires
        // for any line containing `error` (case-sensitive) and not for
        // unrelated lines. Capture group list is always empty for substring.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(r#"rterm.add_match("err", "error")"#)
            .exec()
            .unwrap();
        assert_eq!(host.match_output_line("nothing here"), Vec::<(String, Vec<String>)>::new());
        assert_eq!(
            host.match_output_line("got an error: foo"),
            vec![("err".to_string(), Vec::<String>::new())],
        );
        // Case-sensitive: a substring rule does NOT match a different case.
        assert_eq!(host.match_output_line("Error capital"), Vec::<(String, Vec<String>)>::new());
    }

    #[test]
    fn add_match_regex_compiles_and_matches() {
        // Regex mode is opt-in via `opts.regex = true`. The pattern compiles
        // once at register time so subsequent line checks are cheap.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                local ok = rterm.add_match("warn_any", "[Ww]arn(ing)?", { regex = true })
                assert(ok, "regex rule did not register")
            "#,
            )
            .exec()
            .unwrap();
        // Group 1 is the optional "ing" — present here.
        assert_eq!(
            host.match_output_line("Warning: something"),
            vec![("warn_any".to_string(), vec!["ing".to_string()])],
        );
        // Same rule, group 1 absent (optional didn't capture) → empty string.
        assert_eq!(
            host.match_output_line("a warn shows"),
            vec![("warn_any".to_string(), vec!["".to_string()])],
        );
        assert_eq!(host.match_output_line("WARN UPPER"), Vec::<(String, Vec<String>)>::new());
    }

    #[test]
    fn add_match_rejects_invalid_regex_without_panicking() {
        // A bad regex should NOT crash the plugin host — it logs a warn and
        // returns false so the Lua side can react. The rule list stays empty.
        let host = PluginHost::new().expect("host inits");
        let ok: bool = host
            .lua
            .load(r#"return rterm.add_match("bad", "[unclosed", { regex = true })"#)
            .eval()
            .unwrap();
        assert!(!ok, "bad regex should fail to register");
        assert!(host.match_rule_names().is_empty());
    }

    #[test]
    fn add_match_rejects_empty_name() {
        // Empty rule names are useless (you can't filter or remove them by
        // name later), so guard against accidental empty-string registration.
        let host = PluginHost::new().expect("host inits");
        let ok: bool = host
            .lua
            .load(r#"return rterm.add_match("", "foo")"#)
            .eval()
            .unwrap();
        assert!(!ok);
        assert!(host.match_rule_names().is_empty());
    }

    #[test]
    fn add_match_same_name_replaces_pattern() {
        // Registering the same name twice replaces the prior pattern
        // atomically so plugins can hot-edit rules without piling up
        // stale entries.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.add_match("foo", "alpha")
                rterm.add_match("foo", "beta")
            "#,
            )
            .exec()
            .unwrap();
        assert_eq!(host.match_rule_names(), vec!["foo".to_string()]);
        assert_eq!(host.match_output_line("here is alpha"), Vec::<(String, Vec<String>)>::new());
        assert_eq!(
            host.match_output_line("here is beta"),
            vec![("foo".to_string(), Vec::<String>::new())],
        );
    }

    #[test]
    fn remove_match_deletes_rule() {
        // `rterm.remove_match(name)` returns true on success and false if
        // no such rule exists. After removal the rule no longer fires.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.add_match("a", "alpha")
                rterm.add_match("b", "beta")
                local removed_a = rterm.remove_match("a")
                local removed_x = rterm.remove_match("does_not_exist")
                assert(removed_a, "a should remove")
                assert(not removed_x, "missing rule should report false")
            "#,
            )
            .exec()
            .unwrap();
        assert_eq!(host.match_rule_names(), vec!["b".to_string()]);
        assert_eq!(host.match_output_line("alpha"), Vec::<(String, Vec<String>)>::new());
        assert_eq!(
            host.match_output_line("beta"),
            vec![("b".to_string(), Vec::<String>::new())],
        );
    }

    #[test]
    fn list_matches_returns_registered_names_in_order() {
        // `rterm.list_matches()` exposes the live rule list. Plugins use
        // this to confirm registration or to render an admin UI.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.add_match("first", "a")
                rterm.add_match("second", "b")
                rterm.add_match("third", "c")
            "#,
            )
            .exec()
            .unwrap();
        let names: Vec<String> = host
            .lua
            .load(
                r#"
                local t = rterm.list_matches()
                local out = {}
                for i, v in ipairs(t) do out[i] = v end
                return out
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn rterm_palette_color_covers_named_cube_and_grayscale() {
        // `palette_color(index)` mirrors `indexed_color_to_rgb` in
        // rterm-render: 0..=15 from the (themeable) named palette,
        // 16..=231 from the 6×6×6 cube, 232..=255 from the
        // grayscale ramp. Pin one representative from each tier so
        // a refactor on either side stays in lockstep.
        let host = PluginHost::new().expect("host inits");
        // Push a distinct named palette so the named tier is verified
        // against the pushed value rather than the default theme.
        let mut named = [[0u8; 3]; 16];
        for (i, c) in named.iter_mut().enumerate() {
            *c = [(i as u8) * 16, 0, 0];
        }
        let snapshot = TerminalState {
            named_palette: named,
            ..TerminalState::default()
        };
        host.set_state(snapshot);

        // Tier 1: named slot 5 → pushed value (5*16=80, 0, 0).
        let named_5: Vec<u8> = host
            .lua
            .load(
                r#"
                local t = rterm.palette_color(5)
                return { t[1], t[2], t[3] }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(named_5, vec![80, 0, 0]);

        // Tier 2: cube slot 16 → black corner (0,0,0).
        let cube_low: Vec<u8> = host
            .lua
            .load(
                r#"
                local t = rterm.palette_color(16)
                return { t[1], t[2], t[3] }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(cube_low, vec![0, 0, 0]);

        // Tier 2: cube slot 231 → white corner (255,255,255).
        let cube_high: Vec<u8> = host
            .lua
            .load(
                r#"
                local t = rterm.palette_color(231)
                return { t[1], t[2], t[3] }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(cube_high, vec![255, 255, 255]);

        // Tier 3: grayscale ramp endpoints — 232 → 8, 255 → 238.
        let gray_low: Vec<u8> = host
            .lua
            .load(
                r#"
                local t = rterm.palette_color(232)
                return { t[1], t[2], t[3] }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(gray_low, vec![8, 8, 8]);
        let gray_high: Vec<u8> = host
            .lua
            .load(
                r#"
                local t = rterm.palette_color(255)
                return { t[1], t[2], t[3] }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(gray_high, vec![238, 238, 238]);

        // Out-of-range index → nil.
        let oob: bool = host
            .lua
            .load(r#"return rterm.palette_color(256) == nil"#)
            .eval()
            .unwrap();
        assert!(oob);
    }

    #[test]
    fn rterm_version_info_returns_consistent_struct() {
        // `version_info()` mirrors `--version --json`. Confirm:
        //  - `version` equals `CARGO_PKG_VERSION` (the same value
        //    `rterm.version()` returns),
        //  - `profile` is "debug" when running under cfg(test)
        //    (cargo test always builds debug),
        //  - `target_os` and `target_arch` are non-empty strings.
        let host = PluginHost::new().expect("host inits");
        let (v_str, info_v, os, arch, profile): (String, String, String, String, String) = host
            .lua
            .load(
                r#"
                local short = rterm.version()
                local info = rterm.version_info()
                return short, info.version, info.target_os,
                       info.target_arch, info.profile
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(v_str, info_v, "rterm.version() must match version_info().version");
        assert_eq!(v_str, env!("CARGO_PKG_VERSION"));
        assert!(!os.is_empty());
        assert!(!arch.is_empty());
        assert_eq!(profile, "debug", "cargo test always compiles debug");
    }

    #[test]
    fn rterm_enum_name_getters_return_canonical_sets() {
        // `cursor_shape_names` and `mouse_mode_names` advertise the
        // canonical set of values used by the `pane.cursor_shape`
        // and `pane.mouse_mode` events (and their corresponding
        // `set_*` setters). The index ordering must match the
        // internal u8 encoding so plugins can round-trip
        // numeric-form payloads.
        let host = PluginHost::new().expect("host inits");
        let shapes: Vec<String> = host
            .lua
            .load(
                r#"
                local t = rterm.cursor_shape_names()
                local out = {}
                for i, v in ipairs(t) do out[i] = v end
                return out
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(shapes, vec!["block", "underline", "bar"]);

        let modes: Vec<String> = host
            .lua
            .load(
                r#"
                local t = rterm.mouse_mode_names()
                local out = {}
                for i, v in ipairs(t) do out[i] = v end
                return out
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(modes, vec!["off", "x10", "btn", "any"]);
    }

    #[test]
    fn match_rules_returns_kind_and_pattern() {
        // `rterm.match_rules()` is the rich variant of `list_matches`:
        // each entry is `{name, kind, pattern}` so plugin UIs can show
        // *what* each rule matches, not just its name.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.add_match("plain", "hello")
                rterm.add_match("re_rule", "h.*o", { regex = true })
            "#,
            )
            .exec()
            .unwrap();
        // Flatten in Lua so the Rust side only deserializes a `Vec<String>`
        // (mlua doesn't auto-impl `FromLua` for tuple types).
        let flat: Vec<String> = host
            .lua
            .load(
                r#"
                local out = {}
                for _, r in ipairs(rterm.match_rules()) do
                    out[#out + 1] = r.name
                    out[#out + 1] = r.kind
                    out[#out + 1] = r.pattern
                end
                return out
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(
            flat,
            vec![
                "plain", "substring", "hello", "re_rule", "regex", "h.*o",
            ]
        );
    }

    #[test]
    fn remove_all_matches_clears_rules_and_returns_count() {
        // `rterm.remove_all_matches()` is the bulk-clear sibling of
        // `remove_match`. It returns the dropped count so reset-style
        // plugin code can log "cleared N rules" without first
        // calling `list_matches`.
        let host = PluginHost::new().expect("host inits");
        let dropped_empty: u32 = host
            .lua
            .load(r#"return rterm.remove_all_matches()"#)
            .eval()
            .unwrap();
        assert_eq!(dropped_empty, 0, "clearing an empty registry yields 0");
        host.lua
            .load(
                r#"
                rterm.add_match("a", "x")
                rterm.add_match("b", "y")
                rterm.add_match("c", "z.*", { regex = true })
            "#,
            )
            .exec()
            .unwrap();
        assert_eq!(host.match_rule_names().len(), 3);
        let dropped: u32 = host
            .lua
            .load(r#"return rterm.remove_all_matches()"#)
            .eval()
            .unwrap();
        assert_eq!(dropped, 3);
        assert!(host.match_rule_names().is_empty());
    }

    #[test]
    fn find_match_returns_single_rule_or_nil() {
        // `rterm.find_match(name)` is the single-lookup sibling of
        // `match_rules()`: same `{name, kind, pattern}` shape, or nil
        // when the rule isn't registered.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.add_match("plain", "hello")
                rterm.add_match("re_rule", "h.*o", { regex = true })
            "#,
            )
            .exec()
            .unwrap();
        let (hit_plain_name, hit_plain_kind, hit_plain_pat): (String, String, String) = host
            .lua
            .load(
                r#"
                local r = rterm.find_match("plain")
                return r.name, r.kind, r.pattern
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(hit_plain_name, "plain");
        assert_eq!(hit_plain_kind, "substring");
        assert_eq!(hit_plain_pat, "hello");
        let (hit_re_name, hit_re_kind, hit_re_pat): (String, String, String) = host
            .lua
            .load(
                r#"
                local r = rterm.find_match("re_rule")
                return r.name, r.kind, r.pattern
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(hit_re_name, "re_rule");
        assert_eq!(hit_re_kind, "regex");
        assert_eq!(hit_re_pat, "h.*o");
        let miss_is_nil: bool = host
            .lua
            .load(r#"return rterm.find_match("nope") == nil"#)
            .eval()
            .unwrap();
        assert!(miss_is_nil);
    }

    #[test]
    fn add_match_caps_at_64_rules() {
        // 64-rule ceiling keeps per-line evaluation cost bounded. Past the
        // cap, further registrations return false until something is
        // removed (or replaces an existing slot by name).
        let host = PluginHost::new().expect("host inits");
        let last_ok: bool = host
            .lua
            .load(
                r#"
                local ok = true
                for i = 1, 64 do
                    ok = ok and rterm.add_match("r"..i, "x"..i)
                end
                return ok
            "#,
            )
            .eval()
            .unwrap();
        assert!(last_ok);
        assert_eq!(host.match_rule_names().len(), 64);
        let over: bool = host
            .lua
            .load(r#"return rterm.add_match("overflow", "y")"#)
            .eval()
            .unwrap();
        assert!(!over);
        assert_eq!(host.match_rule_names().len(), 64);
        // Replacing by-name still works (no new slot consumed).
        let replace: bool = host
            .lua
            .load(r#"return rterm.add_match("r1", "new_pattern")"#)
            .eval()
            .unwrap();
        assert!(replace);
        assert_eq!(host.match_rule_names().len(), 64);
    }

    #[test]
    fn rterm_handler_counts_returns_bulk_map() {
        // Register a mix of handlers via `rterm.on`, then assert the
        // bulk `handler_counts()` returns the right counts per event
        // and skips events with zero registrations.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.on("bell", function() end)
                rterm.on("bell", function() end)
                rterm.on("pane.output", function() end)
            "#,
            )
            .exec()
            .unwrap();
        let count_keys: i32 = host
            .lua
            .load(
                r#"
                local t = rterm.handler_counts()
                local n = 0
                for _, _ in pairs(t) do n = n + 1 end
                return n
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(count_keys, 2, "two events have at least one handler");
        let bell: usize = host
            .lua
            .load(r#"return rterm.handler_counts().bell"#)
            .eval()
            .unwrap();
        assert_eq!(bell, 2);
        let output: usize = host
            .lua
            .load(r#"return rterm.handler_counts()["pane.output"]"#)
            .eval()
            .unwrap();
        assert_eq!(output, 1);
        // Event with no handler is absent (nil), not present-with-zero.
        let absent: Option<usize> = host
            .lua
            .load(r#"return rterm.handler_counts().notification"#)
            .eval()
            .unwrap();
        assert!(absent.is_none());
    }

    #[test]
    fn nearest_palette_index_finds_exact_match_and_picks_grayscale() {
        // Use a synthetic named palette whose 16 slots are all
        // far from the test targets — that way the cube/grayscale
        // endpoints win on distance instead of colliding with a
        // default-zeroed named slot 0. (Real-world named palettes
        // hit this case too, but pinning a deliberately-distant
        // synthetic palette keeps the assertions about the
        // cube/grayscale half stable.)
        let named = {
            let mut n = [[0u8; 3]; 16];
            for (i, c) in n.iter_mut().enumerate() {
                // All mid-range pinks — guaranteed not the closest
                // to either pure black, pure white, or mid-gray.
                *c = [128, 64, 64 + (i as u8 * 4)];
            }
            n[3] = [205, 49, 49]; // "red" — one exact-match anchor
            n
        };
        // Cube white corner (slot 231).
        assert_eq!(nearest_palette_index([255, 255, 255], &named), 231);
        // Cube black corner.
        assert_eq!(nearest_palette_index([0, 0, 0], &named), 16);
        // Exact match against the named slot 3 (red anchor).
        assert_eq!(nearest_palette_index([205, 49, 49], &named), 3);
        // Mid-gray ramp anchor (128 = 8 + 12*10 = slot 244).
        assert_eq!(nearest_palette_index([128, 128, 128], &named), 244);
    }

    #[test]
    fn contrast_grade_partitions_at_4_5_and_7_0() {
        // The grade labels are what plugin status UIs render —
        // pin the cutoffs exactly so a future "we should use 4.6"
        // accident doesn't silently re-grade existing themes.
        // 4.49 → fail; 4.50 → AA; 6.99 → AA; 7.00 → AAA.
        assert_eq!(contrast_grade(1.0), "fail");
        assert_eq!(contrast_grade(4.49), "fail");
        assert_eq!(contrast_grade(4.50), "AA");
        assert_eq!(contrast_grade(6.99), "AA");
        assert_eq!(contrast_grade(7.00), "AAA");
        assert_eq!(contrast_grade(21.0), "AAA");
    }

    #[test]
    fn contrast_ratio_white_vs_black_is_max_and_self_is_one() {
        // The WCAG formula spans 1.0 (identical) to 21.0 (white vs
        // black). Pin those endpoints + a documented mid-range
        // pair so a refactor to the gamma curve can't silently
        // shift the threshold-AA / AAA cutoffs that plugins
        // assert against.
        let r = contrast_ratio([0, 0, 0], [255, 255, 255]);
        assert!((r - 21.0).abs() < 0.001, "black↔white should be 21.0, got {r}");
        // Identical colours collapse to 1.0.
        assert!((contrast_ratio([128, 128, 128], [128, 128, 128]) - 1.0).abs() < 1e-3);
        // Order-independent.
        let a = contrast_ratio([10, 12, 18], [220, 220, 220]);
        let b = contrast_ratio([220, 220, 220], [10, 12, 18]);
        assert!((a - b).abs() < 1e-4);
        // Default rterm theme (dark bg, light fg) is comfortably
        // above the AA 4.5 threshold — pin so a theme refactor
        // doesn't drop us below it without explicit intent.
        assert!(a >= 4.5, "default theme contrast {a} < AA 4.5");
    }

    #[test]
    fn luminance_is_dark_threshold_around_mid() {
        // Pin the dark / light split: pure black and very-dark
        // backgrounds are dark; pure white and pastel light backgrounds
        // are light. The boundary near ~50% grey is implementation
        // detail; we assert exact values just inside the dark / light
        // halves to keep the threshold from drifting silently.
        assert!(luminance_is_dark([0, 0, 0]));
        assert!(luminance_is_dark([10, 12, 18])); // editor dark theme bg
        assert!(luminance_is_dark([60, 60, 60]));
        assert!(!luminance_is_dark([255, 255, 255]));
        assert!(!luminance_is_dark([200, 200, 200]));
        // Pure red is mostly dark (R weighted 0.2126); pure green is
        // mostly light (G weighted 0.7152). Pin both — they're the
        // most likely surprises for users new to the BT.709 weights.
        assert!(luminance_is_dark([255, 0, 0]));
        assert!(!luminance_is_dark([0, 255, 0]));
    }

    #[test]
    fn rterm_is_search_active_round_trips_through_snapshot() {
        let host = PluginHost::new().expect("host inits");
        // Default (no push) is false.
        let v: bool = host
            .lua
            .load(r#"return rterm.is_search_active()"#)
            .eval()
            .unwrap();
        assert!(!v);
        host.set_state(TerminalState {
            search_active: true,
            ..TerminalState::default()
        });
        let v: bool = host
            .lua
            .load(r#"return rterm.is_search_active()"#)
            .eval()
            .unwrap();
        assert!(v);
    }

    #[test]
    fn rterm_search_regex_mode_round_trips() {
        let host = PluginHost::new().expect("host inits");
        // Default off.
        let on: bool = host
            .lua
            .load(r#"return rterm.search_regex_mode()"#)
            .eval()
            .unwrap();
        assert!(!on);
        host.set_state(TerminalState {
            search_active: true,
            search_regex_mode: true,
            ..TerminalState::default()
        });
        let on: bool = host
            .lua
            .load(r#"return rterm.search_regex_mode()"#)
            .eval()
            .unwrap();
        assert!(on);
    }

    #[test]
    fn rterm_search_matches_returns_current_and_total() {
        // Pushed state mirrors what the App emits while the user is
        // stepping through search results. `{0, 0}` when no matches /
        // search closed; `{idx, total}` (1-based current) otherwise.
        let host = PluginHost::new().expect("host inits");
        // Default closed.
        let cur: u32 = host
            .lua
            .load(r#"return rterm.search_matches()[1]"#)
            .eval()
            .unwrap();
        let total: u32 = host
            .lua
            .load(r#"return rterm.search_matches()[2]"#)
            .eval()
            .unwrap();
        assert_eq!((cur, total), (0, 0));
        // Active search with 3 matches, cursor on the second.
        host.set_state(TerminalState {
            search_active: true,
            search_match_index: 2,
            search_match_total: 3,
            ..TerminalState::default()
        });
        let cur: u32 = host
            .lua
            .load(r#"return rterm.search_matches()[1]"#)
            .eval()
            .unwrap();
        let total: u32 = host
            .lua
            .load(r#"return rterm.search_matches()[2]"#)
            .eval()
            .unwrap();
        assert_eq!((cur, total), (2, 3));
    }

    #[test]
    fn rterm_search_query_returns_current_string_or_empty() {
        let host = PluginHost::new().expect("host inits");
        // Default = empty.
        let q: String = host
            .lua
            .load(r#"return rterm.search_query()"#)
            .eval()
            .unwrap();
        assert_eq!(q, "");
        host.set_state(TerminalState {
            search_active: true,
            search_query: "error".to_string(),
            ..TerminalState::default()
        });
        let q: String = host
            .lua
            .load(r#"return rterm.search_query()"#)
            .eval()
            .unwrap();
        assert_eq!(q, "error");
    }

    #[test]
    fn rterm_count_family_all_return_zero_on_empty_state() {
        // The bulk-count sugars (`unread_tab_count`,
        // `zoomed_tab_count`, `alt_pane_count`, `muted_pane_count`,
        // `focused_pane_count`) must all return 0 for a freshly-
        // constructed PluginHost (no snapshot pushed yet). Catches
        // a regression where a future getter shows e.g. `1` due
        // to a wrong default predicate or an off-by-one.
        let host = PluginHost::new().expect("host inits");
        let counts: Vec<u32> = host
            .lua
            .load(
                r#"
                return {
                    rterm.unread_tab_count(),
                    rterm.zoomed_tab_count(),
                    rterm.alt_pane_count(),
                    rterm.muted_pane_count(),
                    rterm.focused_pane_count(),
                }
            "#,
            )
            .eval()
            .unwrap();
        assert_eq!(counts, vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn rterm_executable_args_returns_argv_tail() {
        // `executable_args()` returns argv[1..] as a Lua array.
        // Under `cargo test` argv is `cargo`-driven so the exact
        // contents vary by runner. The assertion is structural:
        //   - the table is iterable,
        //   - every element is a string,
        //   - the count matches `std::env::args().count() - 1`.
        let host = PluginHost::new().expect("host inits");
        let count: usize = host
            .lua
            .load(
                r#"
                local args = rterm.executable_args()
                local n = 0
                for _, v in ipairs(args) do
                    assert(type(v) == "string", "arg must be string")
                    n = n + 1
                end
                return n
            "#,
            )
            .eval()
            .unwrap();
        let expected = std::env::args().count().saturating_sub(1);
        assert_eq!(count, expected, "args count must match std::env::args tail");
    }

    #[test]
    fn rterm_executable_path_returns_non_empty_string() {
        // We don't pin the exact path — it varies by test runner — but
        // `std::env::current_exe()` always succeeds in normal test
        // contexts, so the result must be a non-empty string. This
        // catches a regression where the helper silently returns nil.
        let host = PluginHost::new().expect("host inits");
        let path: Option<String> = host
            .lua
            .load(r#"return rterm.executable_path()"#)
            .eval()
            .unwrap();
        let path = path.expect("current_exe should succeed in tests");
        assert!(!path.is_empty(), "exe path was empty");
    }

    #[test]
    fn rterm_contrast_fg_picks_white_on_dark_and_black_on_light() {
        // White text on a dark theme bg, black text on light. Uses
        // the same `luminance_is_dark` threshold as `theme().is_dark`,
        // so plugins painting a coloured badge on the live theme get
        // consistent decisions.
        let host = PluginHost::new().expect("host inits");
        let fg: Table = host
            .lua
            .load(r#"return rterm.contrast_fg({10, 12, 18})"#)
            .eval()
            .unwrap();
        assert_eq!(fg.get::<u8>(1).unwrap(), 255);
        assert_eq!(fg.get::<u8>(2).unwrap(), 255);
        assert_eq!(fg.get::<u8>(3).unwrap(), 255);
        let fg: Table = host
            .lua
            .load(r#"return rterm.contrast_fg({240, 240, 240})"#)
            .eval()
            .unwrap();
        assert_eq!(fg.get::<u8>(1).unwrap(), 0);
        assert_eq!(fg.get::<u8>(2).unwrap(), 0);
        assert_eq!(fg.get::<u8>(3).unwrap(), 0);
    }

    #[test]
    fn parse_hex_rgb_accepts_short_and_long_forms() {
        // 6-digit form with and without `#` prefix.
        assert_eq!(parse_hex_rgb("#FFAA00"), Some([0xFF, 0xAA, 0x00]));
        assert_eq!(parse_hex_rgb("ffaa00"), Some([0xFF, 0xAA, 0x00]));
        // 3-digit short form doubles each digit (CSS convention).
        assert_eq!(parse_hex_rgb("#FA0"), Some([0xFF, 0xAA, 0x00]));
        assert_eq!(parse_hex_rgb("fa0"), Some([0xFF, 0xAA, 0x00]));
        // Whitespace tolerated (paths read from user config files
        // often have stray whitespace).
        assert_eq!(parse_hex_rgb("  #FFAA00  "), Some([0xFF, 0xAA, 0x00]));
        // Wrong length → None.
        assert_eq!(parse_hex_rgb("#FF"), None);
        assert_eq!(parse_hex_rgb("#FFAA0000"), None);
        // Non-hex character → None (not garbage bytes).
        assert_eq!(parse_hex_rgb("#FFAAZZ"), None);
        // Empty → None.
        assert_eq!(parse_hex_rgb(""), None);
    }

    #[test]
    fn rterm_hex_to_rgb_round_trips_through_lua() {
        // `rterm.hex_to_rgb` mirrors the pure helper above for Lua
        // consumers. `pcall(rterm.hex_to_rgb, ...)` always succeeds;
        // malformed input surfaces as nil.
        let host = PluginHost::new().expect("host inits");
        let r: u8 = host
            .lua
            .load(r##"return rterm.hex_to_rgb("#FFAA00")[1]"##)
            .eval()
            .unwrap();
        assert_eq!(r, 0xFF);
        let g: u8 = host
            .lua
            .load(r##"return rterm.hex_to_rgb("#FA0")[2]"##)
            .eval()
            .unwrap();
        assert_eq!(g, 0xAA, "short form should double each digit");
        let nil: Option<Table> = host
            .lua
            .load(r#"return rterm.hex_to_rgb("not hex")"#)
            .eval()
            .unwrap();
        assert!(nil.is_none());
    }

    #[test]
    fn rterm_rgb_to_hex_formats_uppercase_pound_prefixed() {
        // Pair with `rterm.theme()` to emit HTML/CSS colours from a
        // status-line plugin. Pad each byte to two hex digits with
        // uppercase letters; clamp out-of-range input rather than
        // panic.
        let host = PluginHost::new().expect("host inits");
        let hex: String = host
            .lua
            .load(r#"return rterm.rgb_to_hex(255, 170, 0)"#)
            .eval()
            .unwrap();
        assert_eq!(hex, "#FFAA00");
        let hex: String = host
            .lua
            .load(r#"return rterm.rgb_to_hex(0, 0, 0)"#)
            .eval()
            .unwrap();
        assert_eq!(hex, "#000000");
        // Clamps over-range without panicking.
        let hex: String = host
            .lua
            .load(r#"return rterm.rgb_to_hex(999, 999, 999)"#)
            .eval()
            .unwrap();
        assert_eq!(hex, "#FFFFFF");
    }

    #[test]
    fn rterm_theme_includes_is_dark_flag() {
        // The `is_dark` field is the ergonomic hook plugins use to
        // pick light/dark accent colors without re-implementing
        // luminance math. Pin both branches.
        let host = PluginHost::new().expect("host inits");
        host.set_state(TerminalState {
            theme_bg: [10, 12, 18],
            ..TerminalState::default()
        });
        let is_dark: bool = host
            .lua
            .load(r#"return rterm.theme().is_dark"#)
            .eval()
            .unwrap();
        assert!(is_dark);
        host.set_state(TerminalState {
            theme_bg: [240, 240, 240],
            ..TerminalState::default()
        });
        let is_dark: bool = host
            .lua
            .load(r#"return rterm.theme().is_dark"#)
            .eval()
            .unwrap();
        assert!(!is_dark);
    }

    #[test]
    fn progress_state_name_maps_all_documented_states() {
        // Pin the OSC 9;4 state mapping so plugins relying on the
        // `state_name` string don't get blindsided by a refactor that
        // shifts the labels around. Unknown bytes fall back to a
        // stable sentinel rather than `nil`.
        assert_eq!(progress_state_name(0), "clear");
        assert_eq!(progress_state_name(1), "set");
        assert_eq!(progress_state_name(2), "error");
        assert_eq!(progress_state_name(3), "indeterminate");
        assert_eq!(progress_state_name(4), "warning");
        assert_eq!(progress_state_name(99), "unknown");
    }

    #[test]
    fn match_output_line_returns_all_matching_rule_names() {
        // A single line can satisfy multiple registered rules. They all
        // fire in registration order so the App can emit one `match`
        // event per hit (plugins can dedupe with a Lua set if they want).
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.add_match("has_foo", "foo")
                rterm.add_match("has_bar", "bar")
            "#,
            )
            .exec()
            .unwrap();
        let hits = host.match_output_line("foo and bar in one line");
        assert_eq!(
            hits,
            vec![
                ("has_foo".to_string(), Vec::<String>::new()),
                ("has_bar".to_string(), Vec::<String>::new()),
            ],
        );
    }

    #[test]
    fn add_match_regex_captures_multiple_groups() {
        // A regex with several numbered groups returns each one in order,
        // including the empty string when an optional group didn't capture.
        // Group 0 (the whole match) is intentionally omitted since the App
        // also forwards the full line alongside the captures.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"
                rterm.add_match("ip_port", "(\\d+)\\.(\\d+)\\.(\\d+)\\.(\\d+):(\\d+)", { regex = true })
            "#,
            )
            .exec()
            .unwrap();
        let hits = host.match_output_line("connected to 10.0.0.5:8080 successfully");
        assert_eq!(hits.len(), 1);
        let (name, groups) = &hits[0];
        assert_eq!(name, "ip_port");
        assert_eq!(
            groups,
            &vec![
                "10".to_string(),
                "0".to_string(),
                "0".to_string(),
                "5".to_string(),
                "8080".to_string(),
            ],
        );
    }

    #[test]
    fn add_match_regex_captures_only_first_occurrence_per_line() {
        // We only return the FIRST match per rule per line — multiple
        // hits on the same line would otherwise require event handlers to
        // dedupe by source position. Plugins that want all matches can
        // re-evaluate the regex Lua-side using the captures we provide.
        let host = PluginHost::new().expect("host inits");
        host.lua
            .load(
                r#"rterm.add_match("num", "(\\d+)", { regex = true })"#,
            )
            .exec()
            .unwrap();
        let hits = host.match_output_line("first=11 second=22 third=33");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "num");
        assert_eq!(hits[0].1, vec!["11".to_string()]);
    }
}
