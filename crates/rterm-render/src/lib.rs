//! GPU renderer + winit application for rterm.

mod bg;
mod image_decode;
mod image_pass;

/// Quick "does this byte payload decode as an image?" check —
/// used by the auto-detect parser to fall back to text output
/// when its accumulated body turns out to not actually be a
/// displayable image (corrupt header, unsupported variant,
/// half-stream from a partial download). Calls into the same
/// decoder the GPU upload path uses, so any format the
/// renderer can paint is one the validator accepts.
///
/// Returns `false` on any decode error including a too-large
/// payload that would have OOM'd. Logs are emitted by the
/// decoder itself at WARN level.
pub fn validates_as_image(bytes: &[u8]) -> bool {
    image::load_from_memory(bytes).is_ok()
}
pub mod palette;
pub mod highlight;
pub(crate) mod tree;
pub mod action;
pub use action::AppAction;
pub mod events;
pub use events::{EventSink, NullSink};
mod clipboard;
use clipboard::clipboard_set;
mod command_capture;
mod global_hotkey;
mod paste_confirm;
mod suggestion_popup;
mod keybind;
mod layout;
mod window_ops;
mod overlay;
pub use keybind::UserBinding;
use keybind::KeyMatch;
#[cfg(test)]
use keybind::parse_key_spec;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use glyphon::{
    cosmic_text::Wrap, Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics,
    Resolution, Shaping, Style, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer,
    Viewport, Weight,
};
use rterm_core::{Cell, CellAttrs, Color as TermColor, MouseTracking, Size as TermSize, Terminal};

use crate::bg::BgLayer;
use crate::palette::{color_to_rgb, default_bg, default_fg};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent};
use winit::dpi::PhysicalPosition;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, ResizeDirection, Window, WindowAttributes, WindowId};

pub use winit;

pub type SharedTerminal = Arc<Mutex<Terminal>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
struct StyleKey {
    fg: [u8; 3],
    /// Reserved for the eventual background-quad pass; not used by glyphon
    /// today (glyphon paints glyphs only).
    bg: [u8; 3],
    bold: bool,
    italic: bool,
}

impl StyleKey {
    fn from_cell(
        cell: &Cell,
        is_cursor: bool,
        is_selected: bool,
        reverse_screen: bool,
    ) -> Self {
        // Resolve the cell's foreground, applying the "bold brightens" rule
        // before any inversion. We only auto-brighten the eight non-bright
        // named colours; bright variants and explicit RGB/indexed stay put.
        let fg_color = if cell.attrs.contains(CellAttrs::BOLD)
            && palette::bold_is_bright()
        {
            match cell.fg {
                TermColor::Named(n) if (n as u8) < 8 => {
                    TermColor::Named(brighten_named(n))
                }
                other => other,
            }
        } else {
            cell.fg
        };
        let mut fg = color_to_rgb(fg_color, default_fg());
        let mut bg = color_to_rgb(cell.bg, default_bg());

        // REVERSE attribute, cursor, selection, and DECSCNM
        // (?5 reverse-screen) all invert fg/bg. Even count
        // cancels (e.g. SGR REVERSE + DECSCNM = no net swap).
        let inverts = (cell.attrs.contains(CellAttrs::REVERSE) as u8)
            + is_cursor as u8
            + is_selected as u8
            + reverse_screen as u8;
        if inverts % 2 == 1 {
            std::mem::swap(&mut fg, &mut bg);
        }

        // Hyperlinks with default fg get a recognisable accent blue
        // so the user can spot what is clickable. Applied AFTER
        // the inversion swap so the accent survives `REVERSE` and
        // DECSCNM ?5; otherwise the blue would get sent to the
        // `bg` slot and lost (StyleKey only paints the glyph
        // colour — `bg` is reserved). Explicit fg from SGR is
        // honoured: the override only fires when the cell's
        // declared fg is `Default`.
        if cell.hyperlink != 0 && matches!(cell.fg, TermColor::Default) {
            fg = [86, 156, 214];
        }
        Self {
            fg,
            bg,
            bold: cell.attrs.contains(CellAttrs::BOLD),
            italic: cell.attrs.contains(CellAttrs::ITALIC),
        }
    }

    fn into_attrs(self, family: Family<'static>) -> Attrs<'static> {
        let mut a = Attrs::new()
            .family(family)
            .color(GlyphColor::rgb(self.fg[0], self.fg[1], self.fg[2]));
        if self.bold {
            a = a.weight(Weight::BOLD);
        }
        if self.italic {
            a = a.style(Style::Italic);
        }
        a
    }
}

/// Build (text, style-key) spans for the rows of `terminal` visible at
/// `offset`, collapsing consecutive same-styled cells. The cursor is drawn
/// only when `focused` AND `offset == 0` AND the terminal has cursor visible
/// AND `blink_on` (or the cursor is "steady"). Glyph inversion is applied
/// only for the Block cursor shape; thin shapes draw on top of the glyph.
/// Where `query.truncate(...)` should land so that a readline-style
/// Ctrl+W (delete-the-trailing-word) drops just the last word from
/// `query`. Returns a byte index that is always on a UTF-8 char
/// boundary.
///
/// Algorithm: trim trailing whitespace (a Ctrl+W on `"foo bar  "`
/// deletes `bar`, not the empty word after the spaces), then find the
/// last whitespace char in what's left and keep everything up to and
/// including that whitespace. If no whitespace remains the whole
/// query is dropped.
///
/// Subtlety: `rfind` returns the byte index of the *first byte* of
/// the matched whitespace char, and `char::is_whitespace` matches
/// every Unicode whitespace — including multi-byte ones like NBSP
/// (U+00A0, 2 bytes) or U+3000 (3 bytes). The old code used `i + 1`,
/// which on NBSP landed inside the char and made `String::truncate`
/// panic. `rmatch_indices` hands us the matched substring so its byte
/// length is exact.
fn word_back_delete_index(query: &str) -> usize {
    let trimmed = query.trim_end();
    trimmed
        .rmatch_indices(char::is_whitespace)
        .next()
        .map(|(i, m)| i + m.len())
        .unwrap_or(0)
}

/// Map an unbright named colour to its bright variant. Caller has already
/// confirmed the input is one of `Black..=White` (indices 0..=7).
fn brighten_named(n: rterm_core::NamedColor) -> rterm_core::NamedColor {
    use rterm_core::NamedColor::*;
    match n {
        Black => BrightBlack,
        Red => BrightRed,
        Green => BrightGreen,
        Yellow => BrightYellow,
        Blue => BrightBlue,
        Magenta => BrightMagenta,
        Cyan => BrightCyan,
        White => BrightWhite,
        other => other,
    }
}

fn build_spans(
    terminal: &Terminal,
    offset: u16,
    focused: bool,
    blink_on: bool,
    selection: Option<&NormSelection>,
) -> Vec<(String, StyleKey)> {
    let rows = terminal.size().rows;
    let cols = terminal.size().cols;
    let reverse_screen = terminal.is_reverse_screen();
    let blink_state = blink_on || !terminal.cursor_should_blink();
    let cursor_active = focused
        && offset == 0
        && terminal.cursor_visible()
        && blink_state
        && matches!(terminal.cursor_shape(), rterm_core::CursorShape::Block);
    let cursor = terminal.cursor();
    // Clamp the cursor column the SAME way the bg-quad pass does
    // (`cursor.col.min(cols-1)`). After printing into the last column
    // with autowrap pending, `cursor.col == cols` (one past the grid);
    // the bg pass paints the block on the last cell, but an unclamped
    // `== c` test here matched no cell, leaving that glyph un-inverted
    // (invisible against the block). Clamping inverts the right cell.
    let cursor_col = (cursor.col).min(cols.saturating_sub(1));
    // Client-side syntax highlighting (WindTerm-style). Nothing runs
    // when no rules are configured.
    let hl = highlight::active();
    let hl_active = hl.is_active();
    let mut spans: Vec<(String, StyleKey)> = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let Some(row) = terminal.visible_row(offset, r) else { continue };
        // Per-column highlight overrides for this row. Built by running
        // the rules over the row's logical text (spacers excluded), then
        // applied below ONLY to default-fg, non-inverted cells so the
        // shell's own colours / selection / cursor always win.
        let mut hl_overrides: Vec<Option<highlight::HStyle>> = Vec::new();
        if hl_active {
            hl_overrides = vec![None; row.len()];
            let mut text = String::new();
            let mut char_cols: Vec<u16> = Vec::with_capacity(row.len());
            for (c, cell_ref) in row.iter().enumerate() {
                if cell_ref.attrs.contains(CellAttrs::WIDE_SPACER) {
                    continue;
                }
                text.push(cell_ref.ch);
                char_cols.push(c as u16);
            }
            hl.overlay(&text, &char_cols, &mut hl_overrides);
        }
        let mut current_text = String::new();
        let mut current_key: Option<StyleKey> = None;
        for (c, cell_ref) in row.iter().enumerate() {
            let cell = *cell_ref;
            // Skip spacers — the preceding WIDE glyph visually covers them.
            if cell.attrs.contains(CellAttrs::WIDE_SPACER) {
                continue;
            }
            let is_cursor =
                cursor_active && cursor.row == r && cursor_col as usize == c;
            let is_selected = selection
                .map(|s| s.contains(r, c as u16))
                .unwrap_or(false);
            let mut key = StyleKey::from_cell(&cell, is_cursor, is_selected, reverse_screen);
            // Overlay a highlight colour only on plain cells: default
            // foreground, no SGR reverse, not under the cursor, not
            // selected, and not on a reverse-screen. That keeps
            // highlighting purely additive over `ls`/`bat`/`git` output
            // and never disturbs a selection or the cursor block.
            if hl_active
                && matches!(cell.fg, TermColor::Default)
                && !is_cursor
                && !is_selected
                && !reverse_screen
                && !cell.attrs.contains(CellAttrs::REVERSE)
            {
                if let Some(hs) = hl_overrides.get(c).copied().flatten() {
                    key.fg = hs.fg;
                    key.bold = key.bold || hs.bold;
                }
            }
            if current_key == Some(key) {
                current_text.push(cell.ch);
            } else {
                if let Some(k) = current_key {
                    if !current_text.is_empty() {
                        spans.push((std::mem::take(&mut current_text), k));
                    }
                }
                current_text.push(cell.ch);
                current_key = Some(key);
            }
        }
        if let Some(k) = current_key {
            if !current_text.is_empty() {
                spans.push((current_text, k));
            }
        }
        if r + 1 < rows {
            spans.push(("\n".to_string(), StyleKey::default()));
        }
    }
    spans
}


/// Drain the pending OSC 52 clipboard write from every `(uid,
/// terminal)` pair and return the one belonging to `focused_uid`.
/// EVERY pane is drained regardless of focus — so a background pane
/// can't accumulate a pending write and then flush a stale clipboard
/// overwrite when it later gains focus — but only the focused pane's
/// write is returned to apply; the rest are dropped.
fn drain_osc52<'a>(
    panes: impl Iterator<Item = (u64, &'a SharedTerminal)>,
    focused_uid: Option<u64>,
) -> Option<String> {
    let mut focused: Option<String> = None;
    for (uid, term) in panes {
        let pending = term.lock().ok().and_then(|mut t| t.take_pending_clipboard());
        if let Some(b64) = pending {
            if Some(uid) == focused_uid {
                focused = Some(b64);
            } else {
                tracing::debug!(uid, "dropped OSC 52 clipboard write from a non-focused pane");
            }
        }
    }
    focused
}

/// Glue trait so the renderer can write input bytes and resize the PTY
/// without depending on the rterm-pty crate directly.
pub trait TerminalIo: Send + Sync {
    fn write_input(&self, bytes: &[u8]);
    fn resize(&self, cols: u16, rows: u16);
    /// OS process id of the shell running in this pane, if available.
    /// Default returns `None` for embedders without a real PTY (e.g. tests).
    fn process_id(&self) -> Option<u32> {
        None
    }
    /// PID of the foreground process group currently attached to this
    /// pane's PTY — e.g. `vim` when the user is editing, `bash` when
    /// sitting at a prompt. Returns `None` on backends that don't expose
    /// this (Windows / non-real-PTY embedders). Cheap call site (one
    /// ioctl on Unix), so the renderer can poll it per frame.
    fn foreground_pgid(&self) -> Option<u32> {
        None
    }
}

/// One restored tab entry — cwd and optional custom title.
#[derive(Debug, Clone, Default)]
pub struct RestoredTab {
    pub cwd: Option<String>,
    pub title: Option<String>,
}

/// Snapshot of the focused pane's terminal pushed to plugins each frame so
/// Lua callbacks can read fresh state via `rterm.cwd()` etc.
#[derive(Debug, Clone, Default)]
pub struct TerminalSnapshot {
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub panes: Vec<PaneSnapshotInfo>,
    pub tabs: Vec<TabSnapshotInfo>,
    /// Focused pane's visible grid text — used by `rterm.terminal_text()`.
    pub grid_text: String,
    /// Current font size in points (for `rterm.font_size()`).
    pub font_size: f32,
    /// Resolved font family name actually in use. Either the user's
    /// explicit `font.family`, the auto-picked monospace from
    /// `default_monospace_family()`, or `""` when cosmic-text's built-in
    /// fallback is the only choice. Surfaced via `rterm.font_family()`.
    pub font_family: String,
    /// Pixel width of one cell — `rterm.cell_width()`. Equals the probed
    /// monospace advance of the resolved family at the current size.
    pub cell_width: f32,
    /// Pixel height between baselines — `rterm.line_height()`. Useful for
    /// plugins drawing absolute-positioned overlays.
    pub line_height: f32,
    /// Current `terminal.tab_silence_ms` threshold (`rterm.tab_silence_ms()`).
    /// `0` means the `tab.silence`/`pane.silence` events are disabled.
    pub tab_silence_ms: u64,
    /// Current `terminal.slow_command_ms` threshold
    /// (`rterm.slow_command_ms()`). `0` disables the
    /// `pane.slow_command` event.
    pub slow_command_ms: u64,
    /// Whether new output snaps the viewport back to the live grid
    /// (`rterm.scroll_on_output()`).
    pub scroll_on_output: bool,
    /// Whether the scrollbar overlay is drawn (`rterm.show_scrollbar()`).
    pub show_scrollbar: bool,
    /// Whether BEL flashes the focused pane (`rterm.bell_visual()`).
    pub bell_visual: bool,
    /// Whether BEL pings the taskbar when unfocused
    /// (`rterm.bell_urgent()`).
    pub bell_urgent: bool,
    /// Global cursor-blink config flag (`rterm.cursor_blink()`).
    /// Distinct from `PaneSnapshotInfo.cursor_blink`, which tracks
    /// per-pane DECSCUSR. This is the user-config / hot-reload
    /// override — apps that want to honour the "I disabled cursor
    /// blink globally" preference watch this.
    pub cursor_blink: bool,
    /// The 16 named ANSI palette slots (index 0..=15) currently
    /// active. Reflects `init_palette` + any runtime OSC 4 /
    /// OSC 104 updates. Exposed via `rterm.named_palette()` so
    /// theme-aware plugins (status lines, screenshot tools) can
    /// render swatches without re-deriving them.
    pub named_palette: [[u8; 3]; 16],
    /// 1-based index of the tab the user is currently dragging,
    /// or `None` when no drag is in progress. Plugins react to
    /// this (e.g. dim a "your config will be saved" indicator
    /// while a drag is in flight). Exposed as
    /// `rterm.dragging_tab()` returning nil when absent.
    pub dragging_tab: Option<u32>,
    /// Scrollback ring capacity for the focused pane (`rterm.scrollback_limit()`).
    pub scrollback_limit: usize,
    /// Text under the user's current mouse selection, or `None` if there
    /// is no active drag-selection. Surfaced via `rterm.selection()` so
    /// plugins can read the highlighted text without waiting for a
    /// `selection.end` event.
    pub selection_text: Option<String>,
    /// Window opacity in 0.0..=1.0 (for `rterm.opacity()`).
    pub opacity: f32,
    /// Whether the window has keyboard focus (`rterm.window_focused()`).
    pub window_focused: bool,
    /// Most recent shell exit code captured via OSC 133;D.
    pub last_exit_code: Option<i32>,
    /// Focused pane's OSC 133;A prompt marks (logical line indices).
    pub prompt_mark_lines: Vec<usize>,
    /// Focused pane's OSC 133;C command-start marks.
    pub command_mark_lines: Vec<usize>,
    /// Live default foreground / background / cursor colours (RGB bytes).
    /// Surfaced via `rterm.theme()` so status-line plugins can match the
    /// running palette without parsing config or duplicating defaults.
    pub theme_fg: [u8; 3],
    pub theme_bg: [u8; 3],
    pub theme_cursor: [u8; 3],
    /// Focused pane's recent scrollback as a `\n`-joined string, capped
    /// at the renderer's `SCROLLBACK_SNAPSHOT_MAX` lines (most-recent
    /// kept). Empty when no scrollback exists yet or the pane is on the
    /// alt screen. Surfaced via `rterm.scrollback_text()`.
    pub scrollback_text: String,
    /// True when the search overlay is open. Surfaced via
    /// `rterm.is_search_active()` so status-line plugins can branch on
    /// search mode (e.g. dim other widgets while the user is searching).
    pub search_active: bool,
    /// Current search query text, empty when not searching or query is
    /// blank. Surfaced via `rterm.search_query()` so status-line
    /// plugins can render "(searching: `<query>`)" badges.
    pub search_query: String,
    /// `(current_1based, total)` for the search overlay's match cursor.
    /// `(0, 0)` when search is closed or has no matches. Surfaced via
    /// `rterm.search_matches()` so status-line plugins can mirror the
    /// in-overlay `n/m` counter.
    pub search_match_index: u32,
    pub search_match_total: u32,
    /// True when search is active AND the user has toggled regex mode
    /// (Ctrl+R inside the overlay). False otherwise. Surfaced via
    /// `rterm.search_regex_mode()`.
    pub search_regex_mode: bool,
    /// Canonical name of the active built-in theme (e.g. `"dracula"`).
    /// Empty when the palette was hand-rolled via `set_palette` or
    /// `[colors]` overrides without matching a built-in.
    pub active_theme: String,
}

#[derive(Debug, Clone, Default)]
pub struct TabSnapshotInfo {
    pub idx: usize,
    pub focused: bool,
    pub pane_count: usize,
    pub focused_pane: usize,
    /// Stable uid of the focused pane within this tab. `0` when there
    /// is no focused pane (empty tab — transient).
    pub focused_pane_uid: u64,
    pub zoomed: bool,
    pub custom_title: Option<String>,
    /// Milliseconds since the most recently-active pane in this tab last
    /// produced output (i.e. `min` over `idle_ms` of the tab's panes). `0`
    /// while any pane is actively printing; `u64::MAX` if no pane has ever
    /// produced output (uninitialised reader).
    pub idle_ms: u64,
    /// True when this non-focused tab has produced output since it was last
    /// focused; cleared on focus.
    pub unread: bool,
    /// Aggregate OSC 9;4 progress across this tab's panes (`(state,
    /// percent)`). The severity ladder is `error > warn > indeterminate
    /// > set`; ties on severity break by largest percent. `None` when
    /// no pane in the tab has an active report. Plugins use this to
    /// render a tab-strip badge without iterating `list_panes()`.
    pub progress: Option<(u8, u8)>,
}

/// One pane's snapshot for `rterm.list_panes()`.
#[derive(Debug, Clone, Default)]
pub struct PaneSnapshotInfo {
    pub tab: usize,
    pub pane: usize,
    /// Stable identifier from `Pane::uid` — survives reorders / closes
    /// so plugins can address a specific pane long-term.
    pub uid: u64,
    pub title: String,
    pub focused: bool,
    /// Milliseconds elapsed since this pane's PTY last produced output.
    /// `0` means output happened this frame or the reader hasn't started.
    pub idle_ms: u64,
    /// Lines scrolled up into scrollback (0 = following the live grid).
    pub scroll_offset: u16,
    /// True when the pane is currently on the alternate screen (vim, less,
    /// htop, etc.). Plugins can use it to gate output-capture features.
    pub alt_screen: bool,
    /// True when DECSCNM (?5) is set — the renderer is drawing
    /// the pane with default fg/bg swapped. Apps occasionally
    /// flash the screen this way; status-line plugins can read
    /// it to e.g. invert their own overlay glyph.
    pub reverse_screen: bool,
    /// Working directory most recently advertised by the pane's shell via
    /// OSC 7. `None` when the shell hasn't sent one yet.
    pub cwd: Option<String>,
    /// Current grid columns / rows of the pane's terminal.
    pub cols: u16,
    pub rows: u16,
    /// 1-based cursor position inside the pane's terminal grid.
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// Number of lines retained in the pane's scrollback ring.
    pub scrollback_len: usize,
    /// DEC ?25 cursor visibility for this pane.
    pub cursor_visible: bool,
    /// Cursor shape from the most recent DECSCUSR: `"block"`,
    /// `"underline"`, or `"bar"`. Defaults to `"block"`.
    pub cursor_shape: &'static str,
    /// Whether the cursor should blink (DECSCUSR low bit). Toggled
    /// together with `cursor_shape`; surfaced separately so plugins
    /// can render a static cursor preview without losing the shape.
    pub cursor_blink: bool,
    /// Mouse tracking mode: `"off"`, `"x10"`, `"btn"`, or `"any"`.
    pub mouse_mode: &'static str,
    /// Count of OSC 133;A prompt marks accumulated in this pane.
    pub prompt_marks: usize,
    /// Count of OSC 133;C command-start marks accumulated in this pane.
    pub command_marks: usize,
    /// OS process id of the shell running in this pane, if the underlying
    /// PTY backend reports one. Stable for the pane's lifetime.
    pub pid: Option<u32>,
    /// PID of the *current* foreground process group on the PTY (changes as
    /// the user runs commands inside the shell). `None` on backends without
    /// `tcgetpgrp` support (Windows). Re-read every frame.
    pub foreground_pgid: Option<u32>,
    /// Executable name (`comm`, ≤ 15 bytes) of the foreground process group
    /// leader. Linux-only — read from `/proc/<pgid>/comm` each frame. Useful
    /// to display "what is running in this pane right now" when no shell
    /// integration title is being emitted. `None` if `foreground_pgid` is
    /// unknown or `/proc` is unavailable.
    pub foreground_process: Option<String>,
    /// Whether bells from this pane are currently muted. Toggled by
    /// `rterm.set_pane_bell_muted`. When `true`, BEL doesn't fire the
    /// `bell` plugin event or the visual flash.
    pub bell_muted: bool,
    /// Most recent OSC 133;D exit code for the shell in this pane.
    /// `None` until the shell finishes its first command.
    pub last_exit_code: Option<i32>,
    /// Most recent OSC 9;4 progress as `(state, percent)`. `None` when
    /// the pane has not received a progress report (or received a
    /// state=0 "clear"). State 1=set, 2=error, 3=indeterminate, 4=warn.
    pub progress: Option<(u8, u8)>,
    /// Visible grid text for this pane, trimmed and `\n`-joined the same
    /// way as `TerminalSnapshot::grid_text`. Exposed to Lua via
    /// `rterm.terminal_text(tab, pane)`; building it eagerly here means
    /// the Lua call is O(1) instead of plumbing a cross-thread fetch.
    pub text: String,
    /// Capped per-pane scrollback tail (`SCROLLBACK_TAIL_MAX` lines,
    /// `\n`-joined). Empty when the pane is on the alt screen (the
    /// scrollback ring belongs to the suspended primary screen).
    /// Exposed to Lua via `rterm.scrollback_text_of(tab, pane, ...)`.
    pub scrollback_tail: String,
}


#[derive(Debug, Clone, Copy)]
pub struct PaneRect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct NormSelection {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
    /// `true` for rectangular (block) selection — `Alt+drag`. Each
    /// row inside `[start.row, end.row]` is independently clipped to
    /// `[min(start.col, end.col), max(start.col, end.col)]`. `false`
    /// = the usual linear (char/word/line) flow.
    pub block: bool,
}

impl NormSelection {
    pub fn contains(&self, row: u16, col: u16) -> bool {
        if row < self.start.row || row > self.end.row {
            return false;
        }
        if self.block {
            // Rect bounds: the inclusive column range is independent
            // of which row we're on. `start.col <= end.col` is held
            // by every constructor (normalised on creation).
            return col >= self.start.col && col < self.end.col;
        }
        if row == self.start.row && col < self.start.col {
            return false;
        }
        if row == self.end.row && col >= self.end.col {
            return false;
        }
        true
    }
}

pub struct PaneDraw<'a> {
    pub terminal: &'a Terminal,
    pub scroll_offset: u16,
    pub focused: bool,
    pub rect: PaneRect,
    pub selection: Option<NormSelection>,
    /// Cursor blink phase. When false, the cursor block is suppressed even if
    /// the terminal's DECTCEM says it should be visible.
    pub blink_on: bool,
    /// Pane's unique identifier — used by the image pass to
    /// namespace its texture cache. Image IDs are monotonic
    /// per-pane (every `Terminal` owns its own counter), so we
    /// need `(pane_uid, image_id)` to avoid one pane's image
    /// accidentally aliasing another's in the GPU cache.
    pub pane_uid: u64,
}

pub struct TextLayer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    /// glyphon's shared cache. Not read directly, but its `Drop` is
    /// load-bearing: it owns GPU buffers / atlas pages that the
    /// `TextRenderer`s alias. Don't remove this field "because it's
    /// unused" — the `Drop` order pin matters on Wayland shutdown.
    _glyph_cache: Cache,
    viewport: Viewport,
    atlas: TextAtlas,
    /// Pane / header glyph pass — drawn BEFORE the overlay backdrop so
    /// menu chrome can cover pane glyphs that would otherwise bleed
    /// through the modal.
    renderer: TextRenderer,
    /// Overlay glyph pass — drawn AFTER both `renderer` and the
    /// overlay-bg layer so the menu text sits on top of its panel.
    overlay_renderer: TextRenderer,
    buffers: Vec<Buffer>,
    header_buffer: Buffer,
    /// Right-anchored mini-buffer for the minimize / maximize / close
    /// glyphs that float at the trailing edge of the header. Living in
    /// its OWN buffer lets us position it pixel-exactly via the
    /// `TextArea.left` offset — much more reliable than padding the
    /// main header text with spaces and hoping cosmic-text's widths
    /// match `unicode_width`.
    header_right_buffer: Buffer,
    /// Tab-strip glyph buffer. Lives separately from `header_buffer`
    /// (which carries the hamburger + new-tab chrome that DOESN'T
    /// scroll) so we can position its TextArea independently and
    /// have the tab labels physically slide with `tab_scroll_offset`.
    /// Without this split, scrolling moved the chip-fill quads but
    /// left the glyph text anchored to the header left — labels and
    /// chips drifted apart visibly.
    header_tabs_buffer: Buffer,
    /// Cursor-following ghost label rendered ONLY while a tab drag
    /// is in flight. The ghost CHIP background is drawn via
    /// `tab_bar_quads` at `cursor.x - press_offset`, but the label
    /// text used to stay parked at the source slot (just dimmed) —
    /// reads as a disconnected drop-shadow. This buffer carries
    /// just the dragged tab's label and its TextArea is positioned
    /// to land exactly on top of the ghost chip, closing the visual
    /// gap. Empty buffer when no drag is active.
    header_tabs_ghost_buffer: Buffer,
    /// Window-title text in the centre of the top-row title bar
    /// (VSCode pattern). Separate from `header_buffer` so its position
    /// can be derived independently of the tab strip's width.
    title_bar_buffer: Buffer,
    /// Bottom-of-window status bar text — shell name, cwd, pane count.
    status_bar_buffer: Buffer,
    overlay_buffer: Buffer,
    font_size: f32,
    line_height: f32,
    cell_width: f32,
    /// Leaked at `TextLayer::new` so we can hand out `&'static str` to
    /// `glyphon::Family::Name(...)` — `Attrs<'static>` flows into
    /// `Buffer::set_rich_text` which stores spans of `Attrs<'static>`.
    /// We tried switching to `Arc<str>` but every span call site
    /// resists the lifetime relaxation (over 40 cascade errors), and
    /// rebuilding the spans + attrs API is well past LOW-priority
    /// scope. A hot-reload of `font.family` leaks the previous name —
    /// on the order of tens of bytes per change.
    font_family: &'static str,
}

/// A run of styled text fed to the GPU text renderer. The triple is
/// `(text, foreground RGB, bold)`. Borrowed-against-storage so the
/// renderer doesn't allocate per-span strings on the hot path.
pub type SpanList<'a> = Vec<(&'a str, [u8; 3], bool)>;

/// A span list anchored to a specific pixel rect — what every chrome
/// builder (`*_spans`) hands back. `None` means the caller has nothing
/// to render for this frame. Using one alias here also kills the
/// `#[allow(clippy::type_complexity)]` boilerplate that previously
/// wrapped every `*_spans` signature.
type AnchoredSpans<'a> = Option<(SpanList<'a>, PaneRect)>;

/// Tab-bar style header rendered above the panes.
pub struct HeaderDraw<'a> {
    pub spans: SpanList<'a>, // (text, fg rgb, bold)
    pub rect: PaneRect,
    /// Right-side clip — header glyphs never paint past this x. Used
    /// to keep long tab labels from overflowing into the window-
    /// control strip (the trailing minimize / maximize / close
    /// cluster shares the same row as the tabs). When `None`, the
    /// clip falls back to `rect.left + rect.width` (the full header
    /// width — no extra reserve).
    pub right_clip: Option<f32>,
}

/// Tab-label glyphs rendered as a separate buffer so their horizontal
/// position can slide with the user's wheel scroll, independent of
/// the static header chrome (hamburger / new-tab `+`). Without this
/// split the chip-fill quads moved while the label glyphs stayed
/// anchored at `header.left`, drifting noticeably as the strip
/// scrolled.
pub struct HeaderTabsDraw<'a> {
    pub spans: SpanList<'a>,
    /// Pixel x of the left edge of the FIRST tab chip (already
    /// shifted by `-tab_scroll_offset`).
    pub left: f32,
    pub top: f32,
    /// Layout width the buffer can use before truncation (= the
    /// strip's visible span between hamburger and the controls
    /// reserve; `right_clip - left`).
    pub width: f32,
    pub height: f32,
    /// Hard pixel clip on the right — same boundary used by
    /// `HeaderDraw.right_clip` so labels don't paint over the
    /// window-control cluster.
    pub right_clip: f32,
    /// Hard pixel clip on the LEFT — set to `hamburger_end` so a
    /// tab that's scrolled off-screen-left doesn't paint over the
    /// hamburger glyph.
    pub left_clip: f32,
}

/// Cursor-following ghost label rendered ONLY while a tab drag is in
/// flight. Paired with the ghost chip background that `tab_bar_quads`
/// emits at the same position. Both layers move together so the
/// dragged tab reads as a single object lifted off the strip.
pub struct HeaderTabsGhostDraw<'a> {
    pub spans: SpanList<'a>,
    /// Pixel x of the ghost chip's left edge — `cursor.x -
    /// press_offset` clamped so the ghost can't slide past the
    /// hamburger / window-controls strip.
    pub left: f32,
    pub top: f32,
    /// Render width for the ghost label; matches the source chip
    /// so the label clips at the same right edge as its background.
    pub width: f32,
    pub height: f32,
}

/// Right-anchored mini-header that paints the window-control glyphs
/// (minimize / maximize / close) at a fixed pixel position. Built and
/// passed separately so positioning never depends on the variable-width
/// text in the main header buffer.
pub struct HeaderRightDraw<'a> {
    pub spans: SpanList<'a>,
    pub rect: PaneRect,
}

/// Centered window-title text in the top row of the VSCode-style
/// header. Lives in its own buffer to position independently of the
/// tab strip.
pub struct TitleBarDraw<'a> {
    pub spans: SpanList<'a>,
    pub rect: PaneRect,
}

/// Bottom-of-window status bar text (shell, cwd, pane info).
pub struct StatusBarDraw<'a> {
    pub spans: SpanList<'a>,
    pub rect: PaneRect,
}

/// Modal overlay (e.g. help cheat-sheet) — rendered after panes + header.
pub struct OverlayDraw<'a> {
    pub spans: SpanList<'a>,
    pub rect: PaneRect,
    /// When `true`, the overlay text is rendered with `Wrap::None`
    /// — long lines extend past the right edge of the rect and
    /// clip at the bounds rather than wrap onto a second visual
    /// row. The paste-confirmation editor uses this so the
    /// click-to-cursor hit test can rely on "1 logical line == 1
    /// visual row." Defaults to `false` (= cosmic-text's
    /// default word wrap) so help / palette / settings keep
    /// behaving as before.
    pub nowrap: bool,
}

/// Cells reserved on the left of every paste-editor row — the
/// line-start indent (`"  "`) and the soft-wrap continuation marker
/// (`"↪ "`) are both this wide, so cursor / selection / wrap math can
/// treat the text area as starting this many cells in. Keep in lock
/// step with the margin strings emitted by `paste_confirmation_spans`.
const PASTE_EDIT_INDENT_CELLS: usize = 2;

/// Enumerate monospace family names installed on the system, deduplicated
/// and case-insensitively sorted. Used by the `--list-fonts` CLI flag so
/// users can see which families are available — and which one rterm would
/// pick by default — without launching the GUI.
///
/// Heavy on the first call (fontdb scans system directories), so callers
/// should treat it as one-shot diagnostic, not a hot path.
pub fn list_monospace_families() -> Vec<String> {
    let fs = FontSystem::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for face in fs.db().faces() {
        if !face.monospaced {
            continue;
        }
        for (name, _) in &face.families {
            seen.insert(name.clone());
        }
    }
    seen.into_iter().collect()
}

/// Family name `list_monospace_families` would resolve as the default when
/// the user leaves `font.family` empty in config. `None` indicates no
/// known-good monospace is installed — rterm falls back to cosmic-text's
/// built-in `Family::Monospace` resolution in that case.
pub fn default_monospace_family() -> Option<String> {
    let fs = FontSystem::new();
    resolve_default_monospace(&fs)
}

/// Walk fontdb for an installed face that's both monospace AND from our
/// preferred list of well-known monospace families. Returns the chosen
/// family name, or `None` if no preferred family is installed (in which
/// case the caller falls back to `Family::Monospace`'s built-in choice).
fn resolve_default_monospace(fs: &FontSystem) -> Option<String> {
    // Ordered by terminal-readability reputation. The first installed
    // match wins; missing entries just get skipped.
    const PREFERRED: &[&str] = &[
        "JetBrains Mono",
        "JetBrainsMono Nerd Font",
        "Fira Code",
        "FiraCode Nerd Font",
        "Hack",
        "Hack Nerd Font",
        "Cascadia Code",
        "Cascadia Mono",
        "Source Code Pro",
        "IBM Plex Mono",
        "MesloLGS NF",
        "Meslo LG S",
        "Iosevka",
        "Inconsolata",
        "DejaVu Sans Mono",
        "Liberation Mono",
        "Ubuntu Mono",
        "Menlo",
        "Monaco",
        "Consolas",
        "Courier New",
    ];
    let db = fs.db();
    for want in PREFERRED {
        let lower = want.to_ascii_lowercase();
        let hit = db.faces().any(|face| {
            face.families
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(&lower) || name == *want)
        });
        if hit {
            return Some((*want).to_string());
        }
    }
    None
}

/// Shape a single 'M' glyph in `family` at `font_size` and return its
/// natural horizontal advance. Used as the per-cell width so cosmic-text's
/// rendering and our background-quad grid step share the exact same value
/// for the resolved font — keeping character ink centred inside each cell
/// instead of overflowing or under-filling.
fn probe_cell_width(
    font_system: &mut FontSystem,
    font_size: f32,
    line_height: f32,
    family: Family<'_>,
) -> Option<f32> {
    let mut probe = Buffer::new(font_system, Metrics::new(font_size, line_height));
    // Generous bounds: we just want one shaped run, no wrapping.
    probe.set_size(font_system, Some(f32::MAX), Some(line_height * 2.0));
    let attrs = Attrs::new().family(family);
    // 16 'M's gives sub-pixel precision (small per-glyph rounding errors
    // average out), and 'M' is the conventional widest-glyph reference.
    probe.set_text(font_system, "MMMMMMMMMMMMMMMM", attrs, Shaping::Advanced);
    probe.shape_until_scroll(font_system, false);
    let run = probe.layout_runs().next()?;
    if run.glyphs.is_empty() {
        return None;
    }
    let advance = run.line_w / run.glyphs.len() as f32;
    if advance.is_finite() && advance >= 1.0 && advance <= font_size * 1.5 {
        Some(advance)
    } else {
        None
    }
}

impl TextLayer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        font_size: f32,
        font_family: String,
    ) -> Self {
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let renderer = TextRenderer::new(
            &mut atlas,
            device,
            wgpu::MultisampleState::default(),
            None,
        );
        // Separate renderer for the overlay pass — same atlas, same
        // cache, just an independent vertex/index buffer so we can
        // prepare it with a smaller subset of TextAreas and draw it in
        // a later pass after the pane glyphs + overlay backdrop.
        let overlay_renderer = TextRenderer::new(
            &mut atlas,
            device,
            wgpu::MultisampleState::default(),
            None,
        );
        // Keep fractional. Bg quads multiply by this same float when
        // stepping rows, so glyphon's per-line advance and our row step
        // share the exact same arithmetic — no integer drift.
        let line_height = (font_size * 1.25).max(1.0);

        // Leak the family name once so we can hand out 'static slices for
        // Attrs<'static>. The string is tiny and lives for the app lifetime.
        // Production buffers fetch via `self.family_attr()` later.
        //
        // When the user didn't pin a family, resolve `Family::Monospace`
        // ourselves by walking fontdb's installed faces — fontdb's own
        // monospace fallback can land on a near-monospace face whose
        // natural advance disagrees with our deterministic ratio, which
        // shows up as visually uneven character widths (issue: "выводимые
        // символы не одной ширины"). Picking a name here guarantees a
        // truly monospace face when the system has any monospace at all.
        let family_static: &'static str = if font_family.trim().is_empty() {
            resolve_default_monospace(&font_system).map(|n| {
                Box::leak(n.into_boxed_str()) as &'static str
            }).unwrap_or("")
        } else {
            Box::leak(font_family.into_boxed_str())
        };

        let family_for_probe = if family_static.is_empty() {
            Family::Monospace
        } else {
            Family::Name(family_static)
        };
        // Probe the resolved family's natural horizontal advance for one
        // wide glyph ('M'). For a true monospace font the answer is the
        // per-cell advance every character will share. Past iterations
        // probed mixed character sets and got inconsistent results — a
        // single capital 'M' avoids that variance. Falling back to the
        // historical `font_size * 0.6` ratio keeps things sensible when
        // shaping unexpectedly fails (e.g. empty fontdb).
        let cell_width = probe_cell_width(
            &mut font_system,
            font_size,
            font_size * 1.25,
            family_for_probe,
        )
        .unwrap_or_else(|| (font_size * 0.6).max(1.0));

        let mut header_buffer = Buffer::new(&mut font_system, Metrics::new(font_size, line_height));
        header_buffer.set_monospace_width(&mut font_system, Some(cell_width));
        // Header is strictly single-line — disable cosmic-text's
        // default Word/Glyph wrap so overflow beyond the clipped
        // buffer width doesn't spawn a phantom second row underneath
        // the tab strip.
        header_buffer.set_wrap(&mut font_system, Wrap::None);
        let mut header_right_buffer = Buffer::new(&mut font_system, Metrics::new(font_size, line_height));
        header_right_buffer.set_monospace_width(&mut font_system, Some(cell_width));
        let mut header_tabs_buffer = Buffer::new(&mut font_system, Metrics::new(font_size, line_height));
        header_tabs_buffer.set_monospace_width(&mut font_system, Some(cell_width));
        header_tabs_buffer.set_wrap(&mut font_system, Wrap::None);
        let mut header_tabs_ghost_buffer = Buffer::new(&mut font_system, Metrics::new(font_size, line_height));
        header_tabs_ghost_buffer.set_monospace_width(&mut font_system, Some(cell_width));
        header_tabs_ghost_buffer.set_wrap(&mut font_system, Wrap::None);
        let mut title_bar_buffer = Buffer::new(&mut font_system, Metrics::new(font_size, line_height));
        title_bar_buffer.set_monospace_width(&mut font_system, Some(cell_width));
        let mut status_bar_buffer = Buffer::new(&mut font_system, Metrics::new(font_size, line_height));
        status_bar_buffer.set_monospace_width(&mut font_system, Some(cell_width));
        let mut overlay_buffer = Buffer::new(&mut font_system, Metrics::new(font_size, line_height));
        overlay_buffer.set_monospace_width(&mut font_system, Some(cell_width));

        Self {
            font_system,
            swash_cache,
            _glyph_cache: cache,
            viewport,
            atlas,
            renderer,
            overlay_renderer,
            buffers: Vec::new(),
            header_buffer,
            header_right_buffer,
            header_tabs_buffer,
            header_tabs_ghost_buffer,
            title_bar_buffer,
            status_bar_buffer,
            overlay_buffer,
            font_size,
            line_height,
            cell_width,
            font_family: family_static,
        }
    }

    fn family_attr(&self) -> Family<'static> {
        if self.font_family.is_empty() {
            Family::Monospace
        } else {
            Family::Name(self.font_family)
        }
    }

    pub fn header_height(&self) -> f32 {
        // Single-row Chrome-style strip: hamburger + tabs + `+` +
        // window controls, all on one line. ~1.6× line_height gives
        // tab chips comfortable vertical padding above/below text.
        (self.line_height * 1.6 + 6.0).max(self.line_height + 4.0)
    }

    /// Height the bottom bar should reserve right now, given whether
    /// any of its consumers (currently: the search prompt) is asking
    /// for visible space. Computed dynamically each frame so the pane
    /// area can grow back when search closes.
    pub fn bottom_bar_height(&self, active: bool) -> f32 {
        if active {
            self.line_height + 6.0
        } else {
            0.0
        }
    }

    pub fn resize(&mut self, queue: &wgpu::Queue, width: u32, height: u32) {
        self.viewport.update(queue, Resolution { width, height });
    }

    /// Replace the font size in-place, re-probing the monospace advance.
    /// Buffers are recycled — they'll re-shape next prepare() against the
    /// new Metrics.
    pub fn set_font_size(&mut self, font_size: f32) {
        // `f32::clamp` panics on NaN. Plugins / hot-reload could push a
        // bogus value through `rterm.set_font_size`, so fall back to the
        // current size rather than crashing. The value here is PHYSICAL
        // pixels (logical points × scale factor — see
        // `App::set_font_size_absolute`), so the ceiling allows the
        // logical 96 pt maximum on up-to-4× HiDPI displays.
        let font_size = if font_size.is_finite() {
            font_size.clamp(6.0, 384.0)
        } else {
            return;
        };
        if (self.font_size - font_size).abs() < 0.1 {
            return;
        }
        let line_height = (font_size * 1.25).max(1.0);
        // Re-probe at the new size — natural advance scales linearly with
        // font size for monospace faces, but probing each time keeps us
        // honest if cosmic-text ever does sub-pixel hinting differently
        // at small sizes.
        let family_for_probe = self.family_attr();
        let cell_width = probe_cell_width(
            &mut self.font_system,
            font_size,
            line_height,
            family_for_probe,
        )
        .unwrap_or_else(|| (font_size * 0.6).max(1.0));
        self.font_size = font_size;
        self.line_height = line_height;
        self.cell_width = cell_width;
        let metrics = Metrics::new(font_size, line_height);
        // Resync every persistent buffer the layer owns. Missing one
        // here means its glyphs render at the OLD font size while the
        // rect anchored to `cell_width` already moved — produced the
        // "controls drift / icons cut off when font size grows" bug.
        for b in [
            &mut self.header_buffer,
            &mut self.header_right_buffer,
            &mut self.header_tabs_buffer,
            &mut self.title_bar_buffer,
            &mut self.status_bar_buffer,
            &mut self.overlay_buffer,
        ] {
            b.set_metrics(&mut self.font_system, metrics);
            b.set_monospace_width(&mut self.font_system, Some(cell_width));
        }
        for b in &mut self.buffers {
            b.set_metrics(&mut self.font_system, metrics);
            b.set_monospace_width(&mut self.font_system, Some(cell_width));
        }
    }

    /// Replace the active font family in-place. Re-probes the natural
    /// monospace advance for the new face and resyncs every buffer's
    /// monospace width so cell-derived rects stay consistent.
    ///
    /// Empty / whitespace-only `new_family` falls back to cosmic-text's
    /// `Family::Monospace` resolution — the same path
    /// `TextLayer::new` uses when the user leaves `[font].family`
    /// blank.
    ///
    /// `Box::leak` once per change: the glyphon API requires
    /// `Family::Name(&'static str)` for `Attrs<'static>`, so the new
    /// name has to outlive every span built afterwards. Memory cost
    /// is the new family-name bytes (~10-50 B); the previous static
    /// reference becomes unreachable but is small enough that a
    /// long-running session swapping family hundreds of times still
    /// stays under a few KiB.
    pub fn set_font_family(&mut self, new_family: String) {
        let family_static: &'static str = if new_family.trim().is_empty() {
            resolve_default_monospace(&self.font_system)
                .map(|n| Box::leak(n.into_boxed_str()) as &'static str)
                .unwrap_or("")
        } else {
            Box::leak(new_family.into_boxed_str())
        };
        // No-op when nothing actually changed — avoids reshaping every
        // buffer + re-probing cell width on a settling hot-reload tick.
        if family_static == self.font_family {
            return;
        }
        self.font_family = family_static;
        // Re-probe the natural advance for the new face. Falls back
        // to the historical 0.6× ratio when shaping unexpectedly
        // fails (e.g. the requested family isn't in the fontdb).
        let family_for_probe = self.family_attr();
        let cell_width = probe_cell_width(
            &mut self.font_system,
            self.font_size,
            self.line_height,
            family_for_probe,
        )
        .unwrap_or_else(|| (self.font_size * 0.6).max(1.0));
        self.cell_width = cell_width;
        for b in [
            &mut self.header_buffer,
            &mut self.header_right_buffer,
            &mut self.header_tabs_buffer,
            &mut self.title_bar_buffer,
            &mut self.status_bar_buffer,
            &mut self.overlay_buffer,
        ] {
            b.set_monospace_width(&mut self.font_system, Some(cell_width));
        }
        for b in &mut self.buffers {
            b.set_monospace_width(&mut self.font_system, Some(cell_width));
        }
    }

    fn ensure_buffers(&mut self, n: usize) {
        while self.buffers.len() < n {
            let mut b = Buffer::new(
                &mut self.font_system,
                Metrics::new(self.font_size, self.line_height),
            );
            b.set_monospace_width(&mut self.font_system, Some(self.cell_width));
            // Pane content is a pre-wrapped cell grid — the Terminal
            // already broke lines at `cols`. cosmic-text's default
            // word wrap would re-flow any row that rounds a hair
            // wider than the rect (fallback glyphs, grid floors) onto
            // a phantom second visual row, shifting every row below
            // it off the bg-quad/cursor/selection grid.
            b.set_wrap(&mut self.font_system, Wrap::None);
            self.buffers.push(b);
        }
        // Shrink if we have too many buffers — release atlas-bound memory.
        if self.buffers.len() > n + 4 {
            self.buffers.truncate(n);
        }
    }

    pub fn line_height(&self) -> f32 {
        self.line_height
    }
    pub fn cell_width(&self) -> f32 {
        self.cell_width
    }
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Evict glyphs from the glyph atlas that haven't been rendered
    /// since the previous trim. Two atlases (colour + mask) get
    /// trimmed at once. Cheap when nothing's evictable; meaningful
    /// when a long-running session has rasterised many fonts /
    /// sizes / colours and the atlas has grown past its initial
    /// 256×256 footprint. Call infrequently (once a minute is
    /// plenty) so re-uploads don't churn the GPU on cells that
    /// would have been needed again next frame.
    pub fn trim_atlas(&mut self) {
        self.atlas.trim();
    }

    /// Resolved family name handed to cosmic-text. Either the user's
    /// explicit pick or the default-monospace resolution; empty string
    /// means we're on cosmic-text's built-in `Family::Monospace` fallback.
    pub fn font_family(&self) -> &str {
        self.font_family
    }

    pub fn cells_for(&self, width: u32, height: u32, pad: f32) -> (u16, u16) {
        let cols = ((width as f32 - 2.0 * pad) / self.cell_width).max(1.0) as u16;
        let rows = ((height as f32 - 2.0 * pad) / self.line_height).max(1.0) as u16;
        (cols, rows)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        panes: &[PaneDraw<'_>],
        header: Option<&HeaderDraw<'_>>,
        header_right: Option<&HeaderRightDraw<'_>>,
        header_tabs: Option<&HeaderTabsDraw<'_>>,
        header_tabs_ghost: Option<&HeaderTabsGhostDraw<'_>>,
        title_bar: Option<&TitleBarDraw<'_>>,
        status_bar: Option<&StatusBarDraw<'_>>,
        overlay: Option<&OverlayDraw<'_>>,
        viewport_size: (u32, u32),
    ) -> Result<()> {
        self.ensure_buffers(panes.len());

        let family = self.family_attr();
        let default_attrs = Attrs::new().family(family);
        for (i, p) in panes.iter().enumerate() {
            let spans = build_spans(
                p.terminal,
                p.scroll_offset,
                p.focused,
                p.blink_on,
                p.selection.as_ref(),
            );
            let buf = &mut self.buffers[i];
            buf.set_size(
                &mut self.font_system,
                Some(p.rect.width),
                Some(p.rect.height),
            );
            buf.set_rich_text(
                &mut self.font_system,
                spans.iter().map(|(s, k)| (s.as_str(), k.into_attrs(family))),
                default_attrs,
                Shaping::Advanced,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
        }

        if let Some(h) = header {
            // Sized to the CLIPPED width (right_clip - rect.left)
            // rather than the full header rect. Without this cosmic-
            // text uses the buffer width as its wrap boundary —
            // when the combined tab labels are longer than the
            // window-controls reserve allows, the overflow wraps
            // onto a phantom second row visible below the tab strip.
            // Setting `set_size` to the clipped width tells cosmic-
            // text the actual horizontal budget so it truncates
            // (with `right_clip` doing the final pixel-level clip
            // for stragglers that round up at glyph edges).
            let clip_w = h
                .right_clip
                .map(|r| (r - h.rect.left).max(0.0))
                .unwrap_or(h.rect.width);
            self.header_buffer.set_size(
                &mut self.font_system,
                Some(clip_w),
                Some(h.rect.height),
            );
            self.header_buffer.set_rich_text(
                &mut self.font_system,
                h.spans.iter().map(|(s, fg, bold)| {
                    let mut a = default_attrs.color(GlyphColor::rgb(fg[0], fg[1], fg[2]));
                    if *bold {
                        a = a.weight(Weight::BOLD);
                    }
                    (*s, a)
                }),
                default_attrs,
                Shaping::Advanced,
            );
            self.header_buffer.shape_until_scroll(&mut self.font_system, false);
        }
        if let Some(h) = header_tabs {
            self.header_tabs_buffer.set_size(
                &mut self.font_system,
                Some(h.width.max(1.0)),
                Some(h.height),
            );
            self.header_tabs_buffer.set_rich_text(
                &mut self.font_system,
                h.spans.iter().map(|(s, fg, bold)| {
                    let mut a = default_attrs.color(GlyphColor::rgb(fg[0], fg[1], fg[2]));
                    if *bold {
                        a = a.weight(Weight::BOLD);
                    }
                    (*s, a)
                }),
                default_attrs,
                Shaping::Advanced,
            );
            self.header_tabs_buffer.shape_until_scroll(&mut self.font_system, false);
        }

        self.viewport.update(
            queue,
            Resolution { width: viewport_size.0, height: viewport_size.1 },
        );

        // Main pass: panes + header. Drawn before the overlay backdrop.
        let mut main_areas: Vec<TextArea> = panes
            .iter()
            .enumerate()
            .map(|(i, p)| TextArea {
                buffer: &self.buffers[i],
                left: p.rect.left,
                top: p.rect.top,
                scale: 1.0,
                bounds: TextBounds {
                    left: p.rect.left as i32,
                    top: p.rect.top as i32,
                    right: (p.rect.left + p.rect.width) as i32,
                    bottom: (p.rect.top + p.rect.height) as i32,
                },
                default_color: GlyphColor::rgb(220, 220, 220),
                custom_glyphs: &[],
            })
            .collect();
        if let Some(h) = header {
            // Vertically center the single text line in the taller
            // Chrome-style header bar so the tab labels look anchored
            // rather than top-glued.
            let text_offset = ((h.rect.height - self.line_height) * 0.5).max(0.0);
            // Hard right-edge clip — tab labels that would otherwise
            // overflow into the window-control strip (the trailing
            // minimize / maximize / close cluster sharing the same
            // row) are pixel-clipped here instead of relying on the
            // glyph-rendering side to short-circuit. Without this
            // the very last tab visually overlaps the icons even
            // after `tab_layout` reserved cells for them — glyphon
            // would happily paint beyond `tabs_right` since its
            // bounds were the full header rect.
            let right_px = h.right_clip
                .unwrap_or(h.rect.left + h.rect.width) as i32;
            main_areas.push(TextArea {
                buffer: &self.header_buffer,
                left: h.rect.left,
                top: h.rect.top + text_offset,
                scale: 1.0,
                bounds: TextBounds {
                    left: h.rect.left as i32,
                    top: h.rect.top as i32,
                    right: right_px,
                    bottom: (h.rect.top + h.rect.height) as i32,
                },
                default_color: GlyphColor::rgb(220, 220, 220),
                custom_glyphs: &[],
            });
        }
        if let Some(h) = header_tabs {
            // Vertically center the tab labels in the strip — same
            // offset the static `header_buffer` uses, so the two
            // rows align on the same baseline.
            let text_offset = ((h.height - self.line_height) * 0.5).max(0.0);
            main_areas.push(TextArea {
                buffer: &self.header_tabs_buffer,
                left: h.left,
                top: h.top + text_offset,
                scale: 1.0,
                bounds: TextBounds {
                    left: h.left_clip as i32,
                    top: h.top as i32,
                    right: h.right_clip as i32,
                    bottom: (h.top + h.height) as i32,
                },
                default_color: GlyphColor::rgb(220, 220, 220),
                custom_glyphs: &[],
            });
        }
        // Ghost-label glyphs for a tab being dragged. Shaped into a
        // separate buffer + emitted as the last header TextArea so
        // it draws ON TOP of the regular tab labels — closing the
        // visual gap where the chip background follows the cursor
        // but the label text used to stay parked at the source slot.
        if let Some(g) = header_tabs_ghost {
            self.header_tabs_ghost_buffer.set_size(
                &mut self.font_system,
                Some(g.width.max(1.0)),
                Some(g.height),
            );
            self.header_tabs_ghost_buffer.set_rich_text(
                &mut self.font_system,
                g.spans.iter().map(|(s, fg, bold)| {
                    let mut a = default_attrs.color(GlyphColor::rgb(fg[0], fg[1], fg[2]));
                    if *bold {
                        a = a.weight(Weight::BOLD);
                    }
                    (*s, a)
                }),
                default_attrs,
                Shaping::Advanced,
            );
            self.header_tabs_ghost_buffer.shape_until_scroll(&mut self.font_system, false);
            let text_offset = ((g.height - self.line_height) * 0.5).max(0.0);
            main_areas.push(TextArea {
                buffer: &self.header_tabs_ghost_buffer,
                left: g.left,
                top: g.top + text_offset,
                scale: 1.0,
                bounds: TextBounds {
                    left: g.left as i32,
                    top: g.top as i32,
                    right: (g.left + g.width) as i32,
                    bottom: (g.top + g.height) as i32,
                },
                default_color: GlyphColor::rgb(220, 220, 220),
                custom_glyphs: &[],
            });
        }
        if let Some(h) = header_right {
            // Shape into the dedicated right buffer, then position by
            // the caller's pixel rect. Independent of `header.spans`
            // length, so the controls never drift with cwd / url text.
            self.header_right_buffer.set_size(
                &mut self.font_system,
                Some(h.rect.width),
                Some(h.rect.height),
            );
            self.header_right_buffer.set_rich_text(
                &mut self.font_system,
                h.spans.iter().map(|(s, fg, bold)| {
                    let mut a = default_attrs.color(GlyphColor::rgb(fg[0], fg[1], fg[2]));
                    if *bold {
                        a = a.weight(Weight::BOLD);
                    }
                    (*s, a)
                }),
                default_attrs,
                Shaping::Advanced,
            );
            self.header_right_buffer.shape_until_scroll(&mut self.font_system, false);
            let text_offset = ((h.rect.height - self.line_height) * 0.5).max(0.0);
            main_areas.push(TextArea {
                buffer: &self.header_right_buffer,
                left: h.rect.left,
                top: h.rect.top + text_offset,
                scale: 1.0,
                bounds: TextBounds {
                    left: h.rect.left as i32,
                    top: h.rect.top as i32,
                    right: (h.rect.left + h.rect.width) as i32,
                    bottom: (h.rect.top + h.rect.height) as i32,
                },
                default_color: GlyphColor::rgb(220, 220, 220),
                custom_glyphs: &[],
            });
        }
        // Title-bar text — centered in the top header row.
        if let Some(h) = title_bar {
            self.title_bar_buffer.set_size(
                &mut self.font_system,
                Some(h.rect.width),
                Some(h.rect.height),
            );
            self.title_bar_buffer.set_rich_text(
                &mut self.font_system,
                h.spans.iter().map(|(s, fg, bold)| {
                    let mut a = default_attrs.color(GlyphColor::rgb(fg[0], fg[1], fg[2]));
                    if *bold {
                        a = a.weight(Weight::BOLD);
                    }
                    (*s, a)
                }),
                default_attrs,
                Shaping::Advanced,
            );
            self.title_bar_buffer.shape_until_scroll(&mut self.font_system, false);
            let text_offset = ((h.rect.height - self.line_height) * 0.5).max(0.0);
            main_areas.push(TextArea {
                buffer: &self.title_bar_buffer,
                left: h.rect.left,
                top: h.rect.top + text_offset,
                scale: 1.0,
                bounds: TextBounds {
                    left: h.rect.left as i32,
                    top: h.rect.top as i32,
                    right: (h.rect.left + h.rect.width) as i32,
                    bottom: (h.rect.top + h.rect.height) as i32,
                },
                default_color: GlyphColor::rgb(220, 220, 220),
                custom_glyphs: &[],
            });
        }
        // Status-bar text at the very bottom of the window.
        if let Some(h) = status_bar {
            self.status_bar_buffer.set_size(
                &mut self.font_system,
                Some(h.rect.width),
                Some(h.rect.height),
            );
            self.status_bar_buffer.set_rich_text(
                &mut self.font_system,
                h.spans.iter().map(|(s, fg, bold)| {
                    let mut a = default_attrs.color(GlyphColor::rgb(fg[0], fg[1], fg[2]));
                    if *bold {
                        a = a.weight(Weight::BOLD);
                    }
                    (*s, a)
                }),
                default_attrs,
                Shaping::Advanced,
            );
            self.status_bar_buffer.shape_until_scroll(&mut self.font_system, false);
            let text_offset = ((h.rect.height - self.line_height) * 0.5).max(0.0);
            main_areas.push(TextArea {
                buffer: &self.status_bar_buffer,
                left: h.rect.left,
                top: h.rect.top + text_offset,
                scale: 1.0,
                bounds: TextBounds {
                    left: h.rect.left as i32,
                    top: h.rect.top as i32,
                    right: (h.rect.left + h.rect.width) as i32,
                    bottom: (h.rect.top + h.rect.height) as i32,
                },
                default_color: GlyphColor::rgb(220, 220, 220),
                custom_glyphs: &[],
            });
        }

        // Overlay pass: just the modal text. Lives in a separate
        // renderer so we can sandwich the overlay-bg quads between the
        // two text passes. Otherwise pane glyphs land on top of the
        // panel — the original bleed-through bug.
        let mut overlay_areas: Vec<TextArea> = Vec::new();
        if let Some(o) = overlay {
            self.overlay_buffer.set_size(
                &mut self.font_system,
                Some(o.rect.width),
                Some(o.rect.height),
            );
            // Edit-mode mini-editor needs `Wrap::None` so a long
            // line stays on one visual row (clipping on the right
            // edge of the rect). Everyone else gets the default
            // word wrap so help / palette text breaks at word
            // boundaries.
            self.overlay_buffer.set_wrap(
                &mut self.font_system,
                if o.nowrap { Wrap::None } else { Wrap::WordOrGlyph },
            );
            self.overlay_buffer.set_rich_text(
                &mut self.font_system,
                o.spans.iter().map(|(s, fg, bold)| {
                    let mut a = default_attrs.color(GlyphColor::rgb(fg[0], fg[1], fg[2]));
                    if *bold {
                        a = a.weight(Weight::BOLD);
                    }
                    (*s, a)
                }),
                default_attrs,
                Shaping::Advanced,
            );
            self.overlay_buffer.shape_until_scroll(&mut self.font_system, false);
            overlay_areas.push(TextArea {
                buffer: &self.overlay_buffer,
                left: o.rect.left,
                top: o.rect.top,
                scale: 1.0,
                bounds: TextBounds {
                    left: o.rect.left as i32,
                    top: o.rect.top as i32,
                    right: (o.rect.left + o.rect.width) as i32,
                    bottom: (o.rect.top + o.rect.height) as i32,
                },
                default_color: GlyphColor::rgb(240, 240, 240),
                custom_glyphs: &[],
            });
        }

        self.renderer
            .prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                main_areas,
                &mut self.swash_cache,
            )
            .map_err(|e| anyhow!("text prepare main: {e:?}"))?;
        self.overlay_renderer
            .prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                overlay_areas,
                &mut self.swash_cache,
            )
            .map_err(|e| anyhow!("text prepare overlay: {e:?}"))?;
        Ok(())
    }

    /// Draw the MAIN text pass — panes + header. Must be sandwiched
    /// between `BgLayer::draw_main` and `BgLayer::draw_overlay`.
    pub fn render_main<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) -> Result<()> {
        self.renderer
            .render(&self.atlas, &self.viewport, pass)
            .map_err(|e| anyhow!("text render main: {e:?}"))
    }

    /// Draw the OVERLAY text pass — settings/menu/help/rename text on
    /// top of the overlay backdrop. Call last in the render pass.
    pub fn render_overlay<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) -> Result<()> {
        self.overlay_renderer
            .render(&self.atlas, &self.viewport, pass)
            .map_err(|e| anyhow!("text render overlay: {e:?}"))
    }
}

// Struct field drop order matters here. Rust drops fields in declaration
// order (top → bottom), and Mesa's GL path on WSL2 (llvmpipe + EGL +
// Wayland) segfaults if a wgpu pipeline / buffer / texture is destroyed
// after the EGL surface that anchors its GL context. Order is therefore:
//
//   1. heavy GPU resources (text layer, bg layer) — drop first so their
//      `glDelete*` calls run while the EGL surface is still current.
//   2. wgpu surface — drop next; this is the `eglDestroySurface` call.
//   3. queue, device — wgpu cleanup. `Arc<Device>` inside (1) keeps the
//      device alive past nominal drop until those handles are gone.
//   4. window — destroys the Wayland surface; must outlive EGL teardown.
//
// We also explicitly `self.state = None` from `exiting()` so this Drop
// chain runs while the event loop is still alive (Wayland connection
// open, EGL display valid). Letting it run after `run_app` returns is
// what the original segfault was.
pub struct GpuState {
    pub text: TextLayer,
    bg: BgLayer,
    /// Inline-image pipeline. Owned alongside `bg` / `text`, drawn
    /// between the bg-quad pass and the overlay pass so images sit
    /// on top of cell backgrounds and under modal panels.
    images: image_pass::ImageLayer,
    clear_color: wgpu::Color,
    config: wgpu::SurfaceConfiguration,
    surface: wgpu::Surface<'static>,
    queue: wgpu::Queue,
    device: wgpu::Device,
    window: Arc<Window>,
}

impl GpuState {
    pub async fn new(
        window: Arc<Window>,
        font_size: f32,
        font_family: String,
        opacity: f32,
    ) -> Result<Self> {
        let size = window.inner_size();
        // Explicitly disable validation + the Vulkan loader's debug log
        // spam. `InstanceDescriptor::default()` enables `VALIDATION | DEBUG`
        // in debug builds, which has two real costs on Linux: (1) requests
        // `VK_LAYER_KHRONOS_validation` (typically not installed → a warn)
        // and (2) the loader pipes its layer/ICD enumeration to stderr at
        // INFO level — hundreds of lines before the window can open under
        // `cargo run`. We render glyphs, not GPU compute; validation isn't
        // load-bearing for us. End users can opt back in via `WGPU_DEBUG=1`
        // or `RUST_LOG=wgpu_hal=info` if they're chasing a render bug.
        let flags = match std::env::var("WGPU_DEBUG").ok().as_deref() {
            Some("1") | Some("true") => wgpu::InstanceFlags::debugging(),
            _ => wgpu::InstanceFlags::empty(),
        };
        // Backend selection: default to `all()` so GL is in the pool as
        // a fallback. `Backends::PRIMARY` excludes GL, which means on
        // platforms where Vulkan hangs/breaks (WSL2 mesa, headless
        // containers without ICDs) wgpu has nothing left to try and
        // the window never opens. `WGPU_BACKEND=vulkan|gl|dx12|metal`
        // honoured for explicit control.
        let backends = match std::env::var("WGPU_BACKEND").ok().as_deref() {
            Some("vulkan") | Some("vk") => wgpu::Backends::VULKAN,
            Some("gl") | Some("opengl") | Some("gles") => wgpu::Backends::GL,
            Some("metal") => wgpu::Backends::METAL,
            Some("dx12") => wgpu::Backends::DX12,
            Some("primary") => wgpu::Backends::PRIMARY,
            Some("secondary") => wgpu::Backends::SECONDARY,
            _ => {
                // WSL2 ships mesa drivers but Vulkan there frequently
                // stalls during instance init (no proper GPU pass-
                // through). Default to GL only so the window opens.
                // Users with working Vulkan can opt back in via
                // `WGPU_BACKEND=vulkan`.
                if is_wsl() {
                    tracing::info!(
                        "detected WSL2 — defaulting to GL backend \
                         (set WGPU_BACKEND=vulkan to override)",
                    );
                    wgpu::Backends::GL
                } else {
                    wgpu::Backends::all()
                }
            }
        };
        tracing::info!(?backends, "initialising wgpu instance");
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            flags,
            ..wgpu::InstanceDescriptor::default()
        });
        tracing::info!("wgpu instance created");
        let surface = instance.create_surface(window.clone()).context("create_surface")?;
        tracing::info!("requesting GPU adapter");
        // Prefer a hardware adapter; if the platform has none (CI runner,
        // bare-bones container, broken drivers), retry with the explicit
        // software fallback (llvmpipe on Linux, WARP on Windows) so the
        // user gets a working window instead of a hard "no compatible
        // GPU adapter" error.
        let adapter = match instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
        {
            Some(a) => a,
            None => {
                tracing::warn!(
                    "no hardware GPU adapter — falling back to software (slower)",
                );
                instance
                    .request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::default(),
                        compatible_surface: Some(&surface),
                        force_fallback_adapter: true,
                    })
                    .await
                    .ok_or_else(|| anyhow!("no compatible GPU adapter, even with fallback"))?
            }
        };
        let info = adapter.get_info();
        // Log the full adapter identity, not just the friendly name.
        // When a user reports a render glitch the maintainers need
        // (vendor, device, driver, driver_info, device_type) to
        // correlate with known driver bugs (e.g. Qualcomm Adreno on
        // Windows-on-ARM has a different set of DX12 quirks than
        // Intel Arc on x86_64).
        tracing::info!(
            backend = ?info.backend,
            adapter = %info.name,
            vendor = format_args!("0x{:04x}", info.vendor),
            device = format_args!("0x{:04x}", info.device),
            device_type = ?info.device_type,
            driver = %info.driver,
            driver_info = %info.driver_info,
            "GPU adapter selected; requesting device",
        );
        // Take the adapter's full limits rather than `downlevel_defaults`.
        // The downlevel preset caps `max_texture_dimension_2d` at 2048,
        // which immediately fails `Surface::configure` when the user
        // maximises / fullscreens on any modern monitor (a 2560×1440
        // surface won't fit a 2048×2048 texture limit). Using the
        // adapter's own limits lets us track real hardware capability
        // (typically 8192–16384 on dGPU and 4096+ on iGPU); the surface
        // size is also clamped to those limits below, so a downlevel-
        // only adapter still gets a working — if letterboxed — surface
        // instead of a crash.
        let adapter_limits = adapter.limits();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("rterm-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: adapter_limits.clone(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .context("request_device")?;
        tracing::info!("GPU device ready");

        let caps = surface.get_capabilities(&adapter);
        // Echo the surface's full capability matrix so a render-
        // glitch log (Adreno on Windows-ARM, llvmpipe on WSL2,
        // ancient Intel iGPU, ...) shows what we actually had to
        // pick from. Without this the reader has to guess whether
        // the chosen format/present-mode/alpha-mode was first-
        // choice or a fallback after we filtered.
        tracing::info!(
            formats = ?caps.formats,
            present_modes = ?caps.present_modes,
            alpha_modes = ?caps.alpha_modes,
            "surface capabilities",
        );
        // Prefer an sRGB format; fall back to the first advertised one.
        // `caps.formats[0]` would panic on the (pathological) empty
        // list, so go through `first()` with a clear error instead.
        let format = match caps.formats.iter().copied().find(|f| f.is_srgb()) {
            Some(f) => f,
            None => *caps
                .formats
                .first()
                .ok_or_else(|| anyhow!("surface advertises no texture formats"))?,
        };
        // Pick an alpha-supporting composite mode when the user wants
        // transparency; otherwise stick with the platform default.
        let alpha_mode = pick_alpha_mode(opacity, &caps.alpha_modes);
        if opacity < 1.0 && !alpha_mode_is_transparent(alpha_mode) {
            // No alpha-capable mode → the surface composites opaque no
            // matter what we put in the clear alpha. Warn loudly rather
            // than silently ignoring the configured opacity, since the
            // usual cause is the environment (WSL2/WSLg, some GL
            // backends) and not the config.
            tracing::warn!(
                requested_opacity = opacity,
                available_alpha_modes = ?caps.alpha_modes,
                "window opacity < 1.0 requested but the GPU surface \
                 advertises no alpha-capable composite mode — the window \
                 will render OPAQUE. This is common on WSL2/WSLg and some \
                 GL backends; transparency needs a compositor that honours \
                 per-pixel window alpha (a native Wayland/X11 compositor, \
                 macOS, or Windows with DWM)."
            );
        }

        // Present mode: pick whatever the surface advertises that
        // matches our preference. `AutoVsync` is the right default
        // everywhere except WSL2, where Mesa's GL path can deadlock
        // waiting for vsync under heavy llvmpipe load — fall back to
        // `Fifo` there (still vsync-safe but with a different timing
        // path). `WGPU_PRESENT_MODE=fifo|mailbox|immediate|autovsync|autonovsync`
        // overrides for debugging.
        let preferred_present_mode = match std::env::var("WGPU_PRESENT_MODE")
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("fifo") => wgpu::PresentMode::Fifo,
            Some("mailbox") => wgpu::PresentMode::Mailbox,
            Some("immediate") => wgpu::PresentMode::Immediate,
            Some("autonovsync") => wgpu::PresentMode::AutoNoVsync,
            Some("autovsync") => wgpu::PresentMode::AutoVsync,
            _ => {
                if is_wsl() {
                    wgpu::PresentMode::Fifo
                } else {
                    wgpu::PresentMode::AutoVsync
                }
            }
        };
        let present_mode = if caps.present_modes.contains(&preferred_present_mode) {
            preferred_present_mode
        } else {
            // Surface doesn't expose what we wanted — first available
            // mode is always valid per the wgpu spec.
            caps.present_modes.first().copied().unwrap_or(wgpu::PresentMode::Fifo)
        };
        tracing::info!(?present_mode, "selected present mode");
        // Clamp the requested surface size to the adapter's
        // max_texture_dimension_2d. Some GPUs (and especially
        // virtualised Wayland/llvmpipe setups) advertise only 2048,
        // which means a 2560×1440 fullscreen surface would fail
        // validation. The clamp keeps configure() valid; the small
        // letterbox on hyper-conservative hardware is far better than
        // a crash.
        let max_dim = device.limits().max_texture_dimension_2d.max(1);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.clamp(1, max_dim),
            height: size.height.clamp(1, max_dim),
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        tracing::info!(
            format = ?format,
            requested = ?(size.width, size.height),
            configured = ?(config.width, config.height),
            max_dim,
            "configuring surface",
        );
        surface.configure(&device, &config);
        tracing::info!("building text layer");
        let mut text = TextLayer::new(&device, &queue, format, font_size, font_family);
        text.resize(&queue, config.width, config.height);
        // HiDPI layout diagnostic: the physical/logical numbers behind
        // scale-factor bugs (grid not filling the window, cursor/buttons
        // at the wrong x). Plain INFO so it shows without RUST_LOG.
        {
            let (cols, rows) = text.cells_for(config.width, config.height, PAD);
            tracing::info!(
                scale_factor = window.scale_factor(),
                inner_size = ?(size.width, size.height),
                surface = ?(config.width, config.height),
                cell_width = text.cell_width(),
                line_height = text.line_height(),
                grid = ?(cols, rows),
                "layout diagnostic",
            );
        }
        tracing::info!("building bg layer");
        let mut bg = BgLayer::new(&device, format);
        bg.resize(config.width, config.height);
        let mut images = image_pass::ImageLayer::new(&device, format);
        images.resize(config.width, config.height);
        tracing::info!("gpu state ready");

        // Default surface clear matches DEFAULT_BG; pre-multiply RGB if the
        // surface expects pre-multiplied alpha.
        let bg_rgb = palette::default_bg();
        let mut r = palette::srgb_byte_to_linear(bg_rgb[0]) as f64;
        let mut g = palette::srgb_byte_to_linear(bg_rgb[1]) as f64;
        let mut b = palette::srgb_byte_to_linear(bg_rgb[2]) as f64;
        let a = opacity as f64;
        if matches!(alpha_mode, wgpu::CompositeAlphaMode::PreMultiplied) {
            r *= a;
            g *= a;
            b *= a;
        }
        let clear_color = wgpu::Color { r, g, b, a };

        Ok(Self {
            surface,
            device,
            queue,
            config,
            window,
            clear_color,
            text,
            bg,
            images,
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        // Same clamp as the initial configure(): keep the surface size
        // within the adapter's max_texture_dimension_2d, otherwise
        // wgpu's validator rejects fullscreen / maximise transitions on
        // downlevel adapters with a hard crash (observed on Wayland +
        // GNOME with mesa-software / llvmpipe-style 2048 limits).
        let max_dim = self.device.limits().max_texture_dimension_2d.max(1);
        let cw = w.min(max_dim);
        let ch = h.min(max_dim);
        if cw != w || ch != h {
            tracing::warn!(
                requested = ?(w, h),
                configured = ?(cw, ch),
                max_dim,
                "resize clamped to adapter max texture dimension",
            );
        }
        self.config.width = cw;
        self.config.height = ch;
        self.surface.configure(&self.device, &self.config);
        self.text.resize(&self.queue, cw, ch);
        self.bg.resize(cw, ch);
        self.images.resize(cw, ch);
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    /// Recompute the surface clear colour for a new opacity. The window's
    /// `with_transparent` hint and surface alpha mode are set at create
    /// time, so a runtime opacity change from 1.0 → <1.0 only visibly
    /// blends through to the desktop on compositors that allow alpha on
    /// originally-opaque surfaces. We still update the clear colour so
    /// the value is consistent if the window was created translucent.
    pub fn set_opacity(&mut self, opacity: f32) {
        let opacity = if opacity.is_finite() {
            opacity.clamp(0.0, 1.0) as f64
        } else {
            return;
        };
        let bg_rgb = palette::default_bg();
        let mut r = palette::srgb_byte_to_linear(bg_rgb[0]) as f64;
        let mut g = palette::srgb_byte_to_linear(bg_rgb[1]) as f64;
        let mut b = palette::srgb_byte_to_linear(bg_rgb[2]) as f64;
        if matches!(self.config.alpha_mode, wgpu::CompositeAlphaMode::PreMultiplied) {
            r *= opacity;
            g *= opacity;
            b *= opacity;
        }
        self.clear_color = wgpu::Color { r, g, b, a: opacity };
        self.window.request_redraw();
    }

    /// Submit a frame that just clears the surface to `clear_color` — no
    /// text, no bg-quad, nothing that touches `Terminal` data. Called on
    /// the very first `RedrawRequested` so that Wayland compositors see a
    /// committed buffer and respond with `configure` (which fires the
    /// `Resized` event that kicks the normal render loop). Without this
    /// the egg-and-chicken protocol stalls: client waits for configure to
    /// know its size, compositor waits for a frame to map the surface,
    /// and the window never appears. On X11/Win/Mac this is just a cheap
    /// extra frame.
    pub fn render_clear_only(&mut self) -> std::result::Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("rterm-clear-only") },
        );
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rterm-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        panes: &[PaneDraw<'_>],
        header: Option<&HeaderDraw<'_>>,
        header_right: Option<&HeaderRightDraw<'_>>,
        header_tabs: Option<&HeaderTabsDraw<'_>>,
        header_tabs_ghost: Option<&HeaderTabsGhostDraw<'_>>,
        title_bar: Option<&TitleBarDraw<'_>>,
        status_bar: Option<&StatusBarDraw<'_>>,
        overlay: Option<&OverlayDraw<'_>>,
        flash: f32,
        show_scrollbar: bool,
        before_panes: &[bg::BgQuad],
        after_panes: &[bg::BgQuad],
    ) -> std::result::Result<(), wgpu::SurfaceError> {
        let cell_w = self.text.cell_width();
        let line_h = self.text.line_height();

        let dim = overlay.map(|_| 0.65);
        static R1: std::sync::Once = std::sync::Once::new();
        R1.call_once(|| tracing::debug!("render: entering bg.prepare"));
        self.bg.prepare(
            &self.device,
            &self.queue,
            panes,
            cell_w,
            line_h,
            dim,
            show_scrollbar,
            before_panes,
            after_panes,
        );

        // Build the inline-image quad list. Walks every pane's
        // `image_placements()` and projects each placement's
        // absolute (scrollback ++ grid) row coords into the
        // visible viewport using the same `abs_row + offset -
        // sb_len` mapping the selection / search paths use. Quads
        // that fall outside the pane rect are silently skipped —
        // the renderer doesn't try to clip mid-quad on the GPU
        // side, so an image partially scrolled off-screen
        // disappears at the edge rather than getting cut on the
        // pane boundary. Acceptable for v1; a future pass could
        // emit per-row sub-quads to support smooth edge clipping.
        let mut image_quads: Vec<image_pass::ImageQuad> = Vec::new();
        let mut live_keys: std::collections::HashSet<image_pass::CacheKey> =
            std::collections::HashSet::new();
        for p in panes {
            let placements = p.terminal.image_placements();
            if placements.is_empty() {
                continue;
            }
            let on_alt = p.terminal.is_on_alt_screen();
            let sb_len = if on_alt {
                0
            } else {
                p.terminal.scrollback_len()
            };
            let grid_rows = p.terminal.size().rows as i64;
            for pl in placements {
                // Keep the texture (and any `failed` marker) cached for
                // EVERY placement that still exists, not just the ones
                // currently on screen. Registering the key before the
                // viewport cull means scrolling an image one row out of
                // view no longer frees its GPU texture and forces a
                // full re-decode (tens of ms) when it scrolls back —
                // and a permanently-corrupt payload isn't re-decoded
                // every time it re-enters view.
                let key = (p.pane_uid, pl.image_id);
                live_keys.insert(key);
                // Project absolute row into the viewport.
                let viewport_row =
                    pl.abs_row + p.scroll_offset as i64 - sb_len as i64;
                if viewport_row + pl.rows as i64 <= 0 {
                    continue; // fully above the visible window
                }
                if viewport_row >= grid_rows {
                    continue; // fully below
                }
                let pos = [
                    p.rect.left + pl.col as f32 * cell_w,
                    p.rect.top + viewport_row as f32 * line_h,
                ];
                let size = [pl.cols as f32 * cell_w, pl.rows as f32 * line_h];
                // One-shot trace per image so we can verify
                // projection math (abs_row → viewport_row, then
                // → pixel coords) is sane without spamming a
                // frame loop's worth of log lines.
                static FIRST_QUAD: std::sync::Once = std::sync::Once::new();
                FIRST_QUAD.call_once(|| {
                    tracing::info!(
                        pane_uid = p.pane_uid,
                        image_id = pl.image_id,
                        abs_row = pl.abs_row,
                        sb_len,
                        scroll_offset = p.scroll_offset,
                        viewport_row,
                        grid_rows,
                        pos_x = pos[0],
                        pos_y = pos[1],
                        size_w = size[0],
                        size_h = size[1],
                        pane_top = p.rect.top,
                        pane_height = p.rect.height,
                        "image_pass: first quad projection",
                    );
                });
                image_quads.push(image_pass::ImageQuad {
                    key,
                    pos,
                    size,
                    // Scissor to the owning pane's pixel rect so
                    // an image scrolled above its pane (or one
                    // taller than the pane height) doesn't paint
                    // into the header strip / status bar / a
                    // neighbouring pane.
                    clip: [p.rect.left, p.rect.top, p.rect.width, p.rect.height],
                });
            }
        }
        // GC textures for image ids that no longer have placements
        // (FIFO-evicted, RIS, or just panes that closed).
        self.images.sweep(&live_keys);
        // Closure that the image pass uses to fetch the source
        // bytes for a (pane_uid, image_id) pair. Walks the panes
        // to find the matching `Terminal`, then asks for its
        // image — we can't precompute a HashMap of images because
        // the renderer doesn't own the `Terminal` (the pane
        // mutex guards do, scoped to the closure).
        self.images.prepare(
            &self.device,
            &self.queue,
            &image_quads,
            |(pane_uid, image_id)| {
                panes
                    .iter()
                    .find(|p| p.pane_uid == pane_uid)
                    .and_then(|p| p.terminal.image(image_id))
                    .cloned()
            },
        );
        static R2: std::sync::Once = std::sync::Once::new();
        R2.call_once(|| tracing::debug!("render: entering text.prepare"));
        if let Err(e) = self.text.prepare(
            &self.device,
            &self.queue,
            panes,
            header,
            header_right,
            header_tabs,
            header_tabs_ghost,
            title_bar,
            status_bar,
            overlay,
            (self.config.width, self.config.height),
        ) {
            tracing::warn!("text prepare failed: {e:#}");
        }
        static R3: std::sync::Once = std::sync::Once::new();
        R3.call_once(|| tracing::debug!("render: acquiring swapchain"));

        let output = self.surface.get_current_texture()?;
        static R4: std::sync::Once = std::sync::Once::new();
        R4.call_once(|| tracing::debug!("render: swapchain acquired"));
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("rterm-encoder") });
        {
            // For a brief bell flash, lift the clear color a touch so the
            // bell registers even on a fully-painted screen (see
            // `flash_clear_color` for the soft-neutral-pulse rationale).
            // `flash` is the fade intensity (1.0 at the BEL, easing to 0).
            let clear = if flash > 0.0 {
                flash_clear_color(self.clear_color, flash as f64)
            } else {
                self.clear_color
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rterm-main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Layered render so overlay panels sit ABOVE pane glyphs:
            //   1. bg.draw_main    — pane backgrounds, cursor, dim
            //   2. images.render   — inline image quads (iTerm2 /
            //                        Kitty), drawn over cell bg so
            //                        the underlying default-bg
            //                        colour doesn't show through
            //                        partially-transparent images
            //                        but BEFORE text so glyphs that
            //                        coincidentally overlap (which
            //                        shouldn't happen — parser
            //                        advances past image rows — but
            //                        is harmless if it does) sit
            //                        on top of the bitmap.
            //   3. text.render_main — pane + header glyphs
            //   4. bg.draw_overlay  — modal backdrop panels
            //   5. text.render_overlay — modal text
            // Without (4) sandwiched between (3) and (5) the overlay
            // panel ends up under both text passes and pane glyphs
            // bleed through the menu — the original visual bug.
            self.bg.draw_main(&mut pass);
            self.images.render(&mut pass);
            if let Err(e) = self.text.render_main(&mut pass) {
                tracing::warn!("text render main failed: {e:#}");
            }
            self.bg.draw_overlay(&mut pass);
            if let Err(e) = self.text.render_overlay(&mut pass) {
                tracing::warn!("text render overlay failed: {e:#}");
            }
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }
}

/// One terminal session backed by a PTY plus the shared state needed to
/// render it. The opaque `keepalive` holds whatever the spawner needs to
/// keep alive for the pane's lifetime (e.g. the owning `Pty` struct + the
/// reader thread join handle); dropping the `Pane` closes the PTY.
///
/// `alive` is flipped to `false` by the reader thread when the underlying
/// PTY reaches EOF (i.e. the shell exited). The App prunes panes whose
/// flag has gone false on each redraw.
pub struct Pane {
    /// Stable identifier assigned at construction. Survives pane
    /// reorders, splits, and closes — unlike the (tab, pane) DFS index
    /// pair which shifts whenever sibling panes change. Plugins that
    /// want to address a specific pane long-term (e.g. "set title on
    /// the pane running my build") capture this once and use
    /// `rterm.set_pane_title_by_uid` rather than re-resolving by
    /// (tab, pane) every frame.
    pub uid: u64,
    pub terminal: SharedTerminal,
    pub io: Arc<dyn TerminalIo>,
    /// Static fallback title (set at spawn time, e.g. the shell program).
    pub title: String,
    /// Latest title advertised by the shell via OSC 0/2. Updated each frame
    /// from `Terminal::take_title()`. `None` means use the static `title`.
    pub dynamic_title: Mutex<Option<String>>,
    pub alive: Arc<AtomicBool>,
    /// Bumped by the PTY reader on each successful read. The renderer clears
    /// it on the active tab every frame, so a `true` value means a pane in a
    /// non-focused tab produced output the user hasn't seen yet.
    pub activity: Arc<AtomicBool>,
    /// Wall-clock millisecond timestamp (since `UNIX_EPOCH`) of the most
    /// recent PTY read. Updated by the reader thread; read by plugins via
    /// `list_panes` so they can detect silence.
    pub last_output_ms: Arc<AtomicU64>,
    /// Lines scrolled up into scrollback view (0 = follow live grid).
    pub scroll_offset: AtomicU16,
    /// Last observed `scrollback_len` — used by the renderer to compensate
    /// `scroll_offset` when new lines push into scrollback so the user
    /// stays anchored to the content they were reading.
    pub last_sb_len: AtomicUsize,
    /// Most recent observed `is_on_alt_screen` value, used to detect
    /// transitions and fire `pane.alt_enter` / `pane.alt_leave` events.
    /// Defaults to `false` (primary screen) at spawn.
    pub last_alt_screen: AtomicBool,
    /// Edge-trigger gate for `pane.reverse_screen`: most recent
    /// DECSCNM (?5) state. Defaults to `false` so a freshly-
    /// spawned pane doesn't fire a spurious leave.
    pub last_reverse_screen: AtomicBool,
    /// Edge-trigger gate for `pane.cursor_shape`: most recent
    /// DECSCUSR shape encoded as 0=block, 1=underline, 2=bar. Block
    /// is the spec default so the field starts there — no spurious
    /// event on the first snapshot frame.
    pub last_cursor_shape: AtomicU8,
    /// Edge-trigger gate for `pane.cursor_blink`: most recent
    /// DECSCUSR blink flag. Defaults to `true` to match the
    /// terminal's `cursor_should_blink` default — no spurious
    /// initial event.
    pub last_cursor_blink: AtomicBool,
    /// Edge-trigger gate for `pane.cursor_visible`: most recent
    /// DEC ?25 cursor-visibility flag. Defaults to `true` to match
    /// the terminal's `cursor_visible` default — no spurious initial
    /// event for newly-spawned panes.
    pub last_cursor_visible: AtomicBool,
    /// Edge-trigger gate for `pane.mouse_mode`: most recent mouse
    /// tracking mode encoded as 0=off, 1=x10, 2=btn, 3=any. Default
    /// is 0 (off), matching `MouseTracking::Off` at spawn so no
    /// spurious initial event.
    pub last_mouse_mode: AtomicU8,
    /// Edge-trigger gate for `pane.scrollback_enter` /
    /// `pane.scrollback_leave`: whether the user was last seen
    /// scrolled into history (scroll_offset > 0). Defaults to
    /// `false` (following live output) so a freshly-spawned pane
    /// doesn't fire a spurious leave event.
    pub last_in_scrollback: AtomicBool,
    /// Most recent observed cwd. Edge-triggers a `pane.cwd` event when
    /// it changes between snapshot frames (analogous to `last_alt_screen`).
    pub last_cwd: Mutex<Option<String>>,
    /// Edge-trigger gate for `pane.silence`. Set on any frame the pane
    /// produces output (mirror of `Tab::silence_armed` but per-pane), so
    /// plugins watching a specific split can detect when *that* command
    /// finishes regardless of sibling pane activity.
    pub silence_armed: AtomicBool,
    /// Per-pane bell mute. When `true`, BEL bytes drained from this pane
    /// fire neither the `bell` plugin event nor the visual flash. The
    /// terminal still consumes the byte (no garbage in output). Useful
    /// for chatty REPLs that ring on every typo. Plugins toggle this via
    /// `rterm.set_pane_bell_muted(tab, pane, muted)`.
    pub bell_muted: AtomicBool,
    /// Most recent OSC 133;D exit code captured for this pane (`None`
    /// until the shell finishes its first command). Surfaced through
    /// `list_panes()` so plugins can query "did pane N's last command
    /// fail?" without tracking the `pane.shell_exit` event stream.
    pub last_exit_code: Mutex<Option<i32>>,
    /// Most recent OSC 9;4 progress (state, percent). State semantics
    /// match the iTerm2 spec: 0 = clear (we store `None` instead of
    /// `(0, 0)` so plugins can branch on `if progress` cleanly),
    /// 1 = set, 2 = error, 3 = indeterminate, 4 = warning. Percent is
    /// 0..=100 (ignored by the shell for states 0/3).
    pub progress: Mutex<Option<(u8, u8)>>,
    /// Last observed foreground-process name (Linux: `/proc/<pgid>/comm`).
    /// Snapshotted per frame by the renderer; used as a title fallback so
    /// running `vim` or `htop` shows up in the tab bar even when the shell
    /// hasn't sent an OSC 0/1/2. `None` until the first snapshot frame.
    pub last_foreground_process: Mutex<Option<String>>,
    #[allow(dead_code)]
    keepalive: Box<dyn std::any::Any + Send>,
    /// Per-pane command-line accumulator. Receives every byte we
    /// write to the PTY via `send_input`, peels off ESC sequences /
    /// control bytes, and records cleaned commands into the shared
    /// `History` store on each `\r` / `\n`. `None` when the App was
    /// built without a history (tests, `--smoke`).
    pub(crate) command_capture: command_capture::CommandCapture,
}

/// Monotonic source for `Pane::uid`. Starts at 1 so `0` can serve as a
/// "no pane" sentinel for plugins that pass a default. Wraps at u64::MAX
/// — practically unreachable (1 billion splits per second for 580 years
/// to overflow), but we still skip 0 on wrap to keep the sentinel intact.
static PANE_UID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Pane {
    // Builder pattern would be cleaner, but the call sites are
    // contained (one in App, one in tests) and the eight arguments
    // are each load-bearing distinct types — collapsing into a
    // struct just shifts the noise around.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        terminal: SharedTerminal,
        io: Arc<dyn TerminalIo>,
        title: impl Into<String>,
        alive: Arc<AtomicBool>,
        activity: Arc<AtomicBool>,
        last_output_ms: Arc<AtomicU64>,
        keepalive: Box<dyn std::any::Any + Send>,
        history: Option<Arc<Mutex<rterm_history::History>>>,
    ) -> Self {
        let mut uid = PANE_UID_COUNTER.fetch_add(1, Ordering::Relaxed);
        if uid == 0 {
            // Wrap landed on the sentinel — bump once more.
            uid = PANE_UID_COUNTER.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            uid,
            terminal,
            io,
            title: title.into(),
            dynamic_title: Mutex::new(None),
            alive,
            activity,
            last_output_ms,
            scroll_offset: AtomicU16::new(0),
            last_sb_len: AtomicUsize::new(0),
            last_alt_screen: AtomicBool::new(false),
            last_reverse_screen: AtomicBool::new(false),
            last_cursor_shape: AtomicU8::new(0),
            last_cursor_blink: AtomicBool::new(true),
            last_cursor_visible: AtomicBool::new(true),
            last_mouse_mode: AtomicU8::new(0),
            last_in_scrollback: AtomicBool::new(false),
            last_cwd: Mutex::new(None),
            silence_armed: AtomicBool::new(false),
            bell_muted: AtomicBool::new(false),
            last_exit_code: Mutex::new(None),
            progress: Mutex::new(None),
            last_foreground_process: Mutex::new(None),
            keepalive,
            command_capture: command_capture::CommandCapture::new(history),
        }
    }

    /// Write `bytes` to the pane's PTY *and* feed them through the
    /// per-pane command-history capture. Replaces direct
    /// `pane.send_input(...)` calls so we record what the user
    /// types without scattering capture hooks across the renderer.
    pub(crate) fn send_input(&self, bytes: &[u8]) {
        self.command_capture.feed(bytes);
        self.io.write_input(bytes);
    }

    /// Display title resolution, in priority order:
    /// 1. OSC 0/1/2 dynamic title — what the shell or app explicitly set.
    /// 2. Cached foreground process name (`vim`, `htop`, …) — auto-tracked
    ///    per frame from `/proc/<pgid>/comm` on Linux.
    /// 3. Static fallback (typically the shell program name from spawn).
    ///
    /// (2) is skipped when it equals the static title — no point flipping
    /// the tab label between "bash" and "bash" while sitting at a prompt.
    pub fn display_title(&self) -> String {
        if let Some(dyn_title) = self
            .dynamic_title
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .filter(|s| !s.is_empty())
        {
            return dyn_title;
        }
        if let Some(fg) = self
            .last_foreground_process
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .filter(|s| !s.is_empty() && *s != self.title)
        {
            return fg;
        }
        self.title.clone()
    }
}

/// Factory that produces a fresh pane (used to open new tabs / splits).
pub trait PaneSpawner: Send + Sync {
    /// Spawn a new pane. `cwd` is a hint — when `Some`, the shell starts in
    /// that directory; `None` falls back to the implementation default
    /// (typically the parent process's current dir).
    fn spawn_pane(&self, cwd: Option<&str>) -> Result<Pane>;

    /// Hand the spawner a [`Waker`] so the PTY reader threads it
    /// creates can ask the (otherwise idle) event loop to repaint when
    /// the shell produces output. Called once after the event loop
    /// starts, before the first pane is spawned. Default no-op for
    /// test spawners that don't run readers.
    fn set_waker(&self, _waker: Waker) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// Panes laid out side-by-side along the X axis (default).
    Horizontal,
    /// Panes stacked top-to-bottom along the Y axis.
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpatialDir {
    Left,
    Right,
    Up,
    Down,
}

pub struct Tab {
    /// BSP tree of panes. Each leaf is a Pane; internal Splits choose
    /// horizontal or vertical orientation.
    pub tree: tree::Tree<Pane>,
    /// Path through `tree` to the currently focused leaf.
    pub focus_path: tree::TreePath,
    /// When true the focused pane fills the tab area (tmux-style zoom).
    /// Cleared automatically when the layout changes (split / close).
    pub zoomed: bool,
    /// User- or plugin-supplied tab name. When `Some`, it replaces the
    /// auto-derived pane title in the tab bar.
    pub custom_title: Option<String>,
    /// True when a non-focused tab has emitted output since it was last
    /// focused. The renderer draws an indicator dot for tabs in this state;
    /// switching to the tab clears it.
    pub unread: bool,
    /// Edge-trigger gate for `tab.silence`. Set on any frame the tab
    /// produces output, cleared once silence is announced — so each
    /// active→silent transition fires the event exactly once.
    pub silence_armed: bool,
    /// Edge-trigger gate for the tab-aggregate `tab.alt_enter` /
    /// `tab.alt_leave` events. Tracks "is ANY pane in this tab
    /// currently on the alt screen?" — flips when the first pane
    /// enters or the last pane leaves. Defaults to `false` so a
    /// freshly-spawned tab doesn't fire a spurious leave.
    pub last_any_alt: AtomicBool,
    /// Edge-trigger gate for `tab.progress`: most recently emitted
    /// aggregate `(state, percent)` across all panes in this tab.
    /// `None` means "no active progress reported" — fires when
    /// transitioning to/from that or between distinct values.
    pub last_progress: Mutex<Option<(u8, u8)>>,
}

impl Tab {
    /// Panes in DFS order (a-side first at every Split).
    pub fn panes(&self) -> Vec<&Pane> {
        self.tree.leaves()
    }

    pub fn pane_count(&self) -> usize {
        self.tree.count_leaves()
    }

    pub fn pane_at(&self, idx: usize) -> Option<&Pane> {
        self.tree.leaves().get(idx).copied()
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.tree.leaf_at(&self.focus_path)
    }

    /// Index of the focused pane in DFS order, or `None` if no panes.
    pub fn focused_index(&self) -> Option<usize> {
        let target = self.focused_pane()? as *const Pane;
        self.tree
            .leaves()
            .iter()
            .position(|p| std::ptr::eq(*p, target))
    }

    /// Re-point `focus_path` after `close_leaf(removed_path)` reshaped
    /// the tree. `close_leaf` hoists the surviving sibling subtree one
    /// level up, so EVERY path inside that sibling loses one element —
    /// not just paths under the removed leaf. Re-locating the focused
    /// pane by its stable `uid` handles both cases; when the focused
    /// pane itself was the one removed (uid no longer present), focus
    /// falls to the leftmost survivor under the removed leaf's parent.
    pub(crate) fn repair_focus_after_close(
        &mut self,
        removed_path: &[bool],
        focused_uid: Option<u64>,
    ) {
        let by_uid = focused_uid.and_then(|fuid| {
            self.tree.leaf_paths().into_iter().find(|p| {
                self.tree
                    .leaf_at(p)
                    .map(|leaf| leaf.uid == fuid)
                    .unwrap_or(false)
            })
        });
        self.focus_path = match by_uid {
            Some(p) => p,
            None => {
                let parent = if removed_path.is_empty() {
                    Vec::new()
                } else {
                    removed_path[..removed_path.len() - 1].to_vec()
                };
                descend_leftmost(&self.tree, &parent)
            }
        };
    }
}

const PAD: f32 = 4.0;
const SPLIT_GAP: f32 = 3.0;


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkKind {
    Prompt,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollNav {
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    LineUp,
    LineDown,
    Home,
    End,
}


#[derive(Debug, Clone)]
struct PaletteState {
    query: String,
    /// Plugin-registered action names (in addition to `AppAction::ALL`).
    custom: Vec<String>,
    /// Indices into the merged action list. 0..ALL.len() = built-ins;
    /// ALL.len()..ALL.len()+custom.len() = plugin actions.
    filtered: Vec<usize>,
    selected: usize,
    /// Top of the visible window into `filtered` — pulled along when
    /// `selected` moves past the edges so the highlighted row is
    /// always on screen. With `PALETTE_VISIBLE_ROWS` items rendered
    /// per frame, the viewport is `scroll_offset..scroll_offset+N`.
    scroll_offset: usize,
}

const PALETTE_VISIBLE_ROWS: usize = 20;

/// Rows of the help overlay rendered per frame. The overlay rect is
/// fixed at ~520 px wide; with `line_height ≈ 18` and the default
/// content (≈80 rows of keybindings + user-defined) this fits all
/// content on most window sizes, scrolling beyond.
const HELP_VISIBLE_LINES: usize = 28;

/// Quiet period required after the last `WindowEvent::Resized` before
/// we send a SIGWINCH to the shell. Without this the shell would
/// receive 50+ resize events during a single drag, with intermediate
/// cols values (sometimes 1), and reformat each output line to those
/// transient widths.
const RESIZE_DEBOUNCE_MS: u128 = 120;

/// How long a BEL's visual flash takes to fade back to the normal
/// background. The intensity ramps 1 → 0 over this window (see
/// `flash_clear_color`), so there is no hard "off" edge. A repeated
/// BEL (held Backspace on an empty prompt) re-arms the deadline each
/// time, which pins the lift at a steady low level instead of
/// strobing — one smooth fade when the bells stop.
const BELL_FLASH_FADE_MS: u64 = 150;

#[derive(Debug, Clone)]
struct SearchState {
    pane_idx: usize,
    query: String,
    regex_mode: bool,
    /// True when `regex_mode` is on but `query` failed to compile.
    regex_error: bool,
    /// All matches as (visible_row_at_match_offset, start_col, end_col) in
    /// LOGICAL line indices (scrollback first, then grid), not viewport rows.
    matches: Vec<(usize, u16, u16)>,
    current: usize,
}

/// In-flight tab-rename modal. Lives as long as the user is typing a
/// new title; Enter commits and clears, Esc cancels.
#[derive(Debug, Clone)]
struct RenameTabState {
    /// 0-based tab index whose title is being edited.
    tab_idx: usize,
    /// Working buffer for the new title.
    buffer: String,
    /// Caret position inside `buffer`, in bytes — kept in sync with
    /// the chars we append/pop so cursor rendering and edits agree.
    cursor: usize,
    /// `true` until the user has typed / arrow'd / Backspace'd — first
    /// printable character replaces the prefilled title (browser
    /// "select-all-on-focus" convention without true selection).
    pristine: bool,
}

/// A clickable region inside the settings overlay. The settings overlay
/// is text-rendered but tracks pixel hit-zones for every interactive
/// element (theme rows, +/− buttons, toggles) so the mouse can drive it
/// the same way the keyboard shortcuts do.
#[derive(Debug, Clone)]
enum SettingsHit {
    /// Switch to this built-in theme.
    Theme(&'static str),
    /// Bump font size by `delta` points. `0.0` resets to initial.
    FontDelta(f32),
    /// Bump window opacity by `delta` (0..=1). `0.0` resets to initial.
    OpacityDelta(f32),
    /// Toggle cursor blink.
    ToggleBlink,
    /// Toggle scrollbar visibility.
    ToggleScrollbar,
    /// Toggle the opt-in "auto-detect raw image bytes in the
    /// input stream" feature. Off by default; flipping here
    /// propagates to every live pane's `Terminal`.
    ToggleAutoDetectImages,
    /// Toggle WindTerm-style client-side syntax highlighting.
    ToggleHighlight,
    /// Swap to the help overlay (`Ctrl+Shift+H` equivalent).
    OpenHelp,
    /// Close the settings overlay.
    Close,
}

/// One row in a context menu (right-click popup) or app menu.
#[derive(Debug, Clone)]
enum MenuItem {
    /// Clickable row that fires `action` and closes the menu.
    Action {
        label: &'static str,
        action: AppAction,
        /// `false` greys the row out and disables clicks (e.g. "Copy"
        /// when there is no selection).
        enabled: bool,
    },
    /// Visual divider between groups. Non-interactive.
    Separator,
}

/// Active right-click / hamburger menu state. Positioned in pixel
/// coordinates anchored to the click point (clamped to the window).
#[derive(Debug, Clone)]
struct ContextMenu {
    /// Top-left anchor in pixels.
    anchor: (f32, f32),
    items: Vec<MenuItem>,
    /// Index of the row the cursor currently hovers, or selected via
    /// keyboard arrow-nav. Skips `Separator` rows.
    hovered: Option<usize>,
}

/// Boxed callback the App invokes when the active theme changes — used
/// by rterm-app to persist the choice back to `~/.config/rterm/config.toml`.
pub type ThemeChangeCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Same shape as [`ThemeChangeCallback`], invoked when the user toggles
/// `[image].auto_detect` from the Settings overlay. rterm-app
/// implements this to rewrite the value in `~/.config/rterm/config.toml`
/// so the choice sticks across restarts.
pub type ImageAutoDetectCallback = Arc<dyn Fn(bool) + Send + Sync>;

/// Same shape, invoked when the user toggles syntax highlighting from
/// the Settings overlay. rterm-app rewrites `[highlight].enabled` in
/// config.toml so the choice sticks across restarts.
pub type HighlightToggleCallback = Arc<dyn Fn(bool) + Send + Sync>;

/// Active tab-switch animation. The accent stripe under the active tab
/// slides from `from_idx` to `to_idx` over `TAB_SWITCH_ANIM_MS` ms.
#[derive(Debug, Clone, Copy)]
struct TabSwitchAnim {
    from_idx: usize,
    to_idx: usize,
    started_at: Instant,
}

const TAB_SWITCH_ANIM_MS: u128 = 180;

/// Tab swap animation — the two chips affected by an adjacent swap
/// slide between their old and new x positions. `delta_px` is the
/// horizontal distance each chip needs to cover (positive = chip moved
/// right, negative = left); the displaced sibling moves by `-delta_px`.
#[derive(Debug, Clone, Copy)]
struct TabSwapAnim {
    moved_idx: usize,
    displaced_idx: usize,
    delta_px: f32,
    started_at: Instant,
}

const TAB_SWAP_ANIM_MS: u128 = 140;

/// Pre-computed surface of one tab — output of `App::tab_label`.
/// Shared between hit-test math (`tab_layout`) and rendering
/// (`header_spans`), so the two never drift.
#[derive(Debug, Clone)]
struct TabLabel {
    /// Whether this tab is the active one (used for prefix + accent).
    active: bool,
    /// Whether a background pane in this tab has unread output.
    activity: bool,
    /// Final label text including the prefix and trailing space —
    /// exactly what the header renders. Includes ` ·N` count marker
    /// and zoom marker when applicable.
    label: String,
    /// Width of `label` in monospace cells (`UnicodeWidthChar` sum).
    body_cells: usize,
    /// Width of the trailing progress badge in cells, or 0 when no
    /// pane is reporting OSC 9;4 progress.
    badge_cells: usize,
}

/// Pixel layout for one tab in the header bar — output of `tab_layout`.
#[derive(Debug, Clone, Copy)]
struct TabLayoutEntry {
    idx: usize,
    /// Left pixel edge (inclusive).
    left: f64,
    /// Right edge of the body portion (where the close marker starts).
    body_end: f64,
    /// Right edge of the close marker (= start of next tab).
    close_end: f64,
}

/// Whole tab-strip layout — list of tab entries plus the bounds.
#[derive(Debug, Clone)]
struct TabLayoutInfo {
    /// Rect of the bottom row (where the tabs live).
    tab_strip_rect: PaneRect,
    /// X coordinate where the hamburger button ends and tabs begin.
    hamburger_end: f64,
    entries: Vec<TabLayoutEntry>,
    /// X coordinate where the "+" new-tab button starts, or `None`
    /// when there's no room left after the last tab. Width is
    /// `NEW_TAB_WIDTH_CELLS * cell_w`.
    new_tab_left: Option<f64>,
}

/// Cardinal direction the user is snapping the window to. Each maps
/// to a (position, size) pair derived from the current monitor's geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapDir {
    Left,
    Right,
    Top,
    Bottom,
}

/// Right-side header buttons: minimize / maximize / close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowControl {
    Minimize,
    MaximizeToggle,
    Close,
}

/// Hit-zones within a tab label in the header bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabHit {
    /// Click on the tab text — switches to the tab (or starts a drag).
    Body,
    /// Click on the trailing "× " marker — closes the tab.
    Close,
}

/// Width in cells of the `× ` close marker that follows every tab label
/// in `header_spans`. Kept as a constant so `header_spans` and
/// `tab_hit_at` agree on the rendered width.
const TAB_CLOSE_GLYPH: &str = "× ";
const TAB_CLOSE_WIDTH_CELLS: usize = 2;

/// Breathing-room between the last tab and the window-control strip.
/// Without this the trailing edge of the rightmost tab visually
/// presses against the `−`/`□`/`×` cluster on Windows defaults —
/// reported as "last tab overlaps the minimize / close icons".
const TAB_CONTROLS_GAP_CELLS: usize = 2;

/// `≡` hamburger button at the start of the header. Clicking it opens
/// the app menu (every common action in one list). Padded with leading
/// and trailing spaces so the glyph reads as a distinct chip rather
/// than pressing against the first tab label.
const HAMBURGER_GLYPH: &str = " ☰  ";
const HAMBURGER_WIDTH_CELLS: usize = 4;

/// `+` new-tab button rendered immediately after the last tab chip.
/// The cross itself is drawn geometrically by `tab_bar_quads`; the
/// width constant reserves space in the tab-strip layout so cosmic-
/// text's flow doesn't overflow the chip.
const NEW_TAB_WIDTH_CELLS: usize = 3;

/// Window control glyphs at the right end of the header bar. Each
/// button is 3 cells wide (one leading space, one centred glyph, one
/// trailing space) — compact enough that the icons sit clearly anchored
/// to the right edge even on very large font sizes / HiDPI scale
/// factors, where the previous 5-cell layout left so much padding
/// around each glyph that the cluster looked "floating" in the middle
/// of the header.
///
/// Glyph choice notes: each character is from the BMP and present in
/// every common monospace face (Consolas / Cascadia Code / Cascadia
/// Mono on Windows, DejaVu / JetBrains Mono on Linux, Menlo / SF Mono
/// on macOS). The previous set (`─` U+2500, `▢` U+25A2, `✕` U+2715)
/// rendered with bad cell-alignment on Windows defaults — `▢` and `✕`
/// in particular have inconsistent widths/baselines across Cascadia
/// variants, leaving the icons visibly off-centre inside the cell.
/// `−` / `□` / `×` are part of Latin-1 supplement / basic geometric
/// shapes and have uniformly-sized monospace glyphs everywhere.
const WINDOW_CONTROL_MIN: &str = " − ";
const WINDOW_CONTROL_MAX: &str = " □ ";
const WINDOW_CONTROL_CLOSE: &str = " × ";
const WINDOW_CONTROL_BUTTON_CELLS: usize = 3;
/// Cells of breathing room rendered as plain strip between adjacent
/// control chips (visual gap between the three glyphs).
const WINDOW_CONTROL_GAP_CELLS: usize = 1;
const WINDOW_CONTROLS_WIDTH_CELLS: usize =
    WINDOW_CONTROL_BUTTON_CELLS * 3 + WINDOW_CONTROL_GAP_CELLS * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionMode {
    Char,
    Word,
    Line,
    /// Rectangular / block selection — `Alt+drag`. Same as `Char`
    /// in terms of where the anchor / focus points live, but the
    /// selection rect runs as a literal `min..=max` rect rather than
    /// the usual "linear from anchor to focus across rows."
    Block,
}

/// Selection endpoint anchored in ABSOLUTE logical-row coordinates.
///
/// `abs_row` is the row index in the `scrollback ++ grid` stream:
/// `[0, sb_len)` indexes the scrollback ring (oldest first),
/// `[sb_len, sb_len + grid_rows)` indexes the live grid. Stored as
/// `i64` so the conversion to a viewport row (which subtracts the
/// current `sb_len`) can go negative without underflow — that's
/// the signal the renderer uses to clip a selection that's been
/// scrolled off the top of the visible area.
///
/// Previously selections lived in viewport-relative `SelectionPoint`,
/// so a wheel-scroll of N lines while a selection was active
/// drifted the highlight by N rows: the renderer kept painting
/// "viewport row 5", but viewport row 5 now showed a different
/// logical line. Anchoring in absolute coords makes the selection
/// stick to its content regardless of subsequent scrolling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbsPoint {
    abs_row: i64,
    col: u16,
}

impl AbsPoint {
    /// Build from a viewport-relative `SelectionPoint`, capturing
    /// the absolute coordinate via the same `sb_len - offset +
    /// viewport_row` mapping used everywhere else for the visible
    /// <-> logical translation.
    fn from_viewport(sp: SelectionPoint, sb_len: usize, offset: u16) -> Self {
        let abs_row = sb_len as i64 - offset as i64 + sp.row as i64;
        Self { abs_row, col: sp.col }
    }

    /// Project back to a viewport row given the CURRENT scroll
    /// state. May fall outside `[0, rows)` when the selection has
    /// been scrolled off-screen — callers clip.
    fn to_viewport_row(self, sb_len: usize, offset: u16) -> i64 {
        self.abs_row - sb_len as i64 + offset as i64
    }
}

#[derive(Debug, Clone, Copy)]
struct ActiveSelection {
    pane_idx: usize,
    anchor: AbsPoint,
    focus: AbsPoint,
    mode: SelectionMode,
    /// For Word/Line modes: the inclusive cell range of the original
    /// pivot word/line captured on the initial multi-click. Subsequent
    /// drag-extends snap to word/line bounds at the drag point and
    /// anchor against this pivot range, so the selection always covers
    /// whole words or whole lines.
    pivot: Option<(AbsPoint, AbsPoint)>,
}

impl ActiveSelection {
    /// Translate the absolute-coord anchor / focus into a
    /// viewport-relative `NormSelection` using the CURRENT scroll
    /// state. Rows that fall outside `[0, rows)` are clipped — when
    /// the entire selection is off-screen, returns `None`. Block
    /// mode normalises rows + cols independently (rectangle);
    /// linear mode preserves the historical
    /// "open-ended on intermediate rows" stream semantics.
    fn to_visible_norm(
        self,
        sb_len: usize,
        offset: u16,
        rows: u16,
    ) -> Option<NormSelection> {
        let anchor_r = self.anchor.to_viewport_row(sb_len, offset);
        let focus_r = self.focus.to_viewport_row(sb_len, offset);
        let max_r = rows.saturating_sub(1) as i64;
        if matches!(self.mode, SelectionMode::Block) {
            let (r_start, r_end) = if anchor_r <= focus_r {
                (anchor_r, focus_r)
            } else {
                (focus_r, anchor_r)
            };
            // Clip the row range to the visible window.
            let r_start = r_start.max(0);
            let r_end = r_end.min(max_r);
            if r_start > r_end {
                return None;
            }
            let (c_min, c_max) = if self.anchor.col <= self.focus.col {
                (self.anchor.col, self.focus.col)
            } else {
                (self.focus.col, self.anchor.col)
            };
            return Some(NormSelection {
                start: SelectionPoint { row: r_start as u16, col: c_min },
                end: SelectionPoint { row: r_end as u16, col: c_max + 1 },
                block: true,
            });
        }
        // Linear: sort lexicographically by (row, col) in absolute
        // space, THEN clip rows to the visible window.
        let ((s_r, s_col), (e_r, e_col)) = if (anchor_r, self.anchor.col) <= (focus_r, self.focus.col) {
            ((anchor_r, self.anchor.col), (focus_r, self.focus.col))
        } else {
            ((focus_r, self.focus.col), (anchor_r, self.anchor.col))
        };
        let s_clipped = s_r.max(0);
        let e_clipped = e_r.min(max_r);
        if s_clipped > e_clipped {
            return None;
        }
        // When the selection start is above the viewport, treat the
        // first visible row's col as 0 (selection bled in from
        // above). Same logic mirrored for the end row when the
        // selection extends below.
        let start_col = if s_r >= 0 { s_col } else { 0 };
        let end_col = if e_r <= max_r {
            // `e_col` is the INCLUSIVE max cell, but `NormSelection::end`
            // is EXCLUSIVE (see `contains`: `col >= end.col` is dropped).
            // Bump by one so the last cell of the selection stays
            // highlighted — mirrors the `c_max + 1` the block branch
            // above already does. Without it every linear selection lost
            // its final cell: a double-click dropped the word's last
            // glyph, and a drag-left dropped the originally-clicked cell.
            e_col.saturating_add(1)
        } else {
            // Below the viewport — let it absorb the full row width
            // by setting the end col to u16::MAX; `selection_text`
            // and `NormSelection::contains` both clip against the
            // actual row length.
            u16::MAX
        };
        Some(NormSelection {
            start: SelectionPoint { row: s_clipped as u16, col: start_col },
            end: SelectionPoint { row: e_clipped as u16, col: end_col },
            block: false,
        })
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

/// Side-channel events the renderer's `EventLoop` can receive from
/// background threads via [`winit::event_loop::EventLoopProxy`].
///
/// Today the only producer is the optional Windows global-hotkey
/// worker (see `global_hotkey::install_global_hotkey`). The variant
/// stays `non_exhaustive` so we can add more out-of-band wake-up
/// sources (cron-like timers, IPC bridge) without breaking external
/// match arms.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// Fires when the OS-level global hotkey configured in
    /// `[guake].global_hotkey` is pressed (`WM_HOTKEY` on Windows).
    /// The main thread responds by calling `toggle_guake` and forcing
    /// the window to the foreground if it was unfocused.
    GuakeGlobalHotkey,
    /// A background thread (a PTY reader) produced output and the
    /// window needs to repaint. The event loop is otherwise idle
    /// (`ControlFlow::Wait`); this wakes it. Coalesced via the
    /// [`Waker`]'s pending flag so a flood of reads queues at most one
    /// outstanding event.
    Wake,
}

/// Cheap, clonable handle background threads use to ask the render
/// loop for a repaint. Wraps the winit [`EventLoopProxy`] plus a
/// shared "already pending" flag so a burst of PTY reads between
/// frames sends at most ONE `UserEvent::Wake` — without it a
/// `cat /dev/urandom` flood would queue thousands of proxy events.
#[derive(Clone)]
pub struct Waker {
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    pending: Arc<AtomicBool>,
}

impl Waker {
    /// Request a repaint. No-op (beyond an atomic swap) when a wake is
    /// already queued and not yet handled.
    pub fn wake(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            // Loop is exiting (proxy closed) — nothing to wake.
            let _ = self.proxy.send_event(UserEvent::Wake);
        }
    }

    /// Clear the pending flag. Called by the render loop at the start
    /// of handling a frame so output produced DURING/AFTER the frame
    /// re-arms a fresh wake instead of being swallowed.
    fn mark_handled(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

/// Popup tuning carried inside [`RunConfig`]. Mirrors
/// `rterm_config::HistoryConfig` but lives here so the renderer
/// doesn't depend on the config crate. Equivalent to passing the
/// individual fields directly; collapsed into a struct so future
/// knobs land without touching every RunConfig caller.
#[derive(Debug, Clone, Copy)]
pub struct HistoryPopupConfig {
    /// `false` disables capture AND popup. The renderer keeps the
    /// store handle alive (so existing entries stay queryable via
    /// the CLI) but never queries or injects.
    pub enabled: bool,
    /// Visible row count of the popup. Suggestions beyond this scroll.
    pub popup_rows: u8,
    /// Milliseconds the user must pause typing before the popup
    /// queries history. Short enough to feel responsive, long enough
    /// to avoid flicker per keystroke.
    pub popup_debounce_ms: u32,
    /// Minimum prefix length before the popup arms.
    pub min_prefix_len: u8,
}

impl Default for HistoryPopupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            popup_rows: 5,
            popup_debounce_ms: 150,
            min_prefix_len: 1,
        }
    }
}

/// Renderer-side mirror of `rterm_config::PasteConfig`.
#[derive(Debug, Clone, Copy)]
pub struct PasteConfirmConfig {
    /// When `true`, multi-line pastes ≥ `min_bytes` show the
    /// confirmation modal first. `false` skips the modal — the
    /// legacy "just paste" behaviour.
    pub confirm_multiline: bool,
    /// Multi-line pastes shorter than this many bytes skip the
    /// modal even when `confirm_multiline = true`. Avoids
    /// prompting on tiny `cd && ls` chains.
    pub min_bytes: u32,
}

impl Default for PasteConfirmConfig {
    fn default() -> Self {
        Self {
            confirm_multiline: true,
            min_bytes: 80,
        }
    }
}

/// Guake-style drop-down snapshot. Mirrors `rterm_config::GuakeConfig`
/// but lives in the render crate so the renderer doesn't depend on the
/// config crate.
#[derive(Debug, Clone)]
pub struct GuakeRunConfig {
    /// Opt-in signal. When `true`, `toggle_guake` runs silently.
    /// When `false`, the action still runs but the renderer logs an
    /// `info!` line on the first invocation — preserves the
    /// `enabled = true` cue without leaving a bound action no-op'd.
    pub enabled: bool,
    /// `"top"` | `"bottom"` | `"full"`. Other values fall back to
    /// `"top"`.
    pub position: String,
    /// 10..=100, height fraction of the current monitor for `top` /
    /// `bottom`. Ignored for `"full"`.
    pub height_pct: u8,
    /// 20..=100, width fraction of the current monitor.
    pub width_pct: u8,
    /// Optional OS-level global hotkey spec (same syntax as
    /// `[[keybindings]].keys`). When set and the worker can register
    /// it, the action fires even with the window unfocused. Empty =
    /// no global hotkey. Currently implemented on Windows; other
    /// platforms parse-but-warn-and-skip.
    pub global_hotkey: String,
}

/// Bundle of construction parameters for [`App::new`] / [`run`]. Replaces a
/// 13-arg signature with one named-field struct so call sites stay readable.
pub struct RunConfig {
    pub title: String,
    pub size: (u32, u32),
    pub font_size: f32,
    pub font_family: String,
    pub opacity: f32,
    pub user_bindings: Vec<UserBinding>,
    pub spawner: Arc<dyn PaneSpawner>,
    pub events: Arc<dyn EventSink>,
    pub save_scrollback_on_exit: bool,
    /// When true, new output snaps the focused pane's view back to live
    /// (tail-like) instead of holding the user's anchored read position.
    pub scroll_on_output: bool,
    /// Master cursor-blink toggle. `false` forces a steady cursor.
    pub cursor_blink: bool,
    /// Whether to draw the slim right-edge scrollbar.
    pub show_scrollbar: bool,
    /// Initial state of `[image].auto_detect` — opt-in raw image
    /// detection in the input stream. The Settings overlay's
    /// checkbox reads this value on render and can flip it
    /// runtime; flips propagate to every live pane's terminal.
    pub image_auto_detect: bool,
    /// On-screen flash on BEL. `false` keeps the `bell` plugin event but
    /// skips the visual flash for users who find it distracting.
    pub bell_visual: bool,
    /// WM taskbar urgency ping on BEL when the rterm window is unfocused.
    pub bell_urgent: bool,
    /// Milliseconds of idle before `tab.silence` fires for an armed tab.
    /// `0` disables the event entirely.
    pub tab_silence_ms: u64,
    /// Slow-command threshold (ms). When an OSC 133;D-tagged command's
    /// duration meets or exceeds this, fire `pane.slow_command` and (if
    /// unfocused) ping the taskbar. `0` disables.
    pub slow_command_ms: u64,
    pub session_save: bool,
    pub session_path: Option<std::path::PathBuf>,
    pub session_restore: Vec<RestoredTab>,
    pub session_active: Option<usize>,
    /// When true, render a single clear-only frame and exit the event
    /// loop. Used by `--render-test` to verify that the GPU pipeline
    /// can present a frame on the user's display without spinning up
    /// the full GUI (panes, plugins, tabs).
    pub render_test_only: bool,
    /// Canonical name of the built-in theme to start with (e.g.
    /// `"dracula"`). Empty string means "no built-in theme selected" —
    /// the App treats this as the implicit `default`. Used by the
    /// settings overlay's selection marker and as the cycle anchor.
    pub active_theme: String,
    /// Callback the App invokes when the user picks a new theme via
    /// `cycle_theme` / settings UI. Receives the canonical theme name
    /// (e.g. `"dracula"`). rterm-app uses this to persist the choice
    /// back to `~/.config/rterm/config.toml` under `[appearance].theme`.
    /// `None` disables persistence.
    pub on_theme_change: Option<ThemeChangeCallback>,
    /// Persistence callback for the Settings overlay's
    /// `[x] Auto-detect inline images` checkbox. When `Some`,
    /// flipping the box invokes this with the new value so
    /// rterm-app can write it back into
    /// `~/.config/rterm/config.toml`.
    pub on_image_auto_detect_change: Option<ImageAutoDetectCallback>,
    /// Persistence callback for the Settings overlay's
    /// `[x] Syntax highlighting` checkbox — rewrites
    /// `[highlight].enabled` in config.toml. `None` disables
    /// persistence (the toggle still works for the session).
    pub on_highlight_change: Option<HighlightToggleCallback>,
    /// Whether the OS should draw the window title bar and decorations.
    /// `false` (default) makes rterm own the entire window chrome —
    /// matches the Chrome/Firefox look. Set to `true` to fall back to
    /// the platform's native title bar (useful on tiling WMs).
    pub os_decorations: bool,
    /// OSC 52 clipboard-write policy. `false` (default) blocks shell-
    /// issued `\\e]52;c;<base64>\\e\\\\` from touching the system
    /// clipboard. Set to `true` if you rely on tmux / mosh / SSH copy
    /// flows.
    pub allow_osc52: bool,
    /// Guake-style drop-down configuration. `None` disables the action.
    pub guake: Option<GuakeRunConfig>,
    /// Shared command-history store. `None` disables the popup +
    /// capture entirely. When `Some`, the App holds an Arc clone
    /// to query suggestions and `Pane`s clone it again for capture.
    pub history: Option<Arc<Mutex<rterm_history::History>>>,
    /// Popup tuning — debounce / row count / minimum prefix length.
    /// Carried as a struct (rather than three individual fields) so
    /// future knobs land without touching every RunConfig caller.
    pub history_popup: HistoryPopupConfig,
    /// Multi-line paste safety prompt. When `confirm_multiline =
    /// true`, pasted text containing a newline goes through a
    /// modal first (Paste / Edit / Cancel).
    pub paste_confirm: PasteConfirmConfig,
    /// Optional window icon (taskbar / titlebar / Alt-Tab). Pre-
    /// decoded RGBA8 so this crate doesn't need a PNG decoder.
    /// `None` falls back to whatever the OS / WM uses by default.
    pub icon: Option<AppIcon>,
}

/// Raw RGBA8 pixel buffer + dimensions for the window icon. The
/// renderer turns this into a [`winit::window::Icon`] at window-
/// creation time. Held separately from `RunConfig` so the
/// build-time icon pipeline (in `rterm-app/build.rs`) can hand the
/// bytes through without taking a dep on winit.
#[derive(Clone)]
pub struct AppIcon {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct App {
    state: Option<GpuState>,
    title: String,
    initial_size: (u32, u32),
    /// Current font size in LOGICAL points — the value the user
    /// configured / adjusted, before HiDPI scaling. The renderer works
    /// in physical pixels: `set_font_size_absolute` multiplies this by
    /// the window's scale factor before handing it to `TextLayer`.
    font_size: f32,
    /// Font size captured at startup, before any `rterm.set_font_size`
    /// or `font_increase`/`decrease` action overrides it. Used by
    /// `font_reset` to return to the user's configured value (mirrors
    /// `initial_opacity`).
    initial_font_size: f32,
    font_family: String,
    opacity: f32,
    /// Opacity captured at startup, before any `rterm.set_opacity` or
    /// `opacity_increase`/`decrease` action overrides it. Used by
    /// `opacity_reset` to return to the user's configured value.
    initial_opacity: f32,
    tabs: Vec<Tab>,
    active_tab: usize,
    spawner: Arc<dyn PaneSpawner>,
    events: Arc<dyn EventSink>,
    /// Pre-decoded window icon bytes (set from `RunConfig::icon`).
    /// Converted into `winit::window::Icon` and applied via
    /// `WindowAttributes::with_window_icon` in `resumed`.
    icon: Option<AppIcon>,
    modifiers: ModifiersState,
    cursor_pos: PhysicalPosition<f64>,
    selection: Option<ActiveSelection>,
    mouse_dragging: bool,
    /// True while the left mouse button is held inside the paste-
    /// confirmation modal's Edit area. Each CursorMoved event re-
    /// projects the cursor position and extends the selection from
    /// the anchor planted on press. Cleared on `handle_release`.
    paste_modal_dragging: bool,
    last_click: Option<(Instant, SelectionPoint, usize)>,
    /// Timestamp of the most recent click on header-bar empty space (not
    /// on a tab). A second click within `MAX_DBL_CLICK_MS` opens a new
    /// tab — browser-style "double-click empty tab strip → new tab".
    last_header_empty_click: Option<Instant>,
    /// Pane index currently receiving PTY mouse events. Set when a press
    /// happens while mouse reporting is on; cleared on release.
    mouse_pty_pane: Option<usize>,
    /// Until-instant for a bell-induced visual flash of the surface
    /// clear. The flash intensity fades linearly from 1.0 down to 0 as
    /// this deadline approaches (window = `BELL_FLASH_FADE_MS`), so the
    /// pulse eases out rather than blinking off.
    flash_until: Option<Instant>,
    search: Option<SearchState>,
    /// Path to the Split whose gap is currently being dragged.
    gap_dragging: Option<tree::TreePath>,
    /// Source tab index when the user is dragging a tab to reorder it.
    /// Set on left-press over the header; cleared on release after a
    /// final tab_at hit-test decides the destination.
    tab_dragging: Option<usize>,
    /// Pixel offset between the press point and the dragged chip's
    /// left edge. The ghost-chip rendered during drag sits at
    /// `cursor.x - press_offset` so the chip doesn't visually jump
    /// when the press lands anywhere except dead-center on a tab.
    tab_drag_press_offset: f64,
    /// Per-tab swap animation: when `move_active_tab` swaps two
    /// adjacent entries, both tabs animate from their PREVIOUS x
    /// position to the new one over `TAB_SWAP_ANIM_MS` so the slide
    /// looks smooth instead of a hard jump.
    tab_swap_anim: Option<TabSwapAnim>,
    /// Horizontal scroll of the tab strip in pixels. Increasing it
    /// shifts the tab labels LEFT (so tabs that previously ran past
    /// the window-control buttons slide into view from the right).
    /// Driven by wheel-over-tabstrip — VSCode-style scrolling — and
    /// auto-adjusted on focus change so the active tab stays visible.
    /// Clamped to [0, total_tabs_width - available_strip_width] at
    /// the use site (tab_layout / handle_scroll).
    tab_scroll_offset: f64,
    /// Phase reference for the cursor-blink animation.
    cursor_blink_anchor: Instant,
    /// Last cursor icon we asked winit for — avoid re-setting on every mouse
    /// move (some platforms churn).
    last_cursor_icon: CursorIcon,
    /// URL / hyperlink the cursor is currently hovering, shown in the tab
    /// bar as a status hint. Cleared when the cursor leaves the link cell.
    hover_url: Option<String>,
    /// Show the keybinding cheat-sheet overlay. Toggled via Ctrl+Shift+H,
    /// closed with Esc.
    show_help: bool,
    /// Top-row offset for the help overlay's vertical scroll viewport.
    /// Resets to 0 each time the overlay opens; ↑/↓/PgUp/PgDn/Home/End
    /// adjust it. The overlay clamps the value to keep the visible
    /// window inside the row list.
    help_scroll: usize,
    /// Window-resize debounce. winit fires `Resized` continuously
    /// during a drag; sending the PTY a fresh SIGWINCH on every tick
    /// (cols often goes through transient values like 1) breaks the
    /// shell's reformat — output ends up one-char-per-line. We update
    /// the wgpu surface immediately so rendering looks right, then
    /// schedule the PTY reflow for `RESIZE_DEBOUNCE_MS` after the
    /// last Resized. Each new Resized bumps the timestamp; the redraw
    /// loop checks elapsed time and fires when stable.
    pending_pty_resize_at: Option<Instant>,
    /// Show the live settings overlay (terminator-style configuration
    /// panel). Toggled via Ctrl+Shift+, / `open_settings` action; while
    /// open, keys drive theme / font / opacity changes instead of going
    /// to the PTY.
    show_settings: bool,
    /// Active right-click / hamburger context menu. While `Some`, the
    /// menu absorbs mouse + keyboard input and dims the underlying panes.
    context_menu: Option<ContextMenu>,
    /// In-flight tab rename overlay. While `Some`, keyboard is captured
    /// and the user is editing a new title for `tab_idx`.
    rename_tab: Option<RenameTabState>,
    /// Timestamp of the most recent tab-bar press + which tab. Used to
    /// detect a double-click on the same tab → opens the rename overlay
    /// (Chrome/Firefox convention).
    last_tab_click: Option<(Instant, usize)>,
    /// In-flight tab-switch animation: where the accent stripe was and
    /// where it's going. Cleared once the elapsed time exceeds
    /// `TAB_SWITCH_ANIM_MS`. Used by `tab_bar_quads` to interpolate the
    /// stripe position so the active marker visibly slides between tabs.
    tab_switch_anim: Option<TabSwitchAnim>,
    /// Pixel hit zones for the settings overlay's clickable elements
    /// (themes, +/− buttons, toggles). Rebuilt every time we render
    /// the overlay so resizes / font changes stay in sync.
    settings_hits: Vec<(PaneRect, SettingsHit)>,
    /// True when the user clicked the `≡` hamburger glyph in the header
    /// bar. Same modality as `context_menu`, just opened from a fixed
    /// anchor in the header rather than the cursor position.
    show_app_menu: bool,
    /// Canonical name of the currently-applied built-in theme — used by
    /// `cycle_theme` to find the next entry and by the settings overlay
    /// to show what's selected.
    active_theme: String,
    /// Persistence hook: called with the canonical theme name whenever
    /// the user switches via `cycle_theme` / settings UI / Lua
    /// `set_theme`. `None` means "don't persist" (e.g. tests).
    on_theme_change: Option<ThemeChangeCallback>,
    /// Persistence sink for the auto-detect toggle. See
    /// [`RunConfig::on_image_auto_detect_change`].
    on_image_auto_detect_change: Option<ImageAutoDetectCallback>,
    /// Persistence sink for the syntax-highlight toggle. See
    /// [`RunConfig::on_highlight_change`].
    on_highlight_change: Option<HighlightToggleCallback>,
    /// Whether the OS draws the title bar / window border. Cached at
    /// construction so the `resumed()` event creates the window with
    /// the matching `WindowAttributes::with_decorations` flag.
    os_decorations: bool,
    /// Command palette state (Ctrl+Shift+P). When Some, the keyboard is
    /// captured and the palette overlay renders.
    palette: Option<PaletteState>,
    /// User-defined keybindings loaded from config. Consulted BEFORE the
    /// hard-coded shortcuts so users can override them.
    user_bindings: Vec<UserBinding>,
    /// While drag-selecting, set to -1 if the cursor is past the pane's top
    /// edge (scroll into history), +1 past the bottom (scroll toward live),
    /// 0 inside the pane. RedrawRequested ticks the auto-scroll.
    drag_scroll_dir: i32,
    /// Whether the window currently has keyboard focus. Used to suppress
    /// taskbar attention requests when the user is already looking at us.
    window_focused: bool,
    /// Auto-dump the focused pane's scrollback to `~/.cache/rterm/` when
    /// the event loop exits. Off by default; toggled from config.
    save_scrollback_on_exit: bool,
    /// `terminal.scroll_on_output` — when true, every frame snaps each
    /// pane's view to live whenever its scrollback grows.
    scroll_on_output: bool,
    /// `terminal.cursor_blink` — when false, `cursor_blink_on` always
    /// returns true so the cursor is steady regardless of DECSCUSR.
    cursor_blink: bool,
    /// `terminal.show_scrollbar` — toggles the right-edge scrollbar.
    show_scrollbar: bool,
    /// `[image].auto_detect` — opt-in detection of raw PNG / JPEG
    /// magic bytes in the input stream. Mirrored here so the
    /// Settings overlay can render the checkbox state without
    /// reaching into any pane's `Terminal`; toggling the box
    /// writes through to every live pane's terminal.
    image_auto_detect: bool,
    /// `terminal.bell_visual` — turn the on-screen flash on/off.
    bell_visual: bool,
    /// `terminal.bell_urgent` — turn the taskbar attention ping on/off.
    bell_urgent: bool,
    /// `terminal.allow_osc52` — when false (default) OSC 52 clipboard
    /// writes from the shell are dropped silently. See the security
    /// audit in docs for the threat model.
    allow_osc52: bool,
    /// Guake-style drop-down config snapshot. `None` keeps the action
    /// disabled. Cloned per call inside `toggle_guake`.
    guake: Option<GuakeRunConfig>,
    /// Tracks whether the last `toggle_guake` call put the window in the
    /// dropped-down state. `false` (default) means the window is in its
    /// natural, user-controlled layout — the FIRST `toggle_guake` press
    /// should drop the window down rather than minimise it, regardless
    /// of the current minimise state.
    guake_dropped: bool,
    /// RAII handle for the optional OS-level global hotkey worker.
    /// `None` when `[guake].global_hotkey` was empty / unparseable /
    /// rejected by the OS. Held here so its `Drop` runs at the same
    /// time as `App::drop` — unregistering the hotkey and joining
    /// the worker thread before the EventLoopProxy is dropped.
    #[allow(dead_code)] // held purely for its Drop side effect
    global_hotkey: Option<global_hotkey::GlobalHotkeyHandle>,
    /// Shared command-history store. Cloned per pane (in
    /// `Pane::command_capture`) and queried here from the popup
    /// refresh path. `None` disables the feature.
    history: Option<Arc<Mutex<rterm_history::History>>>,
    /// Popup tuning: row count, debounce, prefix gate.
    history_popup_cfg: HistoryPopupConfig,
    /// Multi-line-paste safety-prompt tuning.
    paste_confirm_cfg: PasteConfirmConfig,
    /// `Some` when the paste-confirmation modal is up. Blocks
    /// the rest of the keyboard / mouse pipeline; the modal
    /// resolves to "paste original" / "paste edited" / "cancel".
    paste_confirmation: Option<paste_confirm::PasteConfirmation>,
    /// `Some` when the suggestion popup is on screen. Owns the
    /// suggestion list, current selection, scroll offset, and the
    /// last-seen capture generation for debounce comparisons.
    suggestion_popup: Option<suggestion_popup::SuggestionPopup>,
    /// `Instant` of the most recent input change in the focused
    /// pane. Set every time `CommandCapture::generation` increases;
    /// the popup arms `popup_debounce_ms` after this stamp.
    last_input_change_at: Option<Instant>,
    /// Capture generation snapshot at the previous frame's check.
    /// Detects "did the user type since last redraw?" without
    /// having to compare prefix strings.
    last_capture_generation: u64,
    /// Set after the FIRST `RedrawRequested` has issued a clear-only frame
    /// to kick the Wayland compositor's `configure` → `Resized` chain.
    /// Until then we render `render_clear_only` rather than the full
    /// pipeline so a slow or contended first-frame doesn't strand the
    /// surface unmapped.
    first_frame_done: bool,
    /// `--render-test` shortcut: exit after the first clear-only frame
    /// is presented. Lets users sanity-check the GPU pipeline without
    /// the full GUI (panes/plugins) hiding any wgpu init failure.
    render_test_only: bool,
    /// Last time we emitted a `frame.tick` plugin event (1 Hz heartbeat).
    /// Lets clock-display / idle-warning plugins do periodic work
    /// without subscribing to every redraw.
    last_frame_tick: Option<Instant>,
    /// Most recent `TextAtlas::trim` timestamp. The glyph atlas
    /// grows lazily as new glyph variants are rasterised; in a
    /// long-running session that's seen many font sizes / colours
    /// / families, it can balloon up to the GPU's max texture
    /// dimension. Periodic trims (cadence: once a minute) evict
    /// glyphs not rendered since the previous trim — cheap when
    /// the working set is stable, meaningful after font-size
    /// cycling or theme churn. `None` until the first trim fires.
    last_atlas_trim: Option<Instant>,
    /// Milliseconds of inactivity before an armed tab fires `tab.silence`.
    /// `0` disables the event.
    tab_silence_ms: u64,
    /// Threshold for emitting `pane.slow_command`. `0` disables.
    slow_command_ms: u64,
    /// Previously focused tab index. Used by `ToggleLastTab` so the user
    /// can ping-pong between two tabs with one shortcut.
    previous_tab: Option<usize>,
    /// When `restore_session` is enabled, the initial list of tabs to
    /// spawn instead of the default single tab. Cleared after first use.
    session_restore: Vec<RestoredTab>,
    /// Tab index to focus after the restore replay (capped at `tabs.len()`).
    session_active: Option<usize>,
    /// Persist `(tab, cwd)` on exit. Pulled from config.
    session_save: bool,
    /// File to write the saved session to. None disables save.
    session_path: Option<std::path::PathBuf>,
    /// Latest focused-pane cwd surfaced as a `cwd` plugin event — used to
    /// dedupe per-frame change detection.
    last_focused_cwd: Option<String>,
    /// Latest focused-pane title surfaced as a `title` plugin event.
    last_focused_title: Option<String>,
    /// Most recent shell exit code captured via OSC 133;D. `None` until a
    /// shell finishes its first command.
    last_exit_code: Option<i32>,
    /// Plugin-supplied window title override from `rterm.set_window_title`.
    /// When `Some`, replaces the auto-derived title in `update_title`.
    custom_window_title: Option<String>,
    /// Timestamp when the focused pane entered DECSET ?2026 (Synchronized
    /// Output Mode). While `Some`, GPU presentation is deferred so apps
    /// like neovim can compose tear-free frames. Cleared when the focused
    /// pane resets the mode OR when 200 ms elapses (the safety timeout
    /// recommended by the spec — protects against a crashed app stranding
    /// the terminal in deferred-render limbo).
    sync_started_at: Option<Instant>,
    /// Set while the renderer is holding a frame to let a burst of PTY
    /// output settle (see the coalescing defer in the redraw handler).
    /// `None` when not currently coalescing. Bounds the hold so a
    /// continuously-streaming pane still renders.
    coalesce_started_at: Option<Instant>,
    /// Wake handle shared with the PTY reader threads so they can kick
    /// the event loop out of `ControlFlow::Wait` when output arrives.
    /// `None` until the loop installs it in `resumed`. Also used to
    /// clear the pending-wake flag at the start of each frame.
    waker: Option<Waker>,
}

const CURSOR_BLINK_PERIOD_MS: u128 = 1000;

const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);

impl App {
    /// Install the optional OS-level global hotkey from
    /// `[guake].global_hotkey`. Called once after `App::new`, before
    /// the event loop starts. Idempotent: a second call replaces the
    /// previous handle (the old one's `Drop` unregisters cleanly).
    /// No-op when the spec is empty or unparseable — the in-app
    /// keybind path still works.
    pub fn install_global_hotkey(
        &mut self,
        spec: &str,
        proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    ) {
        if spec.trim().is_empty() {
            return;
        }
        self.global_hotkey = Some(global_hotkey::install_global_hotkey(spec, proxy));
    }

    /// Build the [`Waker`] the event loop hands to PTY reader threads
    /// (via the spawner) so they can request a repaint when the
    /// otherwise-idle loop is parked in `ControlFlow::Wait`. Stored on
    /// the App so the per-frame handler can clear the pending flag.
    pub fn set_event_proxy(&mut self, proxy: winit::event_loop::EventLoopProxy<UserEvent>) {
        self.waker = Some(Waker {
            proxy,
            pending: Arc::new(AtomicBool::new(false)),
        });
    }

    pub fn new(cfg: RunConfig) -> Self {
        let RunConfig {
            title,
            size,
            font_size,
            font_family,
            opacity,
            user_bindings,
            spawner,
            events,
            save_scrollback_on_exit,
            scroll_on_output,
            cursor_blink,
            show_scrollbar,
            image_auto_detect,
            bell_visual,
            bell_urgent,
            tab_silence_ms,
            slow_command_ms,
            session_save,
            session_path,
            session_restore,
            session_active,
            render_test_only,
            active_theme,
            on_theme_change,
            on_image_auto_detect_change,
            on_highlight_change,
            os_decorations,
            allow_osc52,
            guake,
            history,
            history_popup,
            paste_confirm,
            icon,
        } = cfg;
        // Clamp here so a hand-written config with a negative/zero/huge
        // value can't crash glyphon (Metrics expects positive sizes) or
        // produce a 1px-wide cell grid. Same range as `set_font_size`.
        // NaN guard: `f32::clamp` panics on non-finite inputs; fall
        // back to a sensible mid-range default so a borked config
        // doesn't take down startup.
        let font_size = if font_size.is_finite() {
            font_size.clamp(6.0, 96.0)
        } else {
            13.0
        };
        // Minimum window 320x200 so a `width = 0` (or other tiny config)
        // doesn't open a zero-pixel surface that fails wgpu initialisation
        // on some platforms.
        let size = (size.0.max(320), size.1.max(200));
        Self {
            state: None,
            title,
            initial_size: size,
            font_size,
            initial_font_size: font_size,
            font_family,
            // Guard against NaN — `f32::clamp` panics on non-finite
            // inputs. NaN sneaks in if the config file is hand-edited
            // with a `nan` literal, or a plugin pushes a borked value
            // through `set_palette`-style paths in the future.
            opacity: if opacity.is_finite() {
                opacity.clamp(0.0, 1.0)
            } else {
                1.0
            },
            initial_opacity: if opacity.is_finite() {
                opacity.clamp(0.0, 1.0)
            } else {
                1.0
            },
            tabs: Vec::new(),
            active_tab: 0,
            spawner,
            events,
            icon,
            user_bindings,
            modifiers: ModifiersState::empty(),
            cursor_pos: PhysicalPosition::new(0.0, 0.0),
            selection: None,
            mouse_dragging: false,
            paste_modal_dragging: false,
            last_click: None,
            last_header_empty_click: None,
            mouse_pty_pane: None,
            flash_until: None,
            search: None,
            gap_dragging: None,
            tab_dragging: None,
            tab_drag_press_offset: 0.0,
            tab_swap_anim: None,
            tab_scroll_offset: 0.0,
            cursor_blink_anchor: Instant::now(),
            drag_scroll_dir: 0,
            last_cursor_icon: CursorIcon::Default,
            hover_url: None,
            show_help: false,
            help_scroll: 0,
            pending_pty_resize_at: None,
            show_settings: false,
            context_menu: None,
            rename_tab: None,
            last_tab_click: None,
            tab_switch_anim: None,
            settings_hits: Vec::new(),
            show_app_menu: false,
            active_theme: if active_theme.trim().is_empty() {
                "default".to_string()
            } else {
                active_theme
            },
            on_theme_change,
            on_image_auto_detect_change,
            on_highlight_change,
            os_decorations,
            palette: None,
            window_focused: true,
            save_scrollback_on_exit,
            scroll_on_output,
            cursor_blink,
            show_scrollbar,
            image_auto_detect,
            bell_visual,
            bell_urgent,
            allow_osc52,
            guake,
            guake_dropped: false,
            global_hotkey: None,
            history,
            history_popup_cfg: history_popup,
            paste_confirm_cfg: paste_confirm,
            paste_confirmation: None,
            suggestion_popup: None,
            last_input_change_at: None,
            last_capture_generation: 0,
            first_frame_done: false,
            render_test_only,
            last_frame_tick: None,
            waker: None,
            last_atlas_trim: None,
            tab_silence_ms,
            slow_command_ms,
            previous_tab: None,
            session_restore,
            session_save,
            session_path,
            session_active,
            last_focused_cwd: None,
            last_focused_title: None,
            last_exit_code: None,
            custom_window_title: None,
            sync_started_at: None,
            coalesce_started_at: None,
        }
    }

    fn update_cursor_icon(&mut self, x: f64, y: f64) {
        // Compute hover URL first — used for both pointer cursor + status hint.
        let url_under_cursor = self.url_at(x, y);
        // Cursor selection priority:
        //   1. URL hover → Pointer (clickable link).
        //   2. Tab bar  → Pointer (clickable tab strip; user can also
        //      drag-reorder).
        //   3. Pane gap → EW/NS-Resize.
        //   4. Pane     → Text (I-beam, signals drag-selectable).
        // I-beam-over-pane is also the Wayland-quirk-avoidance default:
        // some compositors render "no cursor" until the app explicitly
        // commits one.
        let over_tab_bar = self.tab_at(x, y).is_some()
            || self
                .header_rect()
                .map(|r| {
                    let yf = y as f32;
                    yf >= r.top && yf < r.top + r.height
                })
                .unwrap_or(false);
        // Window-edge resize zones take cursor priority — without them
        // users can't tell where to grab to resize a borderless window.
        let window_edge = if !self.os_decorations {
            self.window_edge_at(x, y)
        } else {
            None
        };
        let desired = if let Some(dir) = window_edge {
            match dir {
                ResizeDirection::North | ResizeDirection::South => CursorIcon::NsResize,
                ResizeDirection::East | ResizeDirection::West => CursorIcon::EwResize,
                ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::NeswResize,
                ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::NwseResize,
            }
        } else if self.tab_dragging.is_some() {
            // Active tab drag — show "grabbing" so the user gets visual
            // confirmation that the press is being held and the
            // release will land the swap.
            CursorIcon::Grabbing
        } else if url_under_cursor.is_some()
            || over_tab_bar
            || self.new_tab_button_at(x, y)
        {
            CursorIcon::Pointer
        } else {
            match self.gap_at(x, y) {
                Some((_, SplitDir::Horizontal)) => CursorIcon::EwResize,
                Some((_, SplitDir::Vertical)) => CursorIcon::NsResize,
                None => CursorIcon::Text,
            }
        };
        if desired != self.last_cursor_icon {
            if let Some(state) = self.state.as_ref() {
                state.window.set_cursor(desired);
            }
            self.last_cursor_icon = desired;
        }
        if self.hover_url != url_under_cursor {
            // Edge-trigger plugin events on hover transitions only.
            match (&self.hover_url, &url_under_cursor) {
                (None, Some(url)) => self.events.emit("link.hover", url),
                (Some(_), None) => self.events.emit("link.unhover", ""),
                (Some(prev), Some(new)) if prev != new => {
                    self.events.emit("link.hover", new);
                }
                _ => {}
            }
            self.hover_url = url_under_cursor;
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
        }
    }

    /// Hyperlink/auto-detected URL at the cursor's screen position, if any.
    fn url_at(&self, x: f64, y: f64) -> Option<String> {
        let i = self.pane_at(x, y)?;
        let p = self.pixel_to_cell(i, x, y)?;
        let pane = self.active_tab()?.pane_at(i)?;
        let offset = pane.scroll_offset.load(Ordering::Relaxed);
        let term = pane.terminal.lock().ok()?;
        term.hyperlink_at(offset, p.row, p.col)
            .map(|s| s.to_string())
            .or_else(|| term.detect_url_at(offset, p.row, p.col))
    }

    fn cursor_blink_on(&self) -> bool {
        if !self.cursor_blink {
            return true;
        }
        let elapsed = self.cursor_blink_anchor.elapsed().as_millis();
        (elapsed % CURSOR_BLINK_PERIOD_MS) < CURSOR_BLINK_PERIOD_MS / 2
    }

    fn reset_cursor_blink(&mut self) {
        self.cursor_blink_anchor = Instant::now();
    }

    /// Whether the focused pane currently shows a BLINKING cursor (so
    /// the event loop must keep waking at the blink period instead of
    /// sleeping). False when blink is globally off, no pane is focused,
    /// the cursor is hidden, the pane is scrolled back, or the shell
    /// asked for a steady cursor.
    fn focused_cursor_blinks(&self) -> bool {
        if !self.cursor_blink {
            return false;
        }
        // An unfocused window shows a steady (hollow/dim) cursor — no
        // blink — so a background terminal idles fully instead of
        // waking twice a second forever.
        if !self.window_focused {
            return false;
        }
        let Some(tab) = self.active_tab() else { return false };
        let Some(pane) = tab.focused_pane() else { return false };
        if pane.scroll_offset.load(Ordering::Relaxed) != 0 {
            return false;
        }
        pane.terminal
            .lock()
            .map(|t| t.cursor_visible() && t.cursor_should_blink())
            .unwrap_or(false)
    }

    /// True while a short visual animation needs the very next frame:
    /// the bell-flash fade, a tab switch/swap slide, or a deferred
    /// (sync-output / coalesce) present. These run continuously for
    /// ≤200 ms; the loop renders back-to-back until they end.
    fn animation_active(&self) -> bool {
        let now = Instant::now();
        let flash = self.flash_until.map(|t| t > now).unwrap_or(false);
        flash || self.tab_switch_anim.is_some() || self.tab_swap_anim.is_some()
    }

    /// Decide the event-loop control flow after rendering a frame.
    /// Continuous (immediate redraw) while an animation runs; a timed
    /// wake for cursor blink / pending PTY-resize debounce / the 1 Hz
    /// plugin heartbeat; otherwise full idle `Wait` (PTY output and
    /// input wake it via the [`Waker`] / window events). This is what
    /// turns the old unconditional per-frame redraw into a quiet loop.
    fn schedule_after_frame(&self, event_loop: &ActiveEventLoop) {
        if self.animation_active() {
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut consider = |t: Instant| {
            next = Some(next.map_or(t, |c: Instant| c.min(t)));
        };
        // Cursor blink: wake at the next half-period boundary.
        if self.focused_cursor_blinks() {
            let half = (CURSOR_BLINK_PERIOD_MS / 2) as u64;
            let into = (self.cursor_blink_anchor.elapsed().as_millis() as u64) % half;
            consider(now + Duration::from_millis(half - into));
        }
        // Pending PTY-resize debounce — fire the deferred SIGWINCH.
        if let Some(at) = self.pending_pty_resize_at {
            consider(at + Duration::from_millis(RESIZE_DEBOUNCE_MS as u64));
        }
        // 1 Hz plugin heartbeat (`frame.tick`) + silence/idle detection —
        // only when a plugin could consume it, so a plugin-less session
        // idles at zero wakes.
        if self.events.wants_terminal_state() {
            let last = self.last_frame_tick.unwrap_or(now);
            consider(last + Duration::from_secs(1));
        }
        match next {
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    /// Push the current `image_auto_detect` value into every live
    /// pane's `Terminal`. Called when the user toggles the
    /// checkbox in the Settings overlay — without this, the App
    /// would track the new value but the parsers would keep
    /// using whatever state they had at pane-spawn time.
    fn propagate_auto_detect_to_panes(&mut self) {
        let enabled = self.image_auto_detect;
        for tab in &self.tabs {
            for pane in tab.panes() {
                if let Ok(mut term) = pane.terminal.lock() {
                    term.set_auto_detect_inline_images(enabled);
                }
            }
        }
    }

    fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    fn focused_pane(&self) -> Option<&Pane> {
        self.active_tab().and_then(|t| t.focused_pane())
    }

    /// Layout the panes of the active tab via BSP-tree recursion. Returns
    /// rects in DFS order (matches `tab.panes()` / `tab.tree.leaves()`).
    /// Tab index under a mouse click on the header bar, or `None` when
    /// the click missed every tab label (no tabs / clicked past the
    /// last one / outside the bar vertically). Approximation: monospace
    /// labels lay out as `char_count * cell_width` pixels each, so the
    /// hit zones reproduce the rendered widths well enough for click
    /// targeting. The progress badge appended after each label is
    /// counted into that tab's width so the hit area extends to the
    /// gap before the next tab.
    fn tab_at(&self, x: f64, y: f64) -> Option<usize> {
        self.tab_hit_at(x, y).map(|(idx, _)| idx)
    }

    /// Hit-test the window edges for a client-side resize zone. Returns
    /// the `ResizeDirection` matching the corner / edge the cursor is
    /// near, or `None` for clicks anywhere else.
    ///
    /// Skipped when the OS owns decorations — in that case the WM
    /// handles edge resize.
    fn window_edge_at(&self, x: f64, y: f64) -> Option<ResizeDirection> {
        let state = self.state.as_ref()?;
        let w = state.config.width as f64;
        let h = state.config.height as f64;
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        // Generous corner zone (12 px) so the cursor finds them; a
        // narrower edge zone (6 px) so dragging just below the corner
        // resizes a single axis as expected.
        let corner = 12.0_f64;
        let edge = 6.0_f64;
        let near_left = x < edge;
        let near_right = x >= w - edge;
        let near_top = y < edge;
        let near_bottom = y >= h - edge;
        let near_left_c = x < corner;
        let near_right_c = x >= w - corner;
        let near_top_c = y < corner;
        let near_bottom_c = y >= h - corner;
        if near_top && near_left_c || near_left && near_top_c {
            Some(ResizeDirection::NorthWest)
        } else if near_top && near_right_c || near_right && near_top_c {
            Some(ResizeDirection::NorthEast)
        } else if near_bottom && near_left_c || near_left && near_bottom_c {
            Some(ResizeDirection::SouthWest)
        } else if near_bottom && near_right_c || near_right && near_bottom_c {
            Some(ResizeDirection::SouthEast)
        } else if near_top {
            Some(ResizeDirection::North)
        } else if near_bottom {
            Some(ResizeDirection::South)
        } else if near_left {
            Some(ResizeDirection::West)
        } else if near_right {
            Some(ResizeDirection::East)
        } else {
            None
        }
    }

    /// True when (x, y) is over the `+` new-tab button just past the
    /// last tab chip in the tab strip.
    fn new_tab_button_at(&self, x: f64, y: f64) -> bool {
        let Some(layout) = self.tab_layout() else { return false };
        let Some(left) = layout.new_tab_left else { return false };
        let rect = layout.tab_strip_rect;
        let yf = y as f32;
        if yf < rect.top || yf >= rect.top + rect.height {
            return false;
        }
        let Some(state) = self.state.as_ref() else { return false };
        let cell_w = state.text.cell_width() as f64;
        if cell_w <= 0.0 {
            return false;
        }
        let width = NEW_TAB_WIDTH_CELLS as f64 * cell_w;
        x >= left && x < left + width
    }

    /// True when (x, y) is over the `≡` hamburger button at the start
    /// of the tab strip (bottom row of the header).
    fn hamburger_at(&self, x: f64, y: f64) -> bool {
        let Some(rect) = self.header_rect() else { return false };
        let yf = y as f32;
        if yf < rect.top || yf >= rect.top + rect.height {
            return false;
        }
        let Some(state) = self.state.as_ref() else { return false };
        let cell_w = state.text.cell_width() as f64;
        if cell_w <= 0.0 {
            return false;
        }
        let hb_end = rect.left as f64 + HAMBURGER_WIDTH_CELLS as f64 * cell_w;
        x >= rect.left as f64 && x < hb_end
    }

    /// True when (x, y) hits one of the right-anchored window control
    /// buttons (minimize / maximize / close). Returns which one.
    /// Chip widths follow `WINDOW_CONTROL_BUTTON_CELLS` with
    /// `WINDOW_CONTROL_GAP_CELLS` of empty strip between adjacent
    /// chips so the buttons read as three distinct targets.
    fn window_control_at(&self, x: f64, y: f64) -> Option<WindowControl> {
        // With OS decorations the native title bar owns min/max/close
        // and we draw no in-header buttons (`header_right_spans` /
        // `tab_bar_quads` gate on the same flag) — but this hit-test
        // used to keep reporting hits, so a click on the rightmost
        // cells of the tab strip triggered an INVISIBLE control;
        // worst case the phantom Close exited the app.
        if self.os_decorations {
            return None;
        }
        let rect = self.header_rect()?;
        let yf = y as f32;
        if yf < rect.top || yf >= rect.top + rect.height {
            return None;
        }
        let state = self.state.as_ref()?;
        let cell_w = state.text.cell_width() as f64;
        if cell_w <= 0.0 {
            return None;
        }
        let right = rect.left as f64 + rect.width as f64;
        let btn = WINDOW_CONTROL_BUTTON_CELLS as f64 * cell_w;
        let gap = WINDOW_CONTROL_GAP_CELLS as f64 * cell_w;
        let close_left = right - btn;
        let max_left = close_left - gap - btn;
        let min_left = max_left - gap - btn;
        if x >= close_left && x < right {
            Some(WindowControl::Close)
        } else if x >= max_left && x < max_left + btn {
            Some(WindowControl::MaximizeToggle)
        } else if x >= min_left && x < min_left + btn {
            Some(WindowControl::Minimize)
        } else {
            None
        }
    }

    /// Execute the click on a window control. Returns true when the
    /// window should exit (close button on the last tab path).
    #[must_use]
    fn click_window_control(&mut self, which: WindowControl) -> bool {
        let Some(state) = self.state.as_ref() else { return false };
        match which {
            WindowControl::Minimize => {
                state.window.set_minimized(true);
            }
            WindowControl::MaximizeToggle => {
                let is_max = state.window.is_maximized();
                state.window.set_maximized(!is_max);
            }
            WindowControl::Close => return true,
        }
        false
    }

    /// Toggle the app menu (hamburger). Anchors below the `≡` glyph.
    fn toggle_app_menu(&mut self) {
        if self.context_menu.is_some() {
            self.close_context_menu();
            self.show_app_menu = false;
            return;
        }
        let anchor = if let Some(rect) = self.header_rect() {
            (rect.left, rect.top + rect.height)
        } else {
            (0.0, 0.0)
        };
        self.context_menu = Some(ContextMenu {
            anchor,
            items: app_menu_items(),
            hovered: None,
        });
        self.show_app_menu = true;
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }

    /// Build a layout table for every tab in the header. Returns
    /// `(idx, body_left, body_right, close_right)` per tab in cursor
    /// order. Both `tab_hit_at` and `tab_bar_quads` consume this so
    /// hit-testing and the per-tab background quads stay in lockstep.
    /// Single source of truth for one tab's rendered surface: the
    /// prefix char (▌ active / • activity / space inactive), the body
    /// label (`prefix + name + pane-count + zoom-marker`), the body
    /// cell width, and the progress-badge cell width. Used by both
    /// `tab_layout` (for hit-test pixel math) and `header_spans` (for
    /// rendering). When these two paths drift, clicks land on the
    /// wrong tab — pulling them through one helper keeps them in
    /// lockstep.
    fn tab_label(
        &self,
        idx: usize,
        tab: &Tab,
    ) -> TabLabel {
        let active = idx == self.active_tab;
        let activity = !active
            && tab.panes().iter().any(|p| p.activity.load(Ordering::Relaxed));
        let prefix = if active {
            "▌"
        } else if activity {
            "•"
        } else {
            " "
        };
        let label = if let Some(p) = tab.focused_pane() {
            let zoom_marker = if tab.zoomed { " ⛶" } else { "" };
            let pane_count = tab.pane_count();
            let count_marker = if pane_count > 1 {
                format!(" ·{}", pane_count)
            } else {
                String::new()
            };
            let name = match tab.custom_title.as_deref() {
                Some(s) if !s.is_empty() => trim_label(s, 14),
                _ => trim_label(&p.display_title(), 14),
            };
            format!(
                "{prefix} {pane}{count}{zoom} ",
                prefix = prefix,
                pane = name,
                count = count_marker,
                zoom = zoom_marker,
            )
        } else {
            format!("{} {} ", prefix, idx + 1)
        };
        use unicode_width::UnicodeWidthChar;
        let body_cells = label
            .chars()
            .map(|c| c.width().unwrap_or(0))
            .sum::<usize>();
        // OSC 9;4 aggregate progress badge — `tab_bar_quads` /
        // `header_spans` append a glyph after the label; report its
        // width so the hit-test stays exact.
        let badge_cells = if let Some((state_, pct)) = tab
            .panes()
            .iter()
            .filter_map(|p| p.progress.lock().ok().and_then(|g| *g))
            .max_by_key(|(s, p)| (progress_severity(*s), *p))
        {
            match state_ {
                2..=4 => 2,
                1 => format!("{}% ", pct).chars().count(),
                _ => 0,
            }
        } else {
            0
        };
        TabLabel {
            active,
            activity,
            label,
            body_cells,
            badge_cells,
        }
    }

    fn tab_layout(&self) -> Option<TabLayoutInfo> {
        let rect = self.header_rect()?;
        let state = self.state.as_ref()?;
        let cell_w = state.text.cell_width() as f64;
        if cell_w <= 0.0 {
            return None;
        }
        let hb_end = rect.left as f64 + HAMBURGER_WIDTH_CELLS as f64 * cell_w;
        // Reserve the trailing N cells for the window controls
        // (min / max / close), plus an extra `TAB_CONTROLS_GAP_CELLS`
        // of breathing room so the very last tab doesn't visually
        // butt up against the close button.
        let controls_cells = if self.os_decorations {
            0.0
        } else {
            WINDOW_CONTROLS_WIDTH_CELLS as f64 + TAB_CONTROLS_GAP_CELLS as f64
        };
        let tabs_right =
            (rect.left as f64 + rect.width as f64) - controls_cells * cell_w;
        // VSCode-style horizontal scroll: shifting the cursor LEFT by
        // `tab_scroll_offset` slides off-screen tabs into view from
        // the right. Negative `left` is fine — TextBounds + the
        // tab-rendering clip on `tabs_right` keep glyphs from
        // bleeding into the hamburger or controls strips.
        let mut cursor = hb_end - self.tab_scroll_offset;
        let mut entries = Vec::with_capacity(self.tabs.len());
        for (i, tab) in self.tabs.iter().enumerate() {
            let info = self.tab_label(i, tab);
            let close_cells = TAB_CLOSE_WIDTH_CELLS as f64;
            let body_end = cursor + (info.body_cells + info.badge_cells) as f64 * cell_w;
            let close_end = body_end + close_cells * cell_w;
            // Every tab gets a full-width entry — even off-screen
            // ones. The clamp on `body_end` / `close_end` to
            // `tabs_right` keeps hit-test from accepting clicks past
            // the visible strip; off-screen `left < hb_end` entries
            // are filtered out at hit-test time.
            entries.push(TabLayoutEntry {
                idx: i,
                left: cursor,
                body_end: body_end.min(tabs_right),
                close_end: close_end.min(tabs_right),
            });
            cursor = close_end;
        }
        // The `+` new-tab button sits right after the last tab when
        // there's still room for it before the window-controls strip.
        let new_tab_w = NEW_TAB_WIDTH_CELLS as f64 * cell_w;
        let new_tab_left = if cursor + new_tab_w <= tabs_right {
            Some(cursor)
        } else {
            None
        };
        Some(TabLayoutInfo {
            tab_strip_rect: rect,
            hamburger_end: hb_end,
            entries,
            new_tab_left,
        })
    }

    /// Total width of all tab chips in pixels (no scroll applied).
    /// Used to clamp `tab_scroll_offset` so the user can't scroll past
    /// the right end of the strip into empty space.
    fn total_tabs_width(&self) -> f64 {
        let Some(state) = self.state.as_ref() else { return 0.0 };
        let cell_w = state.text.cell_width() as f64;
        if cell_w <= 0.0 {
            return 0.0;
        }
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let info = self.tab_label(i, t);
                (info.body_cells + info.badge_cells + TAB_CLOSE_WIDTH_CELLS) as f64 * cell_w
            })
            .sum()
    }

    /// Width of the visible tab-strip slot (between the hamburger
    /// button and the window-controls reserve, minus the breathing
    /// gap that keeps last-tab from touching the close button).
    fn tab_strip_visible_width(&self) -> f64 {
        let Some(rect) = self.header_rect() else { return 0.0 };
        let Some(state) = self.state.as_ref() else { return 0.0 };
        let cell_w = state.text.cell_width() as f64;
        let hb_end = rect.left as f64 + HAMBURGER_WIDTH_CELLS as f64 * cell_w;
        let controls_cells = if self.os_decorations {
            0.0
        } else {
            WINDOW_CONTROLS_WIDTH_CELLS as f64 + TAB_CONTROLS_GAP_CELLS as f64
        };
        let tabs_right =
            (rect.left as f64 + rect.width as f64) - controls_cells * cell_w;
        (tabs_right - hb_end).max(0.0)
    }

    /// Maximum value for `tab_scroll_offset`. Zero when all tabs fit
    /// inside the visible strip.
    fn max_tab_scroll(&self) -> f64 {
        let total = self.total_tabs_width();
        let visible = self.tab_strip_visible_width();
        (total - visible).max(0.0)
    }

    /// Adjust `tab_scroll_offset` by `delta_px` and clamp to
    /// `[0, max_tab_scroll()]`. Returns `true` when the value
    /// actually changed (so callers can request a redraw).
    pub(crate) fn scroll_tab_strip(&mut self, delta_px: f64) -> bool {
        let max = self.max_tab_scroll();
        let new = (self.tab_scroll_offset + delta_px).clamp(0.0, max);
        if (new - self.tab_scroll_offset).abs() < 0.5 {
            return false;
        }
        self.tab_scroll_offset = new;
        true
    }

    /// Make sure the active tab is visible inside the strip. Called
    /// after focus changes / new-tab / tab close so a tab that's
    /// scrolled off-screen doesn't stay there silently.
    pub(crate) fn ensure_active_tab_visible(&mut self) {
        let Some(layout) = self.tab_layout() else { return };
        let Some(entry) = layout.entries.iter().find(|e| e.idx == self.active_tab)
        else {
            return;
        };
        let visible_left = layout.hamburger_end;
        let visible_right = layout.tab_strip_rect.left as f64
            + layout.tab_strip_rect.width as f64
            - if self.os_decorations {
                0.0
            } else {
                let cell_w = self
                    .state
                    .as_ref()
                    .map(|s| s.text.cell_width() as f64)
                    .unwrap_or(0.0);
                (WINDOW_CONTROLS_WIDTH_CELLS + TAB_CONTROLS_GAP_CELLS) as f64 * cell_w
            };
        let tab_w = entry.close_end - entry.left;
        if entry.left < visible_left {
            // Tab is off the LEFT edge — bring it back into view by
            // reducing the scroll offset.
            let delta = entry.left - visible_left;
            self.scroll_tab_strip(delta);
        } else if entry.left + tab_w > visible_right {
            // Off the RIGHT edge — scroll forward until the right
            // edge of the tab matches the visible right edge.
            let delta = (entry.left + tab_w) - visible_right;
            self.scroll_tab_strip(delta);
        }
    }

    /// `tab_at` + a flag saying whether the click landed on the small
    /// "× close" marker appended to each tab label.
    fn tab_hit_at(&self, x: f64, y: f64) -> Option<(usize, TabHit)> {
        let layout = self.tab_layout()?;
        let rect = layout.tab_strip_rect;
        if (y as f32) < rect.top || (y as f32) >= rect.top + rect.height {
            return None;
        }
        if x < rect.left as f64 || x < layout.hamburger_end {
            return None;
        }
        for e in &layout.entries {
            // Clamp the left edge of the hit zone to the hamburger
            // boundary. Off-screen-left tabs (when `tab_scroll_offset`
            // is positive) have `e.left < hamburger_end`; without
            // this clamp a click in the gap between the hamburger
            // and the first visible tab would be attributed to the
            // off-screen tab.
            let visible_left = e.left.max(layout.hamburger_end);
            if x >= visible_left && x < e.close_end {
                let hit = if x >= e.body_end {
                    TabHit::Close
                } else {
                    TabHit::Body
                };
                return Some((e.idx, hit));
            }
        }
        None
    }


    /// How many lines fit in the current overlay rect, minus the
    /// caller's `reserved` rows (used for header/footer chrome like
    /// "↑ N more" hints, the palette query line, etc.). Returns
    /// `fallback` when the rect or font metrics aren't available yet
    /// (e.g. before the first frame).
    fn overlay_visible_lines(&self, reserved: usize, fallback: usize) -> usize {
        let Some(rect) = self.help_rect() else { return fallback };
        let Some(state) = self.state.as_ref() else { return fallback };
        let line_h = state.text.line_height();
        if line_h <= 0.0 {
            return fallback;
        }
        // `reserved + 1` keeps the last row from sitting flush against
        // the panel border.
        let extra = reserved as f32 + 1.0;
        ((rect.height / line_h - extra).max(1.0) as usize).max(1)
    }

    /// How many help-overlay lines fit in the current overlay rect.
    /// Reserves two rows for the "↑ N more" / "↓ N more" hints; the
    /// fallback constant kicks in only when the rect isn't computable.
    fn help_visible_lines(&self) -> usize {
        self.overlay_visible_lines(2, HELP_VISIBLE_LINES)
    }

    /// How many palette items fit in the current overlay rect. The
    /// query line, the (N matches) counter, the blank spacer between
    /// them and the result list, and the optional "↑/↓ N more" hints
    /// together occupy ~4 rows that the result list cannot use.
    fn palette_visible_rows(&self) -> usize {
        self.overlay_visible_lines(4, PALETTE_VISIBLE_ROWS)
    }

    /// Mouse press while the settings overlay is open. Looks up which
    /// clickable element (theme row, +/− button, toggle) the click
    /// landed on; clicks outside the overlay close it.
    fn handle_settings_press(&mut self, x: f64, y: f64) -> bool {
        let xf = x as f32;
        let yf = y as f32;
        let hit = self
            .settings_hits
            .iter()
            .find(|(r, _)| {
                xf >= r.left
                    && xf < r.left + r.width
                    && yf >= r.top
                    && yf < r.top + r.height
            })
            .map(|(_, h)| h.clone());
        match hit {
            Some(SettingsHit::Theme(name)) => {
                if let Some((canon, pal)) = palette::theme_by_name(name) {
                    self.apply_theme(canon, pal);
                }
            }
            Some(SettingsHit::FontDelta(d)) => {
                if d == 0.0 {
                    let initial = self.initial_font_size;
                    self.set_font_size_absolute(initial);
                } else {
                    self.adjust_font_size(d);
                }
            }
            Some(SettingsHit::OpacityDelta(d)) => {
                if d == 0.0 {
                    let initial = self.initial_opacity;
                    self.opacity = initial;
                    if let Some(state) = self.state.as_mut() {
                        state.set_opacity(initial);
                    }
                } else {
                    self.adjust_opacity(d);
                }
            }
            Some(SettingsHit::ToggleBlink) => {
                self.cursor_blink = !self.cursor_blink;
                self.reset_cursor_blink();
            }
            Some(SettingsHit::ToggleScrollbar) => {
                self.show_scrollbar = !self.show_scrollbar;
            }
            Some(SettingsHit::ToggleAutoDetectImages) => {
                self.image_auto_detect = !self.image_auto_detect;
                self.propagate_auto_detect_to_panes();
                // Persist the new value so it survives a restart.
                // The callback (set by rterm-app) rewrites
                // `[image].auto_detect` in config.toml via
                // `toml_edit`, preserving comments + layout.
                if let Some(cb) = self.on_image_auto_detect_change.as_ref() {
                    cb(self.image_auto_detect);
                }
            }
            Some(SettingsHit::ToggleHighlight) => {
                // Runtime flip lives in the global highlight engine;
                // persist `[highlight].enabled` via the callback so it
                // survives a restart.
                let now = !highlight::is_enabled();
                highlight::set_enabled(now);
                if let Some(cb) = self.on_highlight_change.as_ref() {
                    cb(now);
                }
            }
            Some(SettingsHit::OpenHelp) => {
                self.show_settings = false;
                self.show_help = true;
            }
            Some(SettingsHit::Close) => {
                self.show_settings = false;
                self.reset_cursor_blink();
            }
            None => {
                // Click outside any hit zone — but did the click stay
                // within the overlay rect? If yes, keep it open (user
                // missed a button); if outside, close it.
                if let Some(rect) = self.help_rect() {
                    if !(xf >= rect.left
                        && xf < rect.left + rect.width
                        && yf >= rect.top
                        && yf < rect.top + rect.height)
                    {
                        self.show_settings = false;
                        self.reset_cursor_blink();
                    }
                }
            }
        }
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
        false
    }

    /// Consume a key while the settings overlay is open. Returns `true`
    /// when the window should exit (only via the Quit shortcut delegated
    /// to `handle_app_shortcut`).
    fn handle_settings_key(&mut self, event: &KeyEvent) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        if matches!(&event.logical_key, Key::Named(NamedKey::Escape)) {
            self.show_settings = false;
            self.reset_cursor_blink();
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
            return false;
        }
        // Allow the binding that opened the overlay (Ctrl+Shift+,) to
        // close it too, plus app-shortcuts like Ctrl+Q for quit.
        if let Some(exit) = self.handle_app_shortcut(event) {
            return exit;
        }
        let shift = self.modifiers.contains(ModifiersState::SHIFT);
        if let Key::Character(c) = &event.logical_key {
            match c.as_str().to_ascii_lowercase().as_str() {
                "t" => self.cycle_theme(if shift { -1 } else { 1 }),
                "f" => self.adjust_font_size(if shift { -1.0 } else { 1.0 }),
                "0" => {
                    let initial = self.initial_font_size;
                    self.set_font_size_absolute(initial);
                }
                "o" => self.adjust_opacity(if shift { -0.05 } else { 0.05 }),
                "9" => {
                    let initial = self.initial_opacity;
                    self.opacity = initial;
                    if let Some(state) = self.state.as_mut() {
                        state.set_opacity(initial);
                    }
                }
                "b" => {
                    self.cursor_blink = !self.cursor_blink;
                    self.reset_cursor_blink();
                }
                "s" => {
                    self.show_scrollbar = !self.show_scrollbar;
                }
                "y" => {
                    let now = !highlight::is_enabled();
                    highlight::set_enabled(now);
                    if let Some(cb) = self.on_highlight_change.as_ref() {
                        cb(now);
                    }
                }
                "?" => {
                    self.show_settings = false;
                    self.show_help = true;
                }
                _ => {}
            }
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
        }
        false
    }

    /// Background fill + thin top separator for the bottom search
    /// bar. Painted before pane glyphs so the strip blocks anything
    /// that might otherwise extend past `outer_rect`. Returns empty
    /// vec when the bar isn't reserving space (no active search).
    fn status_bar_quads(&self) -> Vec<bg::BgQuad> {
        let Some(rect) = self.status_bar_rect() else { return Vec::new() };
        let mut out = Vec::new();
        let bg = palette::default_bg();
        let fill = [
            bg[0].saturating_sub(14),
            bg[1].saturating_sub(14),
            bg[2].saturating_sub(14),
        ];
        out.push(bg::BgQuad::from_srgb(
            [rect.left, rect.top],
            [rect.width, rect.height],
            fill,
            1.0,
        ));
        let sep = palette::default_fg().map(|c| c.saturating_sub(140));
        out.push(bg::BgQuad::from_srgb(
            [rect.left, rect.top],
            [rect.width, 1.0],
            sep,
            0.45,
        ));
        out
    }

    /// Subtle highlight on the window edge / corner the cursor is
    /// hovering — gives users visual feedback for the otherwise-
    /// invisible CSD resize hit zone. Returns empty Vec when the OS
    /// owns decorations (no client-side resize) or when the cursor is
    /// nowhere near an edge.
    fn resize_marker_quads(&self) -> Vec<bg::BgQuad> {
        if self.os_decorations {
            return Vec::new();
        }
        let (x, y) = (self.cursor_pos.x, self.cursor_pos.y);
        let Some(dir) = self.window_edge_at(x, y) else {
            return Vec::new();
        };
        let Some(state) = self.state.as_ref() else { return Vec::new() };
        let w = state.config.width as f32;
        let h = state.config.height as f32;
        let stripe = 3.0_f32; // visible stripe width along the edge
        let corner = 18.0_f32; // visible corner-arm length
        let accent = palette::default_fg().map(|c| c.saturating_sub(40));
        let alpha = 0.55;
        let mut out: Vec<bg::BgQuad> = Vec::new();
        let push_strip = |out: &mut Vec<bg::BgQuad>, x: f32, y: f32, w: f32, h: f32| {
            out.push(bg::BgQuad::from_srgb([x, y], [w, h], accent, alpha));
        };
        match dir {
            ResizeDirection::North => push_strip(&mut out, 0.0, 0.0, w, stripe),
            ResizeDirection::South => push_strip(&mut out, 0.0, h - stripe, w, stripe),
            ResizeDirection::West => push_strip(&mut out, 0.0, 0.0, stripe, h),
            ResizeDirection::East => push_strip(&mut out, w - stripe, 0.0, stripe, h),
            // Corners get an "L"-shaped marker: two short stripes
            // forming the angle the drag will reshape.
            ResizeDirection::NorthWest => {
                push_strip(&mut out, 0.0, 0.0, corner, stripe);
                push_strip(&mut out, 0.0, 0.0, stripe, corner);
            }
            ResizeDirection::NorthEast => {
                push_strip(&mut out, w - corner, 0.0, corner, stripe);
                push_strip(&mut out, w - stripe, 0.0, stripe, corner);
            }
            ResizeDirection::SouthWest => {
                push_strip(&mut out, 0.0, h - stripe, corner, stripe);
                push_strip(&mut out, 0.0, h - corner, stripe, corner);
            }
            ResizeDirection::SouthEast => {
                push_strip(&mut out, w - corner, h - stripe, corner, stripe);
                push_strip(&mut out, w - stripe, h - corner, stripe, corner);
            }
        }
        out
    }

    /// Per-frame visible divider quads at each pane split gap. Without
    /// these the gap is just transparent clear-colour space and the
    /// boundary between adjacent panes is invisible. Returns one quad
    /// per split frame, sized to the SPLIT_GAP width/height.
    fn pane_split_quads(&self) -> Vec<bg::BgQuad> {
        let Some(outer) = self.outer_rect() else { return Vec::new() };
        let Some(tab) = self.active_tab() else { return Vec::new() };
        // A zoomed pane occupies the entire outer rect; no siblings,
        // no visible gaps to mark.
        if tab.zoomed {
            return Vec::new();
        }
        let bg = palette::default_bg();
        let fg = palette::default_fg();
        // Divider colour — pulls slightly toward fg so it reads as a
        // line on both dark and light themes. Alpha < 1.0 so the
        // divider feels like a hairline, not a hard wall.
        let mix = |a: u8, b: u8, t: f32| {
            let af = a as f32;
            let bf = b as f32;
            (af + (bf - af) * t.clamp(0.0, 1.0)).round().clamp(0.0, 255.0) as u8
        };
        let line = [
            mix(bg[0], fg[0], 0.35),
            mix(bg[1], fg[1], 0.35),
            mix(bg[2], fg[2], 0.35),
        ];
        let mut out = Vec::new();
        for frame in tab.tree.splits(outer, SPLIT_GAP) {
            match frame.dir {
                SplitDir::Horizontal => {
                    let x = frame.a_rect.left + frame.a_rect.width;
                    out.push(bg::BgQuad::from_srgb(
                        [x, frame.a_rect.top],
                        [SPLIT_GAP, frame.a_rect.height],
                        line,
                        0.85,
                    ));
                }
                SplitDir::Vertical => {
                    let y = frame.a_rect.top + frame.a_rect.height;
                    out.push(bg::BgQuad::from_srgb(
                        [frame.a_rect.left, y],
                        [frame.a_rect.width, SPLIT_GAP],
                        line,
                        0.85,
                    ));
                }
            }
        }
        out
    }

    /// Per-frame background quads for the Chrome/Firefox-style tab
    /// strip: a darker strip behind the whole bar, a slightly lighter
    /// "chip" per inactive tab, a brighter chip + accent underline for
    /// the active tab, and a separate chip behind the `≡` hamburger.
    fn tab_bar_quads(&self) -> Vec<bg::BgQuad> {
        let Some(layout) = self.tab_layout() else { return Vec::new() };
        let rect = layout.tab_strip_rect;
        let mut out: Vec<bg::BgQuad> = Vec::new();
        let bg = palette::default_bg();
        // When the window is unfocused, fade every chip toward the
        // strip color so the tab bar visibly recedes — same convention
        // as Chrome / Firefox / system title bars on macOS.
        let focus_mul: f32 = if self.window_focused { 1.0 } else { 0.55 };
        let win_width = self.state.as_ref().map(|s| s.config.width as f32).unwrap_or(0.0);
        let strip_bg = [
            bg[0].saturating_sub(10),
            bg[1].saturating_sub(10),
            bg[2].saturating_sub(10),
        ];
        // Full-width strip fill behind the tab row.
        out.push(bg::BgQuad::from_srgb(
            [0.0, 0.0],
            [win_width, rect.top + rect.height],
            strip_bg,
            1.0,
        ));
        // Thin separator at the bottom of the tab strip → visual break
        // between the chrome and the pane area below.
        let sep_color = palette::default_fg().map(|c| c.saturating_sub(140));
        out.push(bg::BgQuad::from_srgb(
            [0.0, rect.top + rect.height - 1.0],
            [win_width, 1.0],
            sep_color,
            0.35,
        ));
        // What is the cursor hovering? Used to add a subtle highlight
        // on the focused chip for visual feedback. Computed once so
        // every chip push just compares ids.
        let (hover_x, hover_y) = (self.cursor_pos.x, self.cursor_pos.y);
        let hover_tab = self.tab_hit_at(hover_x, hover_y).map(|(i, _)| i);
        let hover_close_tab = self
            .tab_hit_at(hover_x, hover_y)
            .filter(|(_, h)| *h == TabHit::Close)
            .map(|(i, _)| i);
        let hover_hamburger = self.hamburger_at(hover_x, hover_y);
        let hover_winctl = self.window_control_at(hover_x, hover_y);
        // Hamburger chip — slightly lighter than the strip. Rounded
        // corners give it a "button" feel matching the tab chips.
        let hb_left = rect.left;
        let hb_width = (layout.hamburger_end - rect.left as f64) as f32;
        // VSCode-style: shallower insets and smaller corner radius so
        // chips read as flat rectangles rather than rounded buttons.
        let chip_inset = (rect.height * 0.08).clamp(1.0, 4.0);
        let chip_top = rect.top + chip_inset;
        let chip_h = (rect.height - chip_inset * 2.0).max(2.0);
        let chip_radius = (chip_h * 0.14).clamp(2.0, 4.0);
        let hb_chip = [
            bg[0].saturating_add(8),
            bg[1].saturating_add(8),
            bg[2].saturating_add(8),
        ];
        out.push(bg::BgQuad::from_srgb_rounded(
            [hb_left, chip_top],
            [hb_width, chip_h],
            hb_chip,
            (if hover_hamburger { 1.0 } else { 0.85 }) * focus_mul,
            chip_radius,
        ));
        // Per-tab chips. Inactive tabs share a single muted body
        // color; the active tab gets a brighter body + an accent
        // underline (3 px) anchored to the chip's bottom edge.
        let inactive_body = [
            bg[0].saturating_add(6),
            bg[1].saturating_add(6),
            bg[2].saturating_add(8),
        ];
        let active_body = [
            bg[0].saturating_add(22),
            bg[1].saturating_add(22),
            bg[2].saturating_add(28),
        ];
        // VSCode-style blue accent — #007ACC, the canonical "VS Code
        // blue" used for the activity bar focus stripe and active-tab
        // indicator. Falls back to the palette accent under DECSCNM /
        // explicit `[colors].cursor` overrides for thematic coherence.
        let active_accent: [u8; 3] = [0, 122, 204];
        let gap = 2.0_f32; // tiny gap between tab chips
        // Tab-switch animation progress: 0.0 at start, 1.0 when over.
        // None when no animation is in flight.
        let anim_progress = self.tab_switch_anim.and_then(|a| {
            let ms = a.started_at.elapsed().as_millis();
            if ms >= TAB_SWITCH_ANIM_MS {
                None
            } else {
                Some((a, ms as f32 / TAB_SWITCH_ANIM_MS as f32))
            }
        });
        // Cubic ease-out: snappy initial movement, gentle landing.
        let ease = |t: f32| {
            let u = 1.0 - t;
            1.0 - u * u * u
        };
        // Tab-swap animation: when `move_active_tab` swapped two
        // chips, the displaced sibling slides back from where the
        // moved chip used to sit. `1.0 - eased_t` shrinks the offset
        // from `delta_px` to `0` over `TAB_SWAP_ANIM_MS`.
        let swap_anim = self.tab_swap_anim.and_then(|a| {
            let ms = a.started_at.elapsed().as_millis();
            if ms >= TAB_SWAP_ANIM_MS {
                None
            } else {
                Some((a, ms as f32 / TAB_SWAP_ANIM_MS as f32))
            }
        });
        // Cursor-following ghost chip when a drag is in flight. The
        // ghost's left edge tracks `cursor.x - press_offset`, so the
        // chip stays exactly where the user grabbed it. We still draw
        // the "slot" placeholder at the dragged tab's logical
        // position (semi-transparent) so the user sees where it'll
        // land on release.
        let dragging_idx = self.tab_dragging;
        for e in &layout.entries {
            let chip_left = e.left as f32 + gap * 0.5;
            let chip_width = ((e.close_end - e.left) as f32 - gap).max(2.0);
            let active = e.idx == self.active_tab;
            let hovered = hover_tab == Some(e.idx);
            let is_drag_source = dragging_idx == Some(e.idx);
            // Slide offset for the swap animation. The newly-moved
            // chip slides from its OLD x to its NEW x; the displaced
            // sibling slides the opposite way.
            let slide_offset = match swap_anim {
                Some((a, t)) if a.moved_idx == e.idx => -a.delta_px * (1.0 - ease(t)),
                Some((a, t)) if a.displaced_idx == e.idx => a.delta_px * (1.0 - ease(t)),
                _ => 0.0,
            };
            let chip_left = chip_left + slide_offset;
            // Pick a body palette: active > hover > inactive. The
            // hover tier sits between so the user gets visual feedback
            // on the tab they're about to click.
            let body = if active {
                active_body
            } else if hovered {
                [
                    bg[0].saturating_add(14),
                    bg[1].saturating_add(14),
                    bg[2].saturating_add(18),
                ]
            } else {
                inactive_body
            };
            // Drag source becomes a faint "slot" outline at its
            // logical position; the cursor-following ghost is drawn
            // after the loop on top of everything.
            let alpha = if is_drag_source {
                0.25
            } else if active {
                1.0
            } else if hovered {
                0.9
            } else {
                0.75
            };
            out.push(bg::BgQuad::from_srgb_rounded(
                [chip_left, chip_top],
                [chip_width, chip_h],
                body,
                alpha * focus_mul,
                chip_radius,
            ));
            // Hover highlight on the × close marker. The close zone
            // is two cells wide (`"× "` — glyph + trailing space) but
            // the × itself sits in the *left* cell only; centring the
            // highlight over the whole zone would visually offset the
            // marker to the right of the glyph. Centre on the × cell
            // so the marker sits symmetrically around the cross, and
            // make it a true square (width == height) so the shape
            // reads the same regardless of the tab-strip height /
            // cell-width ratio (Windows headers are noticeably taller
            // than Linux defaults — a fixed `chip_h - 4` height with
            // a cell-relative width came out rectangular there).
            if hover_close_tab == Some(e.idx) {
                if let Some(state) = self.state.as_ref() {
                    let cell_w = state.text.cell_width();
                    if cell_w > 0.0 {
                        let glyph_center_x = e.body_end as f32 + cell_w * 0.5;
                        // Side: chip height bounds the upper limit
                        // (the marker can't overflow the tab strip);
                        // ~1.5 × cell_w caps the lower limit so a
                        // very tall chip doesn't grow a square wider
                        // than the × cell itself.
                        let side = (chip_h - 4.0).min(cell_w * 1.5);
                        let close_left_px = glyph_center_x - side * 0.5;
                        let close_top_px = chip_top + (chip_h - side) * 0.5;
                        let highlight = [
                            bg[0].saturating_add(40).min(180),
                            bg[1].saturating_sub(10),
                            bg[2].saturating_sub(10),
                        ];
                        out.push(bg::BgQuad::from_srgb(
                            [close_left_px, close_top_px],
                            [side, side],
                            highlight,
                            0.85,
                        ));
                    }
                }
            }
            // Static accent stripe along the TOP of the active tab
            // (VSCode convention — Chrome uses bottom, VSCode uses
            // top to signal "this tab is the active editor pane").
            // Suppressed mid-animation; sliding stripe takes over.
            if active && anim_progress.is_none() {
                let stripe_h = 2.0_f32;
                out.push(bg::BgQuad::from_srgb(
                    [chip_left, chip_top],
                    [chip_width, stripe_h],
                    active_accent,
                    focus_mul,
                ));
            }
        }
        // Cursor-following ghost chip while a drag is in flight. The
        // chip sits at `cursor.x - press_offset`, semi-transparent so
        // the user sees both the dragged tab's content and the
        // "slot" placeholder underneath (drawn faded inside the loop
        // above). Rendered after the chip loop so it sits on top of
        // every other tab body.
        if let Some(src) = dragging_idx {
            if let Some(e) = layout.entries.iter().find(|e| e.idx == src) {
                let chip_width = ((e.close_end - e.left) as f32 - gap).max(2.0);
                let ghost_left = (hover_x - self.tab_drag_press_offset) as f32 + gap * 0.5;
                // Soft shadow underneath for lift effect.
                out.push(bg::BgQuad::from_srgb_rounded(
                    [ghost_left - 3.0, chip_top + 2.0],
                    [chip_width + 6.0, chip_h + 2.0],
                    [0, 0, 0],
                    0.35 * focus_mul,
                    chip_radius + 3.0,
                ));
                out.push(bg::BgQuad::from_srgb_rounded(
                    [ghost_left, chip_top],
                    [chip_width, chip_h],
                    active_body,
                    0.92 * focus_mul,
                    chip_radius,
                ));
                // Accent stripe so the ghost reads as "active during
                // drag" — VSCode style, sits at the top of the chip.
                out.push(bg::BgQuad::from_srgb(
                    [ghost_left, chip_top],
                    [chip_width, 2.0],
                    active_accent,
                    focus_mul,
                ));
            }
        }
        // `+` new-tab chip — drawn AFTER the tab chips so it sits at
        // the trailing edge of the tab strip. Same chip styling as the
        // hamburger so the two flank the tab list symmetrically. The
        // `+` cross itself is drawn geometrically (two centered bars)
        // rather than as a text glyph — that way it lands dead-center
        // in the chip on every font without any layout-width math.
        if let Some(left) = layout.new_tab_left {
            let cell_w = self
                .state
                .as_ref()
                .map(|s| s.text.cell_width())
                .unwrap_or(0.0);
            let new_tab_w = cell_w * NEW_TAB_WIDTH_CELLS as f32;
            if new_tab_w > 0.0 {
                let hovered_new_tab = self.new_tab_button_at(hover_x, hover_y);
                let chip = [
                    bg[0].saturating_add(8),
                    bg[1].saturating_add(8),
                    bg[2].saturating_add(8),
                ];
                out.push(bg::BgQuad::from_srgb_rounded(
                    [left as f32, chip_top],
                    [new_tab_w, chip_h],
                    chip,
                    (if hovered_new_tab { 1.0 } else { 0.7 }) * focus_mul,
                    chip_radius,
                ));
                // Geometric `+`: short horizontal + vertical bars
                // centered in the chip. Arm length ≈ 40 % of the
                // chip's short axis; thickness scales with chip_h.
                let cross_color = if hovered_new_tab {
                    palette::default_fg()
                } else {
                    palette::default_fg().map(|c| c.saturating_sub(40))
                };
                let arm = (chip_h.min(new_tab_w) * 0.40).max(4.0);
                let thick = (chip_h * 0.10).clamp(1.5, 2.5);
                let cx = left as f32 + new_tab_w * 0.5;
                let cy = chip_top + chip_h * 0.5;
                // Horizontal bar.
                out.push(bg::BgQuad::from_srgb(
                    [cx - arm * 0.5, cy - thick * 0.5],
                    [arm, thick],
                    cross_color,
                    focus_mul,
                ));
                // Vertical bar.
                out.push(bg::BgQuad::from_srgb(
                    [cx - thick * 0.5, cy - arm * 0.5],
                    [thick, arm],
                    cross_color,
                    focus_mul,
                ));
            }
        }
        // Sliding accent — only visible while a tab-switch animation
        // is in flight. Interpolates the stripe rect linearly between
        // the previous active chip and the new one with ease-out.
        if let Some((anim, t)) = anim_progress {
            let from = layout.entries.iter().find(|e| e.idx == anim.from_idx);
            let to = layout.entries.iter().find(|e| e.idx == anim.to_idx);
            if let (Some(f), Some(t_e)) = (from, to) {
                let p = ease(t);
                let lerp = |a: f32, b: f32| a + (b - a) * p;
                let f_left = f.left as f32 + gap * 0.5;
                let f_w = ((f.close_end - f.left) as f32 - gap).max(2.0);
                let t_left = t_e.left as f32 + gap * 0.5;
                let t_w = ((t_e.close_end - t_e.left) as f32 - gap).max(2.0);
                let cur_left = lerp(f_left, t_left);
                let cur_w = lerp(f_w, t_w);
                let stripe_h = 2.0_f32;
                out.push(bg::BgQuad::from_srgb(
                    [cur_left, chip_top],
                    [cur_w, stripe_h],
                    active_accent,
                    1.0,
                ));
            }
        }
        // Window controls chip on the right end of the tab strip.
        // Layout: [min] gap [max] gap [close], right-anchored within
        // the same row as the tabs.
        if let Some(state) = self.state.as_ref() {
            let cell_w = state.text.cell_width();
            if cell_w > 0.0 && !self.os_decorations {
                let right = rect.left + rect.width;
                let btn_w = WINDOW_CONTROL_BUTTON_CELLS as f32 * cell_w;
                let gap_w = WINDOW_CONTROL_GAP_CELLS as f32 * cell_w;
                let close_left = right - btn_w;
                let max_left = close_left - gap_w - btn_w;
                let min_left = max_left - gap_w - btn_w;
                let dim_chip = [
                    bg[0].saturating_add(8),
                    bg[1].saturating_add(8),
                    bg[2].saturating_add(8),
                ];
                let min_hover = hover_winctl == Some(WindowControl::Minimize);
                let max_hover = hover_winctl == Some(WindowControl::MaximizeToggle);
                let close_hover = hover_winctl == Some(WindowControl::Close);
                out.push(bg::BgQuad::from_srgb_rounded(
                    [min_left, chip_top],
                    [btn_w, chip_h],
                    dim_chip,
                    (if min_hover { 1.0 } else { 0.7 }) * focus_mul,
                    chip_radius,
                ));
                out.push(bg::BgQuad::from_srgb_rounded(
                    [max_left, chip_top],
                    [btn_w, chip_h],
                    dim_chip,
                    (if max_hover { 1.0 } else { 0.7 }) * focus_mul,
                    chip_radius,
                ));
                // Close button — slightly redder background, brighter
                // red when hovered.
                let close_chip = if close_hover {
                    [220u8, 60, 60]
                } else {
                    [
                        bg[0].saturating_add(30).min(180),
                        bg[1].saturating_sub(6),
                        bg[2].saturating_sub(6),
                    ]
                };
                out.push(bg::BgQuad::from_srgb_rounded(
                    [close_left, chip_top],
                    [btn_w, chip_h],
                    close_chip,
                    (if close_hover { 1.0 } else { 0.7 }) * focus_mul,
                    chip_radius,
                ));
            }
        }
        out
    }

    /// Open the right-click context menu at (x, y). Picks the menu
    /// contents based on what's under the click: a tab, the header bar,
    /// or a pane.
    fn open_context_menu(&mut self, x: f64, y: f64) {
        // A blocking modal (paste confirmation / tab rename) owns input
        // and renders ABOVE the context menu, so a menu opened here
        // would be invisible yet still swallow the next left-click
        // (possibly firing "Close pane" etc.). Keyboard already routes
        // to the modal first; ignore the right-click to keep mouse and
        // keyboard consistent.
        if self.paste_confirmation.is_some() || self.rename_tab.is_some() {
            return;
        }
        // Close any other modal — a single overlay at a time keeps the
        // input routing simple. show_app_menu cleared too so the
        // hamburger glyph isn't highlighted while a different menu is
        // up.
        self.show_help = false;
        self.show_settings = false;
        self.show_app_menu = false;
        if self.palette.is_some() {
            self.close_palette();
        }
        let items = if let Some((idx, _hit)) = self.tab_hit_at(x, y) {
            // Right-click on a tab targets THAT tab even if it's not
            // the active one — Close, Move left/right.
            let _ = idx; // we apply to the active tab post-click after focusing
            self.select_tab(idx);
            tab_context_items()
        } else if self.header_rect().is_some_and(|r| {
            let yf = y as f32;
            yf >= r.top && yf < r.top + r.height
        }) {
            header_context_items()
        } else {
            let has_selection = self.selection_text().is_some();
            pane_context_items(has_selection)
        };
        self.context_menu = Some(ContextMenu {
            anchor: (x as f32, y as f32),
            items,
            hovered: None,
        });
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }

    /// Close the context menu, no action.
    fn close_context_menu(&mut self) {
        if self.context_menu.take().is_some() {
            self.show_app_menu = false;
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
        }
    }

    /// Hit-test: return the item index under (x, y), or `None` when
    /// outside the menu rect (or over a separator row).
    fn context_menu_item_at(&self, menu: &ContextMenu, x: f64, y: f64) -> Option<usize> {
        let rect = self.context_menu_rect(menu)?;
        let state = self.state.as_ref()?;
        let line_h = state.text.line_height() as f64;
        let xf = x as f32;
        let yf = y as f32;
        if xf < rect.left || xf >= rect.left + rect.width {
            return None;
        }
        if yf < rect.top || yf >= rect.top + rect.height {
            return None;
        }
        let row = ((y - rect.top as f64) / line_h).floor() as usize;
        let item = menu.items.get(row)?;
        match item {
            MenuItem::Separator => None,
            MenuItem::Action { enabled, .. } if !*enabled => None,
            MenuItem::Action { .. } => Some(row),
        }
    }

    /// Mouse press while a context menu is open. Click on a row runs
    /// its action; click outside dismisses.
    fn handle_context_menu_press(&mut self, x: f64, y: f64) -> bool {
        let menu = match self.context_menu.as_ref() {
            Some(m) => m.clone(),
            None => return false,
        };
        if let Some(idx) = self.context_menu_item_at(&menu, x, y) {
            if let Some(MenuItem::Action { action, enabled: true, .. }) = menu.items.get(idx) {
                let act = *action;
                self.context_menu = None;
                self.show_app_menu = false;
                return self.dispatch_action(act);
            }
        }
        // Click outside → close.
        self.context_menu = None;
        self.show_app_menu = false;
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
        false
    }

    /// Keyboard input while a context menu is open: arrows + Enter.
    fn handle_context_menu_key(&mut self, event: &KeyEvent) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        let key = &event.logical_key;
        if matches!(key, Key::Named(NamedKey::Escape)) {
            self.close_context_menu();
            return false;
        }
        if matches!(key, Key::Named(NamedKey::Enter)) {
            let act_idx = self
                .context_menu
                .as_ref()
                .and_then(|m| m.hovered)
                .and_then(|i| match self.context_menu.as_ref()?.items.get(i)? {
                    MenuItem::Action { action, enabled: true, .. } => Some(*action),
                    _ => None,
                });
            if let Some(act) = act_idx {
                self.context_menu = None;
                self.show_app_menu = false;
                return self.dispatch_action(act);
            }
            return false;
        }
        let dir: isize = match key {
            Key::Named(NamedKey::ArrowDown) => 1,
            Key::Named(NamedKey::ArrowUp) => -1,
            _ => return false,
        };
        if let Some(menu) = self.context_menu.as_mut() {
            let n = menu.items.len();
            let start = menu.hovered.map(|i| i as isize).unwrap_or(if dir > 0 { -1 } else { n as isize });
            let mut i = start;
            for _ in 0..n {
                i += dir;
                if i < 0 {
                    i = n as isize - 1;
                }
                if i >= n as isize {
                    i = 0;
                }
                if matches!(menu.items.get(i as usize), Some(MenuItem::Action { enabled: true, .. })) {
                    menu.hovered = Some(i as usize);
                    break;
                }
            }
        }
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
        false
    }

    /// Update mouse-hover on the context menu when the cursor moves.
    fn update_context_menu_hover(&mut self, x: f64, y: f64) {
        let menu = match self.context_menu.as_ref() {
            Some(m) => m.clone(),
            None => return,
        };
        let new_hover = self.context_menu_item_at(&menu, x, y);
        if let Some(m) = self.context_menu.as_mut() {
            if m.hovered != new_hover {
                m.hovered = new_hover;
                if let Some(state) = self.state.as_ref() {
                    state.window.request_redraw();
                }
            }
        }
    }

    /// Build the STATIC header chrome (hamburger only). Tab labels
    /// live in `header_tabs_spans` and are rendered into a separate
    /// buffer so they can slide with `tab_scroll_offset` without
    /// dragging the hamburger glyph along. The hover-URL / cwd hint
    /// that previously sat to the right of the tab strip is now
    /// surfaced via the bottom status bar.
    fn header_spans<'a>(&self, storage: &'a mut Vec<String>) -> Vec<(&'a str, [u8; 3], bool)> {
        storage.clear();
        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        storage.push(HAMBURGER_GLYPH.to_string());
        let hb_color = if self.show_app_menu {
            palette::default_fg()
        } else {
            palette::default_fg().map(|c| c.saturating_sub(40))
        };
        spans.push((storage.len() - 1, hb_color, self.show_app_menu));
        spans
            .into_iter()
            .map(|(idx, color, bold)| (storage[idx].as_str(), color, bold))
            .collect()
    }

    /// Build the tab-label text + per-span colour. Active tab is
    /// rendered bold in a brighter colour; others muted. Lives in a
    /// dedicated span list so the renderer can shape these into a
    /// scroll-tracking buffer (see `HeaderTabsDraw`).
    fn header_tabs_spans<'a>(
        &self,
        storage: &'a mut Vec<String>,
    ) -> Vec<(&'a str, [u8; 3], bool)> {
        storage.clear();
        let muted = palette::default_fg().map(|c| c.saturating_sub(80));
        let activity_accent: [u8; 3] = [255, 204, 102]; // warm yellow
        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            let info = self.tab_label(i, tab);
            let (active, activity) = (info.active, info.activity);
            storage.push(info.label);
            let color = if active {
                palette::default_fg()
            } else if activity {
                activity_accent
            } else {
                muted
            };
            // Tab drag-reorder visual feedback: dim the source tab so
            // the user sees which one is being moved. Without this the
            // entire row looks identical mid-drag and there's no signal
            // that the drag is active (the cursor icon change is easy
            // to miss when the eye is on the tab strip itself).
            let color = if self.tab_dragging == Some(i) {
                color.map(|c| c.saturating_sub(80))
            } else {
                color
            };
            spans.push((storage.len() - 1, color, active));
            // OSC 9;4 aggregate badge: pick the most-severe pane's
            // progress, render as one extra coloured span right after
            // the tab label. State 0/None → no badge.
            if let Some((state, pct)) = tab
                .panes()
                .iter()
                .filter_map(|p| p.progress.lock().ok().and_then(|g| *g))
                .max_by_key(|(s, p)| (progress_severity(*s), *p))
            {
                let red: [u8; 3] = [241, 76, 76];
                let yellow: [u8; 3] = [255, 204, 102];
                let blue: [u8; 3] = [120, 180, 240];
                let (glyph, color) = match state {
                    2 => ("✗ ".to_string(), red),
                    4 => ("⚠ ".to_string(), yellow),
                    3 => ("⋯ ".to_string(), blue),
                    1 => (format!("{}% ", pct), activity_accent),
                    _ => (String::new(), muted),
                };
                if !glyph.is_empty() {
                    storage.push(glyph);
                    spans.push((storage.len() - 1, color, false));
                }
            }
            // Trailing "× " close marker. Dim by default; brighter on the
            // active tab so the user notices it's clickable. Width is
            // pinned to `TAB_CLOSE_WIDTH_CELLS` so `tab_hit_at` agrees.
            storage.push(TAB_CLOSE_GLYPH.to_string());
            let close_color = if active {
                palette::default_fg().map(|c| c.saturating_sub(40))
            } else {
                muted
            };
            spans.push((storage.len() - 1, close_color, false));
        }
        spans
            .into_iter()
            .map(|(idx, color, bold)| (storage[idx].as_str(), color, bold))
            .collect()
    }

    /// Build the single-tab spans for the cursor-following ghost
    /// label rendered during a tab drag. Returns `None` when no
    /// drag is in flight (or the source index is stale). Reuses
    /// `tab_label` so the ghost text matches whatever
    /// `header_tabs_spans` would have drawn for that tab — same
    /// prefix / pane count / zoom marker — just at full intensity
    /// instead of the dimmed-source colour.
    fn header_tabs_ghost_spans<'a>(
        &self,
        storage: &'a mut Vec<String>,
    ) -> Option<Vec<(&'a str, [u8; 3], bool)>> {
        let idx = self.tab_dragging?;
        let tab = self.tabs.get(idx)?;
        let info = self.tab_label(idx, tab);
        storage.clear();
        storage.push(info.label);
        // Active tab gets full default-fg; non-active uses the same
        // muted colour the regular strip uses for inactive tabs.
        // Activity-yellow and progress badges are intentionally
        // dropped from the ghost — those signal "background news",
        // which is incongruous on a chip the user is actively
        // moving.
        let color = if info.active {
            palette::default_fg()
        } else {
            palette::default_fg().map(|c| c.saturating_sub(80))
        };
        Some(vec![(storage[0].as_str(), color, info.active)])
    }

    fn sync_terminal_size(&self) {
        let Some(state) = self.state.as_ref() else { return };
        let Some(tab) = self.active_tab() else { return };
        let rects = self.layout_active_tab();
        if rects.is_empty() {
            return;
        }
        let cell_w = state.text.cell_width();
        let line_h = state.text.line_height();
        for (idx, (pane, rect)) in tab.panes().into_iter().zip(rects.iter()).enumerate() {
            // Zoomed-out panes report a 0×0 rect; preserve their previous size
            // so their backing programs don't see a transient 1×1 reflow.
            if rect.width <= 0.5 || rect.height <= 0.5 {
                continue;
            }
            // Honour the actual visible cell count, but with a tiny
            // safety floor (10 cols × 3 rows). On wide windows the
            // floor never kicks in — cols is just the real count and
            // long output wraps at the natural window width. On very
            // narrow windows (some Wayland compositors honour
            // `with_min_inner_size` only as a hint), the floor stops
            // shells from receiving `cols=1` and writing every
            // character onto its own line — which becomes permanent
            // scrollback "s\nl\na\nv\na\n..." garbage.
            //
            // The floor MUST stay tiny. It was once bumped to 80,
            // which silently lied to the shell for every pane
            // narrower than 80 cells (any side-by-side split, large
            // fonts, HiDPI): output formatted for 80 columns wrapped
            // past the visible pane edge, glyph rows soft-wrapped
            // onto phantom lines, and mouse hit-tests addressed
            // columns that don't exist on screen.
            const MIN_COLS: u16 = 10;
            const MIN_ROWS: u16 = 3;
            let cols = ((rect.width / cell_w).max(1.0) as u16).max(MIN_COLS);
            let rows = ((rect.height / line_h).max(1.0) as u16).max(MIN_ROWS);
            let mut changed = false;
            if let Ok(mut term) = pane.terminal.lock() {
                let prev = term.size();
                if prev.cols != cols || prev.rows != rows {
                    term.resize(TermSize { cols, rows });
                    changed = true;
                }
            }
            // Only forward to the PTY when the integer cell grid
            // actually changed. Without this gate, every CursorMoved
            // event during a gap-drag fires a SIGWINCH at the shell
            // even when the cols/rows didn't move — the shell reprints
            // its prompt each time and the user sees a cascade of
            // partial prompts in the scrollback.
            if changed {
                pane.io.resize(cols, rows);
            }
            if changed {
                // Legacy `resize` event: payload `<cols>x<rows>`. Fires
                // for every pane whose grid actually changed dimensions
                // — useful as a coarse "the layout reflowed" signal.
                self.events.emit("resize", &format!("{cols}x{rows}"));
                // Pane-scoped variant for plugins that care which pane
                // changed (and to which size). Same convention as the
                // other `pane.*` events:
                // `<tab+1>:<pane+1>\t<cols>x<rows>\t<uid>`.
                // The trailing uid lets plugins answer "did MY pane
                // just resize?" without re-resolving the index pair.
                self.events.emit(
                    "pane.resize",
                    &format!(
                        "{}:{}\t{}x{}\t{}",
                        self.active_tab + 1,
                        idx + 1,
                        cols,
                        rows,
                        pane.uid,
                    ),
                );
            }
        }
    }

    fn update_title(&self) {
        let Some(state) = self.state.as_ref() else { return };
        // Plugin override from `rterm.set_window_title` wins over the
        // auto-derived label.
        if let Some(custom) = self.custom_window_title.as_ref() {
            state.window.set_title(custom);
            return;
        }
        if self.tabs.is_empty() {
            state.window.set_title(&self.title);
            return;
        }
        // `active_tab()` should be `Some` after the empty guard above,
        // but a stale `self.active_tab` index (set by a bug elsewhere)
        // would still return `None` and the original `.expect` would
        // panic. Take the first tab as a last-resort fallback — the
        // title may be slightly stale for one frame but the window
        // stays up.
        let Some(tab) = self.active_tab().or_else(|| self.tabs.first()) else {
            state.window.set_title(&self.title);
            return;
        };
        let pane_title = tab
            .focused_pane()
            .map(|p| p.display_title())
            .unwrap_or_default();
        // Skip count segments when there's only one tab AND one pane —
        // "tab 1/1 · pane 1/1" is just noise on a fresh launch. Single-
        // tab-multi-pane and multi-tab-single-pane both keep the
        // relevant counter so the user can still tell where they are.
        let tab_count = self.tabs.len();
        let pane_count = tab.pane_count();
        let pane_idx = tab.focused_index().map(|i| i + 1).unwrap_or(0);
        let t = match (tab_count, pane_count) {
            (1, 1) => format!("{} — {}", self.title, pane_title),
            (1, _) => format!(
                "{} — pane {}/{} · {}",
                self.title, pane_idx, pane_count, pane_title,
            ),
            (_, 1) => format!(
                "{} — tab {}/{} · {}",
                self.title,
                self.active_tab + 1,
                tab_count,
                pane_title,
            ),
            _ => format!(
                "{} — tab {}/{} · pane {}/{} · {}",
                self.title,
                self.active_tab + 1,
                tab_count,
                pane_idx,
                pane_count,
                pane_title,
            ),
        };
        state.window.set_title(&t);
    }

    fn spawn_pane(&self, cwd: Option<&str>) -> Option<Pane> {
        match self.spawner.spawn_pane(cwd) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("spawn pane failed: {e:#}");
                None
            }
        }
    }

    /// Snapshot the focused pane's last-known cwd (from OSC 7), if any.
    fn focused_cwd(&self) -> Option<String> {
        let pane = self.focused_pane()?;
        let from_osc7 = pane
            .terminal
            .lock()
            .ok()
            .and_then(|t| t.cwd().map(|s| s.to_string()));
        // OSC-7-less shells (dash, some fish setups) never advertise cwd.
        // Fall back to `/proc/<pid>/cwd` on Linux so split/new-tab still
        // inherits the right directory; without this, every split starts
        // at the process cwd instead of "wherever the user is".
        from_osc7.or_else(|| pid_cwd_fallback(pane.io.process_id()))
    }

    fn new_tab(&mut self) {
        let cwd = self.focused_cwd();
        self.new_tab_in(cwd.as_deref());
    }

    /// Spawn a new tab using `cwd` for the initial pane's working directory.
    /// Falls back to the spawner's default (typically the process cwd) when
    /// `cwd` is `None`.
    fn new_tab_in(&mut self, cwd: Option<&str>) {
        let Some(pane) = self.spawn_pane(cwd) else { return };
        // Remember where the user came from so Ctrl+Shift+Tab toggles
        // back to it. Without this, opening a new tab strands the
        // previous focus until the user manually navigates back.
        let prev = if self.tabs.is_empty() {
            None
        } else {
            Some(self.active_tab)
        };
        self.tabs.push(Tab {
            tree: tree::Tree::new(pane),
            focus_path: Vec::new(),
            zoomed: false,
            custom_title: None,
            unread: false,
            silence_armed: false,
            last_any_alt: AtomicBool::new(false),
            last_progress: Mutex::new(None),
        });
        self.active_tab = self.tabs.len() - 1;
        self.previous_tab = prev;
        self.sync_terminal_size();
        self.update_title();
        // A freshly-added tab is the last one in the strip; if the
        // user already scrolled the strip, the new tab might appear
        // off the right edge. Scroll forward so it stays visible.
        self.ensure_active_tab_visible();
        // Payload: `<tab_1based>\t<cwd>` — empty cwd field means the
        // spawner used its default (typically the process cwd).
        self.events.emit(
            "tab.new",
            &format!("{}\t{}", self.active_tab + 1, cwd.unwrap_or("")),
        );
    }

    /// Close the tab at `idx` (0-based). Returns `true` if the window
    /// should exit (last tab closed). Keeps the previously-focused tab
    /// active when possible; if the closed tab was active, falls back to
    /// the same rules as `close_active_tab`.
    fn close_tab_at(&mut self, idx: usize) -> bool {
        if idx >= self.tabs.len() {
            return false;
        }
        let prev_active = self.active_tab;
        self.active_tab = idx;
        let exit = self.close_active_tab();
        if exit || self.tabs.is_empty() {
            return exit;
        }
        // Restore the user's previous focus where it ended up after the
        // remove() shift, unless they had been on the closed tab itself.
        if prev_active != idx {
            let restore = if prev_active > idx { prev_active - 1 } else { prev_active };
            if restore < self.tabs.len() {
                self.active_tab = restore;
                self.sync_terminal_size();
                self.update_title();
            }
        }
        false
    }

    fn close_active_tab(&mut self) -> bool {
        if self.tabs.is_empty() {
            return true;
        }
        let closed_idx = self.active_tab;
        self.tabs.remove(self.active_tab);
        // Forget previous_tab if it pointed at the closed slot or past
        // the new vec end — avoids ToggleLastTab landing on a stale index.
        if let Some(p) = self.previous_tab {
            if p == closed_idx || p >= self.tabs.len() {
                self.previous_tab = None;
            } else if p > closed_idx {
                self.previous_tab = Some(p - 1);
            }
        }
        if self.tabs.is_empty() {
            self.events.emit("tab.close", &(closed_idx + 1).to_string());
            return true;
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
        self.sync_terminal_size();
        self.update_title();
        self.events.emit("tab.close", &(closed_idx + 1).to_string());
        false
    }

    /// Move the active tab `delta` positions left (-1) or right (+1) in the
    /// tab strip. Active focus follows the moved tab.
    fn move_active_tab(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        let target = self.active_tab as isize + delta;
        if target < 0 || target >= n {
            return;
        }
        let target = target as usize;
        let from = self.active_tab;
        self.tabs.swap(from, target);
        self.active_tab = target;
        // Remap `previous_tab` through the swap so toggle-last-tab keeps
        // pointing at the right entry after a reorder.
        if let Some(p) = self.previous_tab {
            if p == from {
                self.previous_tab = Some(target);
            } else if p == target {
                self.previous_tab = Some(from);
            }
        }
        self.update_title();
        self.events.emit("tab.move", &(self.active_tab + 1).to_string());
    }

    fn switch_tab(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        let next = (((self.active_tab as isize + delta) % n + n) % n) as usize;
        if next != self.active_tab {
            self.previous_tab = Some(self.active_tab);
        }
        self.active_tab = next;
        self.reset_cursor_blink();
        self.sync_terminal_size();
        self.update_title();
        self.mark_tab_read(self.active_tab);
        // Keep the newly-active tab visible inside the (possibly
        // scrolled) tab strip — symmetric with VSCode / Chrome.
        self.ensure_active_tab_visible();
        // The search overlay's `pane_idx` referenced the old tab. Keeping
        // it open across the switch would silently re-target a same-index
        // pane in the new tab. Close it instead so a re-open re-anchors.
        self.end_search();
        let payload = self.tab_switch_payload(self.active_tab);
        self.events.emit("tab.switch", &payload);
    }

    /// Jump to a tab by 0-based index. No-op if `idx` is out of range so
    /// `Ctrl+Shift+5` on a 3-tab window stays put rather than wrapping.
    fn select_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() || idx == self.active_tab {
            return;
        }
        let from = self.active_tab;
        self.previous_tab = Some(self.active_tab);
        self.active_tab = idx;
        self.tab_switch_anim = Some(TabSwitchAnim {
            from_idx: from,
            to_idx: idx,
            started_at: Instant::now(),
        });
        self.reset_cursor_blink();
        self.sync_terminal_size();
        self.update_title();
        self.mark_tab_read(self.active_tab);
        self.ensure_active_tab_visible();
        // The search overlay's `pane_idx` referenced the old tab. Keeping
        // it open across the switch would silently re-target a same-index
        // pane in the new tab. Close it instead so a re-open re-anchors.
        self.end_search();
        let payload = self.tab_switch_payload(self.active_tab);
        self.events.emit("tab.switch", &payload);
    }

    /// Swap focus with the most recently visited tab. No-op when only one
    /// tab is alive or no previous tab is remembered yet.
    fn toggle_last_tab(&mut self) {
        let Some(prev) = self.previous_tab else { return };
        if prev >= self.tabs.len() || prev == self.active_tab {
            return;
        }
        let from = self.active_tab;
        self.previous_tab = Some(self.active_tab);
        self.active_tab = prev;
        self.tab_switch_anim = Some(TabSwitchAnim {
            from_idx: from,
            to_idx: prev,
            started_at: Instant::now(),
        });
        self.reset_cursor_blink();
        self.sync_terminal_size();
        self.update_title();
        self.mark_tab_read(self.active_tab);
        // The search overlay's `pane_idx` referenced the old tab. Keeping
        // it open across the switch would silently re-target a same-index
        // pane in the new tab. Close it instead so a re-open re-anchors.
        self.end_search();
        let payload = self.tab_switch_payload(self.active_tab);
        self.events.emit("tab.switch", &payload);
    }

    /// Split the focused pane in the active tab in the chosen direction.
    /// With BSP each split is independent so different siblings can have
    /// different orientations — `Ctrl+Shift+D` and `Ctrl+Shift+E` always
    /// honour the requested direction now.
    /// Split the focused pane, choosing the split direction from the pane's
    /// current aspect ratio: wider than tall → horizontal (new pane to the
    /// right); taller → vertical (new pane below). Mirrors tmux's "smart
    /// split" behaviour so users only need one shortcut.
    fn split_active_pane_auto(&mut self) {
        let dir = self.split_auto_direction();
        self.split_active_pane(dir);
    }

    fn split_auto_direction(&self) -> SplitDir {
        self.focused_pane_rect()
            .map(|r| if r.width >= r.height { SplitDir::Horizontal } else { SplitDir::Vertical })
            .unwrap_or(SplitDir::Horizontal)
    }

    /// Pixel rect of the currently focused pane in the active tab, if any.
    fn focused_pane_rect(&self) -> Option<PaneRect> {
        let rects = self.layout_active_tab();
        let idx = self.active_tab().and_then(|t| t.focused_index())?;
        rects.get(idx).copied()
    }

    fn split_active_pane(&mut self, dir: SplitDir) {
        let cwd = self.focused_cwd();
        self.split_active_pane_in(dir, cwd.as_deref());
    }

    /// Split the focused pane, spawning the new child in `cwd`. Falls back to
    /// the spawner's default when `cwd` is `None` (i.e. process cwd).
    fn split_active_pane_in(&mut self, dir: SplitDir, cwd: Option<&str>) {
        let Some(pane) = self.spawn_pane(cwd) else { return };
        let active_tab_index = self.active_tab;
        let Some(tab) = self.tabs.get_mut(active_tab_index) else { return };
        let path = tab.focus_path.clone();
        if !tab.tree.split_leaf(&path, pane, dir, 0.5) {
            return;
        }
        // The new pane lives on the b-side of the new Split.
        tab.focus_path.push(true);
        // Splitting a zoomed pane would hide the new sibling under the zoom
        // mask — drop zoom so the new layout is visible. Capture the
        // previous value so plugins watching `pane.zoom` get the
        // off transition (the split-as-side-effect can otherwise
        // de-sync a "Z" indicator in a custom tab bar).
        let was_zoomed = std::mem::replace(&mut tab.zoomed, false);
        let new_pane_idx = self
            .active_tab()
            .and_then(|t| t.focused_index())
            .unwrap_or(0);
        // Capture the new pane's stable uid before any further frame
        // work — included in the event so uid-aware plugins can stash
        // it for long-term addressing without a follow-up list_panes().
        let new_pane_uid = self
            .active_tab()
            .and_then(|t| t.pane_at(new_pane_idx))
            .map(|p| p.uid)
            .unwrap_or(0);
        self.sync_terminal_size();
        self.update_title();
        let payload = pane_split_payload(
            self.active_tab + 1,
            new_pane_idx + 1,
            dir,
            new_pane_uid,
        );
        self.events.emit("pane.split", &payload);
        if was_zoomed {
            self.events.emit("pane.zoom", "off");
        }
    }

    fn toggle_zoom(&mut self) {
        let Some(tab) = self.active_tab_mut() else { return };
        if tab.pane_count() <= 1 {
            tab.zoomed = false;
            return;
        }
        tab.zoomed = !tab.zoomed;
        let zoomed = tab.zoomed;
        self.sync_terminal_size();
        self.update_title();
        self.events.emit(
            "pane.zoom",
            if zoomed { "on" } else { "off" },
        );
    }

    /// Build the standard `pane.focus` event payload for a `(tab, pane)`
    /// pair: `<tab+1>:<pane+1>\t<uid>`. The uid is looked up live so the
    /// caller doesn't need to hold a reference to the Pane. `0` when no
    /// pane sits at the requested index (matches the "no pane" sentinel
    /// from `Pane::new`).
    fn pane_focus_payload(&self, tab_idx: usize, pane_idx: usize) -> String {
        let uid = self
            .tabs
            .get(tab_idx)
            .and_then(|t| t.pane_at(pane_idx))
            .map(|p| p.uid)
            .unwrap_or(0);
        pane_edge_payload(tab_idx + 1, pane_idx + 1, uid)
    }

    /// Walk the tab tree and return the `(tab_idx, pane_idx)` 0-based
    /// pair of the pane with the given stable uid, or `None` when
    /// the uid no longer points at a live pane. Centralised so the
    /// uid-resolution loop lives in one place instead of being
    /// open-coded at each `*_by_uid` drain site.
    fn find_pane_indices_by_uid(&self, uid: u64) -> Option<(usize, usize)> {
        for (ti, tab) in self.tabs.iter().enumerate() {
            for (pi, pane) in tab.panes().into_iter().enumerate() {
                if pane.uid == uid {
                    return Some((ti, pi));
                }
            }
        }
        None
    }

    /// Higher-level uid → `&Pane` lookup. Combines
    /// `find_pane_indices_by_uid` with `pane_at` so callers that
    /// don't need the indices themselves get the live pane
    /// reference in one call. Returns `None` for vanished uids.
    fn find_pane_by_uid(&self, uid: u64) -> Option<&Pane> {
        let (ti, pi) = self.find_pane_indices_by_uid(uid)?;
        self.tabs.get(ti).and_then(|t| t.pane_at(pi))
    }

    /// Clear a tab's `unread` flag and, if it was actually set,
    /// fire `tab.read` (the inverse of `tab.unread`). Centralised
    /// so each "tab regained focus" path emits exactly once via
    /// one code path — adding a fifth such path later doesn't risk
    /// silently dropping the event.
    fn mark_tab_read(&mut self, tab_idx: usize) {
        let prev = self
            .tabs
            .get_mut(tab_idx)
            .map(|t| std::mem::replace(&mut t.unread, false))
            .unwrap_or(false);
        if prev {
            self.events.emit("tab.read", &(tab_idx + 1).to_string());
        }
    }

    /// Build the standard `tab.switch` event payload: `<tab+1>\t<uid>`,
    /// where `<uid>` is the uid of the tab's currently focused pane (or
    /// `0` when the tab is transiently empty). Plugins tracking by uid
    /// can react to "the user just switched to the tab containing my
    /// watched pane" without a follow-up `list_panes()` walk.
    fn tab_switch_payload(&self, tab_idx: usize) -> String {
        let uid = self
            .tabs
            .get(tab_idx)
            .and_then(|t| t.focused_index().and_then(|i| t.pane_at(i)))
            .map(|p| p.uid)
            .unwrap_or(0);
        tab_event_payload(tab_idx + 1, uid)
    }

    /// Returns true if the whole window should close (last pane of last tab gone).
    fn close_focused_pane(&mut self) -> bool {
        let active_tab_index = self.active_tab;
        let mut closed_pane_idx: usize = 0;
        // Capture the closed pane's uid before the tree drops the leaf
        // so the `pane.close` event can carry the stable identifier.
        // `0` is the documented "no pane" sentinel for the missing-tab
        // / empty-tab edge cases.
        let mut closed_uid: u64 = 0;
        // Captured for the post-borrow `pane.zoom off` emission
        // so closing a pane that happened to be zoomed sends the
        // edge event just like the explicit `toggle_zoom` does.
        let mut was_zoomed = false;
        let tab_empty = if let Some(tab) = self.tabs.get_mut(active_tab_index) {
            if tab.pane_count() == 0 {
                true
            } else {
                closed_pane_idx = tab.focused_index().unwrap_or(0);
                let path = tab.focus_path.clone();
                closed_uid = tab
                    .tree
                    .leaf_at(&path)
                    .map(|p| p.uid)
                    .unwrap_or(0);
                let _ = tab.tree.close_leaf(&path);
                // Walk back up: parent slot now holds the sibling subtree;
                // descend to its leftmost leaf for focus.
                let parent_path = if path.is_empty() {
                    Vec::new()
                } else {
                    path[..path.len() - 1].to_vec()
                };
                tab.focus_path = descend_leftmost(&tab.tree, &parent_path);
                // Closing reshapes the tree; drop zoom so the surviving
                // siblings are visible again.
                was_zoomed = std::mem::replace(&mut tab.zoomed, false);
                tab.pane_count() == 0
            }
        } else {
            return true;
        };
        if tab_empty {
            return self.close_active_tab();
        }
        self.sync_terminal_size();
        self.update_title();
        // Payload: `<tab>:<pane>\t<uid>`. Additive — legacy
        // `(%d+):(%d+)` parsers still extract tab/pane from the prefix.
        self.events.emit(
            "pane.close",
            &pane_edge_payload(self.active_tab + 1, closed_pane_idx + 1, closed_uid),
        );
        if was_zoomed {
            self.events.emit("pane.zoom", "off");
        }
        false
    }

    /// Remove panes whose reader thread reported the PTY closed. Returns
    /// `true` if there are no panes left and the window should exit.
    fn prune_dead_panes(&mut self) -> bool {
        // Last known shell exit code (from OSC 133;D) captured before the
        // pane is dropped, so the `pane.exit` event payload can surface
        // "what was the last thing this shell ran" — `None` when the
        // pane never finished a command (e.g. user typed `exit` at a
        // pristine prompt, or the PTY died before the shell ran anything).
        // uid is captured at the same time so uid-tracking plugins can
        // match the event to a previously-stashed identifier.
        let mut exited: Vec<(usize, usize, Option<i32>, u64)> = Vec::new();
        for (tab_idx, tab) in self.tabs.iter_mut().enumerate() {
            // Closing a leaf shifts paths; iterate one-at-a-time.
            loop {
                let paths = tab.tree.leaf_paths();
                let dead = paths.iter().enumerate().find(|(_, p)| {
                    tab.tree
                        .leaf_at(p)
                        .map(|leaf| !leaf.alive.load(Ordering::Relaxed))
                        .unwrap_or(false)
                });
                let Some((dead_idx, path)) = dead else { break };
                let path = path.clone();
                let (exit_code, uid) = tab
                    .tree
                    .leaf_at(&path)
                    .map(|leaf| {
                        (
                            leaf.last_exit_code.lock().ok().and_then(|g| *g),
                            leaf.uid,
                        )
                    })
                    .unwrap_or((None, 0));
                exited.push((tab_idx, dead_idx, exit_code, uid));
                // Capture the focused pane's identity BEFORE the tree
                // mutates, then let `repair_focus_after_close` re-find
                // it by uid. The old prefix check only repaired focus
                // when it pointed INTO the dead subtree; focus inside
                // the hoisted sibling kept a stale extra branch bit,
                // `focused_pane()` returned `None`, and keyboard input
                // silently died until the user clicked.
                let focused_uid = tab.focused_pane().map(|p| p.uid);
                let _ = tab.tree.close_leaf(&path);
                tab.repair_focus_after_close(&path, focused_uid);
            }
        }
        // Track which tab indices disappear so plugins still see tab.close.
        let mut removed_tabs: Vec<usize> = Vec::new();
        let mut kept: Vec<Tab> = Vec::with_capacity(self.tabs.len());
        for (i, t) in self.tabs.drain(..).enumerate() {
            if t.pane_count() > 0 {
                kept.push(t);
            } else {
                removed_tabs.push(i);
            }
        }
        self.tabs = kept;
        for idx in &removed_tabs {
            self.events.emit("tab.close", &(idx + 1).to_string());
        }
        // Same index-shift dance as `previous_tab` below: if the active
        // tab itself was removed, fall back to the last remaining one;
        // otherwise shift left by the number of removed slots that
        // preceded it so the user stays on the *same* visual tab even
        // when an earlier sibling disappeared.
        if !self.tabs.is_empty() {
            if removed_tabs.contains(&self.active_tab) {
                self.active_tab = self.tabs.len() - 1;
            } else {
                let shift = removed_tabs.iter().filter(|&&i| i < self.active_tab).count();
                self.active_tab = (self.active_tab - shift).min(self.tabs.len() - 1);
            }
        }
        // Adjust the "previous tab" pointer for the removals. Three cases:
        //   - prev was one of the removed tabs → forget it (None)
        //   - prev was after one or more removed tabs → shift left by the
        //     number of removed-index slots that were below it (so it
        //     still points at the *same* Tab even though indices moved)
        //   - prev was before all removed tabs → leave as-is
        // Without the shift, ToggleLastTab would jump to a wrong-tab
        // sibling whenever an earlier tab in the row closed.
        if let Some(p) = self.previous_tab {
            if removed_tabs.contains(&p) {
                self.previous_tab = None;
            } else {
                let shift = removed_tabs.iter().filter(|&&i| i < p).count();
                let new_p = p - shift;
                self.previous_tab = if new_p < self.tabs.len() {
                    Some(new_p)
                } else {
                    None
                };
            }
        }
        if self.tabs.is_empty() {
            return true;
        }
        if !exited.is_empty() {
            self.sync_terminal_size();
            self.update_title();
            for (tab_idx, pane_idx, exit_code, uid) in exited {
                let payload = pane_exit_payload(tab_idx + 1, pane_idx + 1, exit_code, uid);
                self.events.emit("pane.exit", &payload);
            }
            // Focus may have moved to a different leaf when the closed
            // pane was the focused one (or an ancestor). Surface that
            // as a `pane.focus` event so status-line plugins update —
            // without this, a plugin tracking pane.focus would still
            // think the dead pane is selected until the next switch.
            if let Some(tab) = self.tabs.get(self.active_tab) {
                if let Some(idx) = tab.focused_index() {
                    let payload = self.pane_focus_payload(self.active_tab, idx);
                    self.events.emit("pane.focus", &payload);
                }
            }
            // Drop selection / search state if it referred to a pane that
            // no longer exists in the active tab. Without this the next
            // render frame would dereference a stale `pane_idx` and either
            // miss the pane (silent) or — for search — re-scan whichever
            // pane now sits at that index.
            let active_pane_count = self
                .tabs
                .get(self.active_tab)
                .map(|t| t.pane_count())
                .unwrap_or(0);
            if let Some(sel) = self.selection.as_ref() {
                if sel.pane_idx >= active_pane_count {
                    self.selection = None;
                }
            }
            if let Some(s) = self.search.as_ref() {
                if s.pane_idx >= active_pane_count {
                    self.end_search();
                }
            }
        }
        false
    }

    /// Scroll the focused pane so the previous (or next) shell prompt
    /// (captured via OSC 133;A) lands at the top of the viewport. `dir < 0`
    /// jumps back, `dir > 0` jumps forward.
    fn jump_to_prompt(&self, dir: isize) {
        self.jump_to_mark(dir, MarkKind::Prompt);
    }

    /// Same as `jump_to_prompt` but uses OSC 133;C command-start marks.
    fn jump_to_command(&self, dir: isize) {
        self.jump_to_mark(dir, MarkKind::Command);
    }

    fn jump_to_mark(&self, dir: isize, kind: MarkKind) {
        let Some(pane) = self.focused_pane() else { return };
        let Ok(term) = pane.terminal.lock() else { return };
        // Marks live in primary's scrollback / grid; on alt screen the
        // viewport is pinned to the alt grid so jumping there is a no-op.
        if term.is_on_alt_screen() {
            return;
        }
        let sb_len = term.scrollback_len();
        let cur_offset = pane.scroll_offset.load(Ordering::Relaxed) as usize;
        let top_line = sb_len.saturating_sub(cur_offset);
        let marks = match kind {
            MarkKind::Prompt => term.prompt_marks(),
            MarkKind::Command => term.command_marks(),
        };
        let target = if dir < 0 {
            marks.iter().filter(|&&m| m < top_line).max().copied()
        } else {
            marks.iter().filter(|&&m| m > top_line).min().copied()
        };
        drop(term);
        let Some(m) = target else { return };
        let new_off = sb_len.saturating_sub(m).min(u16::MAX as usize) as u16;
        pane.scroll_offset.store(new_off, Ordering::Relaxed);
        let event = match kind {
            MarkKind::Prompt => "prompt.jump",
            MarkKind::Command => "command.jump",
        };
        self.events.emit(event, &m.to_string());
    }

    fn focus_pane(&mut self, delta: isize) {
        let new_index = {
            let Some(tab) = self.active_tab_mut() else { return };
            let paths = tab.tree.leaf_paths();
            if paths.is_empty() {
                return;
            }
            let cur = tab.focused_index().unwrap_or(0);
            let n = paths.len() as isize;
            let next = (((cur as isize + delta) % n + n) % n) as usize;
            tab.focus_path = paths[next].clone();
            next
        };
        // Make the new cursor immediately visible.
        self.reset_cursor_blink();
        self.update_title();
        let payload = self.pane_focus_payload(self.active_tab, new_index);
        self.events.emit("pane.focus", &payload);
    }

    /// Focus the pane at DFS index `idx` (0-based) in the active tab. Bound
    /// to Alt+1..Alt+9 (mapped 1..9 → idx 0..8) for tmux-style numeric
    /// pane selection. Out-of-range indices are silently ignored so a
    /// keystroke against a tab with fewer panes doesn't surprise the user.
    fn focus_pane_index(&mut self, idx: usize) {
        let Some((prev, path_count)) = self
            .active_tab()
            .map(|t| (t.focused_index().unwrap_or(0), t.pane_count()))
        else {
            return;
        };
        let Some(decision) = focus_index_decision(prev, path_count, idx) else {
            return;
        };
        let new_idx = match decision {
            FocusIndexAction::Apply(n) => n,
            FocusIndexAction::AlreadyThere => {
                self.reset_cursor_blink();
                return;
            }
        };
        {
            let Some(tab) = self.active_tab_mut() else { return };
            let paths = tab.tree.leaf_paths();
            tab.focus_path = paths[new_idx].clone();
        }
        self.reset_cursor_blink();
        self.update_title();
        let payload = self.pane_focus_payload(self.active_tab, new_idx);
        self.events.emit("pane.focus", &payload);
    }

    /// Swap the focused pane with the leaf `delta` positions away in DFS
    /// order (wrapping). Focus follows the swapped pane to its new slot, so
    /// repeated calls cycle the same pane through the layout.
    fn swap_focused_pane(&mut self, delta: isize) {
        let new_focus_path = {
            let Some(tab) = self.active_tab_mut() else { return };
            let paths = tab.tree.leaf_paths();
            if paths.len() < 2 {
                return;
            }
            let cur = tab.focused_index().unwrap_or(0);
            let n = paths.len() as isize;
            let other = (((cur as isize + delta) % n + n) % n) as usize;
            if other == cur {
                return;
            }
            let p_a = paths[cur].clone();
            let p_b = paths[other].clone();
            if !tab.tree.swap_leaves(&p_a, &p_b) {
                return;
            }
            tab.focus_path = p_b.clone();
            p_b
        };
        self.sync_terminal_size();
        self.reset_cursor_blink();
        self.update_title();
        // Compute the new focused index after the swap for the plugin event.
        let new_index = self
            .active_tab()
            .and_then(|t| t.tree.leaf_paths().into_iter().position(|p| p == new_focus_path))
            .unwrap_or(0);
        // Reuse the focus-payload helper for the swap event too — both
        // events use the identical `<tab>:<pane>\t<uid>` shape now, so
        // pane-tracking plugins can match on the uid alone.
        let swap_payload = self.pane_focus_payload(self.active_tab, new_index);
        self.events.emit("pane.swap", &swap_payload);
        self.events.emit("pane.focus", &swap_payload);
    }

    /// Flip the focused pane's `bell_muted` atomic. Wired to the
    /// `toggle_bell_mute` action so users can silence a chatty pane
    /// from the command palette without touching the config file. Fires
    /// a `pane.bell_mute` event with payload `<tab+1>:<pane+1>\t<bool>\t<uid>`
    /// so status-line plugins can render a 🔕 indicator and uid-tracking
    /// plugins can match without re-resolving the index pair.
    fn toggle_focused_pane_bell_mute(&mut self) {
        let Some(tab) = self.active_tab() else { return };
        let Some(pane_idx) = tab.focused_index() else { return };
        let Some(pane) = tab.pane_at(pane_idx) else { return };
        let prev = pane.bell_muted.load(Ordering::Relaxed);
        pane.bell_muted.store(!prev, Ordering::Relaxed);
        let payload = pane_value_uid_payload(
            self.active_tab + 1,
            pane_idx + 1,
            !prev,
            pane.uid,
        );
        self.events.emit("pane.bell_mute", &payload);
    }

    /// Nudge the focused pane's enclosing split by `delta` in the requested
    /// direction. Walks up from the focused leaf looking for the nearest
    /// ancestor Split whose orientation matches `dir`. No-op when the focus
    /// is at the root (only one pane) or no matching ancestor exists.
    fn resize_focused_pane(&mut self, dir: SpatialDir, delta: f32) {
        let Some(tab) = self.active_tab() else { return };
        if tab.zoomed {
            return;
        }
        let want_horizontal = matches!(dir, SpatialDir::Left | SpatialDir::Right);
        let path = tab.focus_path.clone();
        // Walk the path from leaf toward root; for each ancestor split,
        // check if its orientation matches what we need.
        let mut ancestor_len = path.len();
        let (target_path, on_b_side) = loop {
            if ancestor_len == 0 {
                return;
            }
            let parent = &path[..ancestor_len - 1];
            let on_b = path[ancestor_len - 1];
            if let Some((sdir, _)) = tab.tree.split_info(parent) {
                let matches = match sdir {
                    SplitDir::Horizontal => want_horizontal,
                    SplitDir::Vertical => !want_horizontal,
                };
                if matches {
                    break (parent.to_vec(), on_b);
                }
            }
            ancestor_len -= 1;
        };
        let Some((_, ratio)) = tab.tree.split_info(&target_path) else { return };
        // For Right / Down: growing the "left/top" side means moving the
        // boundary further from origin → +delta if focused side is `a`,
        // -delta if focused side is `b`. Left / Up is the inverse.
        let growth = match dir {
            SpatialDir::Right | SpatialDir::Down => delta,
            SpatialDir::Left | SpatialDir::Up => -delta,
        };
        let signed = if on_b_side { -growth } else { growth };
        let new_ratio = (ratio + signed).clamp(0.05, 0.95);
        if let Some(tab) = self.active_tab_mut() {
            tab.tree.set_split_ratio(&target_path, new_ratio);
        }
        self.sync_terminal_size();
    }

    /// Focus the pane in geometric direction `dir` from the focused one.
    /// Picks the neighbour whose perpendicular-axis overlap with the source
    /// pane is largest, breaking ties by smallest along-axis gap. Falls back
    /// to `focus_pane` cycle when no spatial neighbour exists (e.g. zoomed).
    fn focus_pane_spatial(&mut self, dir: SpatialDir) {
        let rects = self.layout_active_tab();
        let Some(tab) = self.active_tab() else { return };
        if tab.zoomed || rects.len() < 2 {
            return;
        }
        let Some(cur) = tab.focused_index() else { return };
        let Some(src) = rects.get(cur).copied() else { return };
        let mut best: Option<(usize, f32, f32)> = None;
        for (i, r) in rects.iter().enumerate() {
            if i == cur || r.width <= 0.5 || r.height <= 0.5 {
                continue;
            }
            let (in_dir, overlap, gap) = match dir {
                SpatialDir::Left => {
                    let beyond = (r.left + r.width) <= src.left + 0.5;
                    let o = (src.top + src.height).min(r.top + r.height)
                        - src.top.max(r.top);
                    let g = src.left - (r.left + r.width);
                    (beyond, o, g)
                }
                SpatialDir::Right => {
                    let beyond = r.left >= (src.left + src.width) - 0.5;
                    let o = (src.top + src.height).min(r.top + r.height)
                        - src.top.max(r.top);
                    let g = r.left - (src.left + src.width);
                    (beyond, o, g)
                }
                SpatialDir::Up => {
                    let beyond = (r.top + r.height) <= src.top + 0.5;
                    let o = (src.left + src.width).min(r.left + r.width)
                        - src.left.max(r.left);
                    let g = src.top - (r.top + r.height);
                    (beyond, o, g)
                }
                SpatialDir::Down => {
                    let beyond = r.top >= (src.top + src.height) - 0.5;
                    let o = (src.left + src.width).min(r.left + r.width)
                        - src.left.max(r.left);
                    let g = r.top - (src.top + src.height);
                    (beyond, o, g)
                }
            };
            if !in_dir || overlap <= 0.0 {
                continue;
            }
            let g = gap.max(0.0);
            // Score: larger overlap is better; smaller gap is better.
            let score = overlap - g * 0.1;
            if best.map(|(_, s, _)| score > s).unwrap_or(true) {
                best = Some((i, score, g));
            }
        }
        let Some((target_idx, _, _)) = best else { return };
        let paths = tab.tree.leaf_paths();
        let Some(target_path) = paths.get(target_idx).cloned() else { return };
        if let Some(tab) = self.active_tab_mut() {
            tab.focus_path = target_path;
        }
        self.reset_cursor_blink();
        self.update_title();
        let payload = self.pane_focus_payload(self.active_tab, target_idx);
        self.events.emit("pane.focus", &payload);
    }

    /// Match the key event against the configured `user_bindings` and run
    /// the first match. Returns `Some(exit)` if a binding fired.
    fn check_user_bindings(&mut self, event: &KeyEvent) -> Option<bool> {
        let mods = self.modifiers;
        // Collect first to avoid borrowing `self.user_bindings` across the
        // dispatch_action call below.
        let action = {
            let mut hit: Option<AppAction> = None;
            for b in &self.user_bindings {
                if b.mods != mods {
                    continue;
                }
                let matches = match &b.key {
                    KeyMatch::Char(c) => match &event.logical_key {
                        Key::Character(s) => {
                            let lower = s.as_str().to_lowercase();
                            // Map shifted-digit symbols (!, @, #, ...) back to
                            // their digits so a binding like `Ctrl+Shift+1`
                            // matches the `!` event under US layouts.
                            let normalised = shifted_digit_to_digit(&lower)
                                .map(|d| d.to_string())
                                .unwrap_or(lower);
                            normalised == *c
                        }
                        _ => false,
                    },
                    KeyMatch::Named(n) => matches!(&event.logical_key, Key::Named(k) if k == n),
                };
                if matches {
                    hit = Some(b.action);
                    break;
                }
            }
            hit
        };
        action.map(|a| self.dispatch_action(a))
    }

    /// App-level key handling. Returns `Some(true)` to request window exit,
    /// `Some(false)` if the key was consumed by App, `None` if it should be
    /// forwarded to the active pane's PTY.
    fn handle_app_shortcut(&mut self, event: &KeyEvent) -> Option<bool> {
        let mods = self.modifiers;
        let ctrl = mods.contains(ModifiersState::CONTROL);
        let shift = mods.contains(ModifiersState::SHIFT);
        let alt = mods.contains(ModifiersState::ALT);

        // Alt+Tab / Alt+Shift+Tab is the OS window switcher. Swallow it
        // so the meta-Tab (ESC + \t) never leaks into the PTY — a shell
        // or TUI reads that as forward/back tab and visibly switches its
        // own tab/pane. rterm takes no action; the compositor still does
        // the actual window switch.
        if is_window_switch_chord(&event.logical_key, alt, ctrl) {
            return Some(false);
        }

        // xterm-style Insert shortcuts: Ctrl+Insert copies, Shift+Insert
        // pastes. Both ignore Alt to avoid shadowing user bindings, and
        // require their sole modifier (Ctrl-only or Shift-only) so e.g.
        // Ctrl+Shift+Insert doesn't accidentally fire either.
        if !alt {
            if let Key::Named(NamedKey::Insert) = &event.logical_key {
                if ctrl && !shift {
                    self.copy_selection();
                    return Some(false);
                }
                if shift && !ctrl {
                    self.paste_clipboard();
                    return Some(false);
                }
            }
        }

        // Alt+Shift+Arrow → resize the focused pane's enclosing split.
        if alt && shift && !ctrl {
            if let Key::Named(named) = &event.logical_key {
                let dir = match named {
                    NamedKey::ArrowLeft => Some(SpatialDir::Left),
                    NamedKey::ArrowRight => Some(SpatialDir::Right),
                    NamedKey::ArrowUp => Some(SpatialDir::Up),
                    NamedKey::ArrowDown => Some(SpatialDir::Down),
                    _ => None,
                };
                if let Some(d) = dir {
                    self.resize_focused_pane(d, 0.05);
                    return Some(false);
                }
            }
        }

        // Alt+Arrow → focus the spatial neighbour in that direction.
        // Alt+1..Alt+9 → focus the Nth pane (tmux-style numeric jump).
        if alt && !ctrl && !shift {
            if let Key::Named(named) = &event.logical_key {
                match named {
                    NamedKey::ArrowRight => {
                        self.focus_pane_spatial(SpatialDir::Right);
                        return Some(false);
                    }
                    NamedKey::ArrowLeft => {
                        self.focus_pane_spatial(SpatialDir::Left);
                        return Some(false);
                    }
                    NamedKey::ArrowUp => {
                        self.focus_pane_spatial(SpatialDir::Up);
                        return Some(false);
                    }
                    NamedKey::ArrowDown => {
                        self.focus_pane_spatial(SpatialDir::Down);
                        return Some(false);
                    }
                    _ => {}
                }
            }
            if let Key::Character(c) = &event.logical_key {
                if let Some(d) = c.chars().next().and_then(|ch| ch.to_digit(10)) {
                    if (1..=9).contains(&d) {
                        self.focus_pane_index(d as usize - 1);
                        return Some(false);
                    }
                }
            }
        }

        // Ctrl+Alt+Up / Ctrl+Alt+Down → jump to previous / next prompt mark.
        if ctrl && alt && !shift {
            if let Key::Named(named) = &event.logical_key {
                match named {
                    NamedKey::ArrowUp => {
                        self.jump_to_prompt(-1);
                        return Some(false);
                    }
                    NamedKey::ArrowDown => {
                        self.jump_to_prompt(1);
                        return Some(false);
                    }
                    _ => {}
                }
            }
        }

        if !(ctrl && shift) {
            return None;
        }
        // Ctrl+Shift+0 resets the font size. Match the PHYSICAL `0` key
        // so it fires on every layout: on German / Spanish / French
        // keyboards Shift+0 produces a character other than `)`, which
        // the logical-key arm below (`"0" | ")"`) would silently miss.
        // The physical `0` key is layout-invariant and unambiguous (it
        // never doubles as a tab selector — those are 1‑9).
        if is_font_reset_key(&event.physical_key) {
            let initial = self.initial_font_size;
            self.set_font_size_absolute(initial);
            return Some(false);
        }
        match &event.logical_key {
            Key::Character(c) => match c.as_str() {
                "T" | "t" => {
                    self.new_tab();
                    return Some(false);
                }
                "W" | "w" => {
                    return Some(self.close_active_tab());
                }
                "D" | "d" => {
                    // tmux convention: Ctrl+Shift+D = "split
                    // horizontal" = horizontal divider = panes stacked.
                    self.split_active_pane(SplitDir::Vertical);
                    return Some(false);
                }
                "E" | "e" => {
                    // Ctrl+Shift+E = "split vertical" = vertical
                    // divider = panes side-by-side.
                    self.split_active_pane(SplitDir::Horizontal);
                    return Some(false);
                }
                "X" | "x" => {
                    return Some(self.close_focused_pane());
                }
                "V" | "v" => {
                    self.paste_clipboard();
                    return Some(false);
                }
                "C" | "c" => {
                    self.copy_selection();
                    return Some(false);
                }
                "Y" | "y" => {
                    self.copy_hovered_url();
                    return Some(false);
                }
                "F" | "f" => {
                    self.start_search();
                    return Some(false);
                }
                "K" | "k" => {
                    self.clear_active_scrollback();
                    return Some(false);
                }
                // Ctrl+Shift+R — Reset focused pane. Resets our own
                // VT parser state AND writes RIS bytes to the PTY so
                // that, when the shell echoes them back, the
                // intervening ConPTY (Windows) sees RIS and clears
                // its OWN state. Without the echo round-trip a
                // random `ESC ( R` from earlier `cat picture.png`
                // output leaves ConPTY's G0 stuck in French NRCS,
                // which makes every subsequent `\`, `@`, `~` etc.
                // render as their French-NRCS substitutes (`ç`, `à`,
                // `¨`, …) and locks the pane until the user opens a
                // new tab.
                "R" | "r" => {
                    return Some(self.dispatch_action(AppAction::ResetPane));
                }
                "H" | "h" => {
                    self.show_help = !self.show_help;
                    if !self.show_help {
                        self.reset_cursor_blink();
                    }
                    return Some(false);
                }
                "P" | "p" => {
                    self.open_palette();
                    return Some(false);
                }
                "Z" | "z" => {
                    self.toggle_zoom();
                    return Some(false);
                }
                "," | "<" => {
                    self.move_active_tab(-1);
                    return Some(false);
                }
                "." | ">" => {
                    self.move_active_tab(1);
                    return Some(false);
                }
                // tmux convention: `{` swaps focused pane with previous,
                // `}` with next. We accept the unshifted square brackets
                // too so layouts that don't shift `[`/`]` to curlies still
                // get the binding.
                "{" | "[" => {
                    self.swap_focused_pane(-1);
                    return Some(false);
                }
                "}" | "]" => {
                    self.swap_focused_pane(1);
                    return Some(false);
                }
                // Ctrl+Shift+1..9 → jump to tab N. US layout sends the
                // shifted symbols when Shift is held, so accept either.
                "1" | "!" => {
                    self.select_tab(0);
                    return Some(false);
                }
                "2" | "@" => {
                    self.select_tab(1);
                    return Some(false);
                }
                "3" | "#" => {
                    self.select_tab(2);
                    return Some(false);
                }
                "4" | "$" => {
                    self.select_tab(3);
                    return Some(false);
                }
                "5" | "%" => {
                    self.select_tab(4);
                    return Some(false);
                }
                "6" | "^" => {
                    self.select_tab(5);
                    return Some(false);
                }
                "7" | "&" => {
                    self.select_tab(6);
                    return Some(false);
                }
                "8" | "*" => {
                    self.select_tab(7);
                    return Some(false);
                }
                "9" | "(" => {
                    self.select_tab(8);
                    return Some(false);
                }
                // Font-size controls. `+` is Shift+`=` on most layouts;
                // accept both shifted forms.
                "=" | "+" => {
                    self.adjust_font_size(1.0);
                    return Some(false);
                }
                "-" | "_" => {
                    self.adjust_font_size(-1.0);
                    return Some(false);
                }
                "0" | ")" => {
                    let initial = self.initial_font_size;
                    self.set_font_size_absolute(initial);
                    return Some(false);
                }
                _ => {}
            },
            Key::Named(NamedKey::ArrowRight) => {
                self.switch_tab(1);
                return Some(false);
            }
            Key::Named(NamedKey::ArrowLeft) => {
                self.switch_tab(-1);
                return Some(false);
            }
            Key::Named(NamedKey::Tab) => {
                self.toggle_last_tab();
                return Some(false);
            }
            _ => {}
        }
        None
    }

    fn pixel_to_cell(&self, pane_idx: usize, x: f64, y: f64) -> Option<SelectionPoint> {
        let rects = self.layout_active_tab();
        let rect = rects.get(pane_idx)?;
        let state = self.state.as_ref()?;
        let cell_w = state.text.cell_width() as f64;
        let line_h = state.text.line_height() as f64;
        let cx = ((x - rect.left as f64) / cell_w).max(0.0) as u16;
        let cy = ((y - rect.top as f64) / line_h).max(0.0) as u16;
        let tab = self.active_tab()?;
        let pane = tab.pane_at(pane_idx)?;
        let term = pane.terminal.lock().ok()?;
        let size = term.size();
        let mut col = cx.min(size.cols.saturating_sub(1));
        let row = cy.min(size.rows.saturating_sub(1));
        // If the hit landed on a WIDE_SPACER (right half of a CJK / emoji
        // glyph), snap back to the wide cell's left half so selection /
        // word-pick treat the glyph as one unit instead of acting on the
        // ' ' that physically occupies the spacer.
        let offset = pane.scroll_offset.load(Ordering::Relaxed);
        if let Some(row_cells) = term.visible_row(offset, row) {
            if let Some(cell) = row_cells.get(col as usize) {
                if cell.attrs.contains(CellAttrs::WIDE_SPACER) && col > 0 {
                    col -= 1;
                }
            }
        }
        Some(SelectionPoint { row, col })
    }

    /// Find the Split path whose gap is currently under the cursor (within
    /// ±GAP_HIT pixels), or None.
    fn gap_at(&self, x: f64, y: f64) -> Option<(tree::TreePath, SplitDir)> {
        const GAP_HIT: f64 = 5.0;
        let outer = self.outer_rect()?;
        let tab = self.active_tab()?;
        if tab.zoomed {
            return None;
        }
        for frame in tab.tree.splits(outer, SPLIT_GAP) {
            match frame.dir {
                SplitDir::Horizontal => {
                    let center =
                        (frame.a_rect.left + frame.a_rect.width) as f64 + SPLIT_GAP as f64 * 0.5;
                    let top = frame.a_rect.top as f64;
                    let bottom = top + frame.a_rect.height as f64;
                    if y >= top && y <= bottom && (x - center).abs() <= GAP_HIT {
                        return Some((frame.path, frame.dir));
                    }
                }
                SplitDir::Vertical => {
                    let center =
                        (frame.a_rect.top + frame.a_rect.height) as f64 + SPLIT_GAP as f64 * 0.5;
                    let left = frame.a_rect.left as f64;
                    let right = left + frame.a_rect.width as f64;
                    if x >= left && x <= right && (y - center).abs() <= GAP_HIT {
                        return Some((frame.path, frame.dir));
                    }
                }
            }
        }
        None
    }

    /// Drag-resize a Split: compute new ratio from the cursor position
    /// within the Split's containing rect, set, and resize the affected
    /// panes' PTYs.
    fn resize_gap(&mut self, path: &tree::TreePath, x: f64, y: f64) {
        let outer = match self.outer_rect() {
            Some(o) => o,
            None => return,
        };
        let containing = match self
            .active_tab()
            .and_then(|t| t.tree.rect_at(outer, SPLIT_GAP, path))
        {
            Some(r) => r,
            None => return,
        };
        let dir = match self
            .active_tab()
            .and_then(|t| t.tree.splits(outer, SPLIT_GAP).into_iter().find(|f| f.path == *path))
        {
            Some(f) => f.dir,
            None => return,
        };
        // `f32::clamp` panics when min > max. Deep nested splits can
        // shrink `usable` below 40 px (split_rect floors children at
        // 1 px), so the upper bound must never drop under the lower
        // one — otherwise a divider drag inside a tiny rect crashes
        // the whole window.
        let clamp_a = |v: f32, usable: f32| v.clamp(20.0, (usable - 20.0).max(20.0));
        let new_ratio = match dir {
            SplitDir::Horizontal => {
                let usable = (containing.width - SPLIT_GAP).max(1.0);
                let new_a =
                    clamp_a((x - containing.left as f64) as f32 - SPLIT_GAP * 0.5, usable);
                (new_a / usable).clamp(0.05, 0.95)
            }
            SplitDir::Vertical => {
                let usable = (containing.height - SPLIT_GAP).max(1.0);
                let new_a =
                    clamp_a((y - containing.top as f64) as f32 - SPLIT_GAP * 0.5, usable);
                (new_a / usable).clamp(0.05, 0.95)
            }
        };
        if let Some(tab) = self.active_tab_mut() {
            tab.tree.set_split_ratio(path, new_ratio);
        }
        self.sync_terminal_size();
    }

    fn pane_at(&self, x: f64, y: f64) -> Option<usize> {
        let rects = self.layout_active_tab();
        for (i, rect) in rects.iter().enumerate() {
            if x >= rect.left as f64
                && x < (rect.left + rect.width) as f64
                && y >= rect.top as f64
                && y < (rect.top + rect.height) as f64
            {
                return Some(i);
            }
        }
        None
    }

    /// Returns `true` if the press caused the window to want exit
    /// (e.g. clicking × on the very last tab). The caller is expected
    /// to forward that to `event_loop.exit()`.
    #[must_use]
    fn handle_press(&mut self, x: f64, y: f64) -> bool {
        // Open overlays swallow the click — same modality as Esc.
        // Closes the overlay and returns; if the user actually wanted
        // to interact with the underlying content, they click again.
        if self.context_menu.is_some() {
            return self.handle_context_menu_press(x, y);
        }
        if self.show_settings {
            return self.handle_settings_press(x, y);
        }
        if self.show_help {
            self.show_help = false;
            self.reset_cursor_blink();
            return false;
        }
        if self.palette.is_some() {
            self.close_palette();
            return false;
        }
        // Paste-confirmation modal is up. Clicks INSIDE the modal
        // card go to its own handler (buttons / cursor placement).
        // Clicks OUTSIDE the card pass through to the window
        // chrome only — resize edges, min/max/close, title-bar
        // drag — so the user can still move / resize / close the
        // window while the modal is open. All other outside clicks
        // (tab strip, hamburger, pane area) are absorbed so a
        // stray click can't quietly shift focus underneath.
        if self.paste_confirmation.is_some() {
            let modal_rect = self
                .paste_confirmation
                .as_ref()
                .and_then(|m| self.paste_confirmation_rect(m));
            let inside_modal = modal_rect.is_some_and(|r| {
                let xf = x as f32;
                let yf = y as f32;
                xf >= r.left
                    && xf < r.left + r.width
                    && yf >= r.top
                    && yf < r.top + r.height
            });
            if inside_modal {
                self.handle_paste_confirmation_press(x, y);
                return false;
            }
            if !self.os_decorations {
                if let Some(dir) = self.window_edge_at(x, y) {
                    if let Some(state) = self.state.as_ref() {
                        let _ = state.window.drag_resize_window(dir);
                    }
                    return false;
                }
            }
            if let Some(which) = self.window_control_at(x, y) {
                return self.click_window_control(which);
            }
            if let Some(rect) = self.header_rect() {
                let yf = y as f32;
                if yf >= rect.top && yf < rect.top + rect.height {
                    let on_chrome = self.tab_at(x, y).is_some()
                        || self.hamburger_at(x, y)
                        || self.new_tab_button_at(x, y);
                    if !on_chrome && !self.os_decorations {
                        if let Some(state) = self.state.as_ref() {
                            let _ = state.window.drag_window();
                        }
                    }
                }
            }
            return false;
        }
        // Suggestion popup: a click inside the popup picks the row
        // under the cursor + injects it (same effect as TAB after
        // ↓ nav). A click OUTSIDE the popup closes it but lets the
        // event continue to the normal pane-click handlers, so the
        // user can e.g. click a different pane to shift focus
        // while a popup was visible.
        if self.suggestion_popup.is_some() {
            if self.handle_suggestion_popup_press(x, y) {
                return false;
            }
            self.suggestion_popup = None;
        }
        // Window-edge resize zone — only meaningful when we own the
        // decorations. The OS handles edge resize when its title bar
        // is visible.
        if !self.os_decorations {
            if let Some(dir) = self.window_edge_at(x, y) {
                if let Some(state) = self.state.as_ref() {
                    let _ = state.window.drag_resize_window(dir);
                }
                return false;
            }
        }
        // `≡` hamburger button — opens the app menu anchored below it.
        if self.hamburger_at(x, y) {
            self.toggle_app_menu();
            return false;
        }
        // `+` new-tab button right after the last tab chip.
        if self.new_tab_button_at(x, y) {
            self.new_tab();
            return false;
        }
        // Window control buttons (minimize / maximize / close) at the
        // right end of the header bar. Close exits the loop the same
        // way `quit` / last-tab-× does.
        if let Some(which) = self.window_control_at(x, y) {
            return self.click_window_control(which);
        }
        // Tab-bar click → switch to that tab, OR close it when the
        // press landed on the trailing "× " close marker. Beats
        // gap-drag and pane hit-testing because the tab bar lives above
        // every pane (and the gap detector only matches over the pane
        // area anyway).
        if let Some((t, hit)) = self.tab_hit_at(x, y) {
            if hit == TabHit::Close {
                self.last_header_empty_click = None;
                self.last_tab_click = None;
                let exit = self.close_tab_at(t);
                if !exit {
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                }
                return exit;
            }
            // Landing on a tab cancels any "double-click empty area"
            // intent — otherwise a tap-on-tab-then-tap-on-empty would
            // wrongly trigger new-tab on the second tap.
            self.last_header_empty_click = None;
            // Detect a double-click on the same tab → open rename
            // overlay (Chrome/Firefox convention).
            let now = Instant::now();
            let double_click = matches!(
                self.last_tab_click,
                Some((prev_t, prev_idx))
                    if prev_idx == t && now.duration_since(prev_t) <= MULTI_CLICK_INTERVAL
            );
            self.last_tab_click = Some((now, t));
            if double_click {
                self.start_tab_rename(t);
                return false;
            }
            // Record drag source + press offset so the ghost chip in
            // `tab_bar_quads` sits under the cursor at the same pixel
            // offset where the user grabbed it (no jump-to-cursor on
            // press). `tab_layout` provides each tab's pixel left
            // edge; we subtract that from `x` to get the within-chip
            // offset.
            self.tab_dragging = Some(t);
            self.tab_drag_press_offset = self
                .tab_layout()
                .and_then(|l| l.entries.iter().find(|e| e.idx == t).map(|e| x - e.left))
                .unwrap_or(0.0);
            // Fire a `tab.drag_start` edge event so plugins can
            // suspend tooltips / hover-tracking while a drag is
            // in flight. Payload: 1-based source tab index.
            self.events.emit("tab.drag_start", &(t + 1).to_string());
            if t != self.active_tab {
                self.select_tab(t);
            } else if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
            return false;
        }
        // Click in the header bar but NOT on a tab.
        //   - Single click: initiates a client-side window drag when
        //     we own decorations (Chrome/Firefox feel — grab the tab
        //     strip to move the window).
        //   - Double click: toggle maximize (Chrome/Firefox/macOS
        //     convention). This replaces the old "double-click empty
        //     header opens new tab" gesture; new-tab is on the
        //     hamburger / Ctrl+Shift+T / palette anyway.
        if let Some(rect) = self.header_rect() {
            let yf = y as f32;
            if yf >= rect.top && yf < rect.top + rect.height {
                let now = Instant::now();
                if let Some(prev) = self.last_header_empty_click {
                    if now.duration_since(prev) <= MULTI_CLICK_INTERVAL {
                        self.last_header_empty_click = None;
                        if let Some(state) = self.state.as_ref() {
                            let is_max = state.window.is_maximized();
                            state.window.set_maximized(!is_max);
                        }
                        return false;
                    }
                }
                self.last_header_empty_click = Some(now);
                if !self.os_decorations {
                    if let Some(state) = self.state.as_ref() {
                        let _ = state.window.drag_window();
                    }
                }
                return false;
            }
        }
        // Out of the header bar → reset the empty-click anchor so
        // double-click word selection in panes stays unaffected.
        self.last_header_empty_click = None;
        // Resize gap drag takes priority over all other click handling.
        if let Some((path, _dir)) = self.gap_at(x, y) {
            self.gap_dragging = Some(path);
            return false;
        }
        let Some(i) = self.pane_at(x, y) else {
            self.selection = None;
            self.last_click = None;
            return false;
        };

        // Shift+Click extends the current selection (xterm/iTerm behaviour).
        if self.modifiers.contains(ModifiersState::SHIFT) {
            let same_pane = self
                .selection
                .as_ref()
                .map(|s| s.pane_idx == i)
                .unwrap_or(false);
            if same_pane {
                if let Some(p) = self.pixel_to_cell(i, x, y) {
                    if let Some(ap) = self.abs_point(i, p) {
                        if let Some(sel) = self.selection.as_mut() {
                            sel.focus = ap;
                        }
                        self.mouse_dragging = true;
                        return false;
                    }
                }
            }
        }
        let focus_changed = {
            let Some(tab) = self.active_tab_mut() else { return false };
            if i >= tab.pane_count() {
                return false;
            }
            let new_path = match tab.tree.leaf_paths().get(i).cloned() {
                Some(p) => p,
                None => return false,
            };
            if tab.focus_path != new_path {
                tab.focus_path = new_path;
                true
            } else {
                false
            }
        };
        if focus_changed {
            self.update_title();
            let payload = self.pane_focus_payload(self.active_tab, i);
            self.events.emit("pane.focus", &payload);
        }
        let Some(p) = self.pixel_to_cell(i, x, y) else { return false };

        // Ctrl+click (Linux/Windows) or Cmd+click (macOS) on an OSC 8
        // hyperlink or auto-detected URL → open in the default browser.
        let modclick = self.modifiers.contains(ModifiersState::CONTROL)
            || self.modifiers.contains(ModifiersState::SUPER);
        if modclick {
            if let Some(pane) = self.active_tab().and_then(|t| t.pane_at(i)) {
                let offset = pane.scroll_offset.load(Ordering::Relaxed);
                let url = pane.terminal.lock().ok().and_then(|t| {
                    t.hyperlink_at(offset, p.row, p.col)
                        .map(|s| s.to_string())
                        .or_else(|| t.detect_url_at(offset, p.row, p.col))
                });
                if let Some(url) = url {
                    // Scheme whitelist: OSC 8 lets a shell embed an
                    // arbitrary URI under any visible text, including
                    // `javascript:`, `data:`, `file:///...`. Run every
                    // URL — auto-detected OR OSC 8 — through the same
                    // safety filter before invoking xdg-open / start /
                    // open. Detection already filtered; the OSC 8 path
                    // previously did not.
                    if !rterm_core::is_safe_url(&url) {
                        tracing::warn!(url = %url, "blocked URL with disallowed scheme");
                        self.events.emit("link.blocked", &url);
                        return false;
                    }
                    // `that_detached` so a slow-launching browser doesn't
                    // block the UI thread (matches the plugin-driven
                    // `rterm.open_url` path).
                    if let Err(e) = open::that_detached(&url) {
                        tracing::warn!("open URL failed: {e}");
                    } else {
                        self.events.emit("link.open", &url);
                    }
                    return false;
                }
            }
        }

        // If the pane's shell asked for mouse reporting, forward the click
        // instead of starting a local selection.
        if let Some(pane) = self.active_tab().and_then(|t| t.pane_at(i)) {
            if let Some((_mode, sgr)) = mouse_mode_for(pane) {
                let bytes = encode_mouse(sgr, 0, p.col, p.row, true);
                pane.send_input(&bytes);
                self.mouse_pty_pane = Some(i);
                self.selection = None;
                return false;
            }
        }

        // Determine click count for multi-click word/line selection.
        let now = Instant::now();
        let count = match self.last_click {
            Some((t, last_p, n))
                if now.duration_since(t) <= MULTI_CLICK_INTERVAL
                    && last_p.row == p.row
                    && last_p.col == p.col =>
            {
                if n >= 3 { 1 } else { n + 1 }
            }
            _ => 1,
        };
        self.last_click = Some((now, p, count));

        match count {
            1 => {
                // `Alt+drag` triggers rectangular (block) selection —
                // matches Terminator / iTerm2 / kitty. `Ctrl` is
                // reserved for "open URL under cursor" (single click)
                // so it can't double as the block modifier. The mode
                // sticks until mouse-up; if the user is still holding
                // Alt when they later double-click, the second click
                // dispatches through the `count == 2` arm above and
                // we drop back into Word mode — same as Terminator.
                let mode = if self.modifiers.contains(ModifiersState::ALT) {
                    SelectionMode::Block
                } else {
                    SelectionMode::Char
                };
                let Some(ap) = self.abs_point(i, p) else { return false };
                self.selection = Some(ActiveSelection {
                    pane_idx: i,
                    anchor: ap,
                    focus: ap,
                    mode,
                    pivot: None,
                });
                self.mouse_dragging = true;
            }
            2 => {
                if let Some(sel) = self.word_selection_at(i, p) {
                    let end_inclusive = SelectionPoint {
                        row: sel.end.row,
                        col: sel.end.col.saturating_sub(1),
                    };
                    let Some(start_abs) = self.abs_point(i, sel.start) else { return false };
                    let Some(end_abs) = self.abs_point(i, end_inclusive) else { return false };
                    self.selection = Some(ActiveSelection {
                        pane_idx: i,
                        anchor: start_abs,
                        focus: end_abs,
                        mode: SelectionMode::Word,
                        pivot: Some((start_abs, end_abs)),
                    });
                    // Keep dragging armed so the user can extend the
                    // initial word selection by sweeping the mouse — the
                    // drag handler snaps to word boundaries against the
                    // stored pivot from here.
                    self.mouse_dragging = true;
                }
            }
            _ => {
                if let Some(sel) = self.line_selection_at(i, p) {
                    let end_inclusive = SelectionPoint {
                        row: sel.end.row,
                        col: sel.end.col.saturating_sub(1),
                    };
                    let Some(start_abs) = self.abs_point(i, sel.start) else { return false };
                    let Some(end_abs) = self.abs_point(i, end_inclusive) else { return false };
                    self.selection = Some(ActiveSelection {
                        pane_idx: i,
                        anchor: start_abs,
                        focus: end_abs,
                        mode: SelectionMode::Line,
                        pivot: Some((start_abs, end_abs)),
                    });
                    self.mouse_dragging = true;
                }
            }
        }
        false
    }

    /// Build an `AbsPoint` from a viewport-relative `SelectionPoint`
    /// by sampling the pane's current `scrollback_len` + `scroll_offset`.
    /// Returns `None` only on truly degenerate state (no active tab,
    /// no pane at `pane_idx`, poisoned terminal mutex). The alt-
    /// screen path forces `sb_len = 0` since the alt grid has no
    /// scrollback — that way selections made in vim / less stay
    /// anchored to the alt grid rather than drifting into negative
    /// abs-row space when alt exits.
    fn abs_point(&self, pane_idx: usize, sp: SelectionPoint) -> Option<AbsPoint> {
        let tab = self.active_tab()?;
        let pane = tab.pane_at(pane_idx)?;
        let term = pane.terminal.lock().ok()?;
        let sb_len = if term.is_on_alt_screen() {
            0
        } else {
            term.scrollback_len()
        };
        drop(term);
        let offset = pane.scroll_offset.load(Ordering::Relaxed);
        Some(AbsPoint::from_viewport(sp, sb_len, offset))
    }

    fn word_selection_at(&self, pane_idx: usize, p: SelectionPoint) -> Option<NormSelection> {
        let tab = self.active_tab()?;
        let pane = tab.pane_at(pane_idx)?;
        let term = pane.terminal.lock().ok()?;
        let offset = pane.scroll_offset.load(Ordering::Relaxed);
        let row_cells = term.visible_row(offset, p.row)?;
        let n = row_cells.len();
        if n == 0 || (p.col as usize) >= n {
            return None;
        }
        // Word characters include URL / path symbols so double-click on
        // `https://x.com/y?a=1&b=2#z` selects the whole URL. Brackets,
        // quotes, and whitespace remain delimiters.
        let is_word = |ch: char| -> bool {
            ch.is_alphanumeric()
                || matches!(
                    ch,
                    '_' | '-'
                        | '.'
                        | '/'
                        | '~'
                        | '+'
                        | '='
                        | '@'
                        | ':'
                        | '?'
                        | '&'
                        | '#'
                        | '%'
                        | ','
                )
        };
        let hit = row_cells[p.col as usize].ch;
        if !is_word(hit) {
            // Just select the single non-word character.
            return Some(NormSelection {
                start: SelectionPoint { row: p.row, col: p.col },
                end: SelectionPoint { row: p.row, col: p.col + 1 },
                block: false,
            });
        }
        let mut left = p.col;
        while left > 0 && is_word(row_cells[left as usize - 1].ch) {
            left -= 1;
        }
        let mut right = (p.col + 1) as usize;
        while right < n && is_word(row_cells[right].ch) {
            right += 1;
        }
        Some(NormSelection {
            start: SelectionPoint { row: p.row, col: left },
            end: SelectionPoint { row: p.row, col: right as u16 },
            block: false,
        })
    }

    fn line_selection_at(&self, pane_idx: usize, p: SelectionPoint) -> Option<NormSelection> {
        let tab = self.active_tab()?;
        let pane = tab.pane_at(pane_idx)?;
        let term = pane.terminal.lock().ok()?;
        let offset = pane.scroll_offset.load(Ordering::Relaxed);
        let row_cells = term.visible_row(offset, p.row)?;
        let last_nonblank = row_cells
            .iter()
            .rposition(|c| c.ch != ' ')
            .map(|i| (i + 1) as u16)
            .unwrap_or(0);
        Some(NormSelection {
            start: SelectionPoint { row: p.row, col: 0 },
            end: SelectionPoint { row: p.row, col: last_nonblank.max(1) },
            block: false,
        })
    }

    fn handle_drag(&mut self, x: f64, y: f64) {
        // Tab drag-reorder: live-shuffle the source tab through the
        // tab strip as the cursor crosses chip boundaries. Each swap
        // also seeds a `tab_swap_anim` so the displaced sibling
        // visibly slides into its new slot rather than jumping.
        if let Some(src) = self.tab_dragging {
            if let Some(dst) = self.tab_at(x, y) {
                if dst < self.tabs.len() && src < self.tabs.len() && dst != src {
                    let delta: isize = if dst > src { 1 } else { -1 };
                    // Measure chip widths BEFORE the swap so the
                    // animation knows how far each tab has to slide.
                    // For a single-step swap, both chips move by the
                    // average width — close enough on monospace and
                    // visually indistinguishable from per-chip widths.
                    let chip_w_px = self
                        .tab_layout()
                        .and_then(|l| {
                            l.entries
                                .iter()
                                .find(|e| e.idx == src)
                                .map(|e| (e.close_end - e.left) as f32)
                        })
                        .unwrap_or(0.0);
                    self.move_active_tab(delta);
                    let new_src = (src as isize + delta) as usize;
                    self.tab_dragging = Some(new_src);
                    self.tab_swap_anim = Some(TabSwapAnim {
                        moved_idx: new_src,
                        displaced_idx: src,
                        delta_px: chip_w_px * delta as f32,
                        started_at: Instant::now(),
                    });
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                }
            }
            return;
        }
        // Resize-gap drag has top priority while active.
        if let Some(path) = self.gap_dragging.clone() {
            self.resize_gap(&path, x, y);
            return;
        }
        // Forward motion to PTY when the focused pane has button-event /
        // any-event tracking on.
        if let Some(i) = self.mouse_pty_pane {
            if let Some(pane) = self.active_tab().and_then(|t| t.pane_at(i)) {
                if let Some((mode, sgr)) = mouse_mode_for(pane) {
                    if matches!(mode, MouseTracking::ButtonEvent | MouseTracking::AnyEvent) {
                        if let Some(p) = self.pixel_to_cell(i, x, y) {
                            // Button 0 (left) with +32 motion bit.
                            let bytes = encode_mouse(sgr, 32, p.col, p.row, true);
                            pane.send_input(&bytes);
                        }
                    }
                }
            }
            return;
        }
        if !self.mouse_dragging {
            return;
        }
        let pane_idx = match self.selection {
            Some(s) => s.pane_idx,
            None => return,
        };
        // Detect drag past pane edges: cue auto-scroll. RedrawRequested ticks
        // the actual scroll per frame; here we just record the direction.
        let rect = self.layout_active_tab().get(pane_idx).copied();
        if let Some(rect) = rect {
            self.drag_scroll_dir = if y < rect.top as f64 {
                -1
            } else if y > (rect.top + rect.height) as f64 {
                1
            } else {
                0
            };
        }
        if self.drag_scroll_dir == 0 {
            if let Some(p) = self.pixel_to_cell(pane_idx, x, y) {
                self.update_drag_focus(pane_idx, p);
            }
        }
    }

    /// Update the selection focus during a drag. For Char mode this is
    /// just `focus = p`. For Word/Line modes, snap the focus (and the
    /// anchor, when the drag crosses the pivot) so the selection grows
    /// in whole-word or whole-line increments and the original pivot
    /// word/line is always covered.
    fn update_drag_focus(&mut self, pane_idx: usize, p: SelectionPoint) {
        let (mode, pivot) = match self.selection {
            Some(sel) => (sel.mode, sel.pivot),
            None => return,
        };
        // Helper: lift a viewport `SelectionPoint` into the
        // absolute-row frame of the active pane. Fails-soft to
        // `(0, p.col)` if the terminal mutex is poisoned mid-drag.
        let lift = |sp: SelectionPoint| -> AbsPoint {
            self.abs_point(pane_idx, sp).unwrap_or(AbsPoint {
                abs_row: sp.row as i64,
                col: sp.col,
            })
        };
        let p_abs = lift(p);
        let snapped = match mode {
            // Char and Block both move the focus directly to the
            // cursor position — Block doesn't snap to anything (the
            // rect math happens later in `to_visible_norm`), so it
            // shares the no-snap branch with Char.
            SelectionMode::Char | SelectionMode::Block => None,
            SelectionMode::Word => pivot.map(|piv| {
                let (drag_start, drag_end_incl) = self
                    .word_selection_at(pane_idx, p)
                    .map(|w| (lift(w.start), lift(SelectionPoint {
                        row: w.end.row,
                        col: w.end.col.saturating_sub(1),
                    })))
                    .unwrap_or((p_abs, p_abs));
                snap_drag_to_range(piv, drag_start, drag_end_incl, false)
            }),
            SelectionMode::Line => pivot.map(|piv| {
                let (drag_start, drag_end_incl) = self
                    .line_selection_at(pane_idx, p)
                    .map(|l| (lift(l.start), lift(SelectionPoint {
                        row: l.end.row,
                        col: l.end.col.saturating_sub(1),
                    })))
                    .unwrap_or((
                        lift(SelectionPoint { row: p.row, col: 0 }),
                        lift(SelectionPoint { row: p.row, col: 0 }),
                    ));
                snap_drag_to_range(piv, drag_start, drag_end_incl, true)
            }),
        };
        if let Some(sel) = self.selection.as_mut() {
            match snapped {
                Some((anchor, focus)) => {
                    sel.anchor = anchor;
                    sel.focus = focus;
                }
                None => sel.focus = p_abs,
            }
        }
    }

    fn handle_release(&mut self) {
        self.drag_scroll_dir = 0;
        if self.gap_dragging.take().is_some() {
            return;
        }
        // Tab drag-reorder finalize. The live-drag handler in
        // `handle_drag` already kept the tab in lock-step with the
        // cursor by calling `move_active_tab(±1)` per chip boundary,
        // so on release we just clear the state and emit the edge
        // event. `moved` reports whether the tab actually shifted
        // from its press-time position.
        if let Some(src) = self.tab_dragging.take() {
            let (x, y) = (self.cursor_pos.x, self.cursor_pos.y);
            let moved = src != self.active_tab;
            self.update_cursor_icon(x, y);
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
            // `src` here is the LATEST position recorded by the live
            // drag, so it equals `active_tab`. The 1-based emit
            // payload reports the final landing position; plugins
            // that need the original source should subscribe to
            // `tab.drag_start`.
            self.events.emit(
                "tab.drag_end",
                &tab_drag_end_payload(self.active_tab + 1, moved),
            );
            return;
        }
        if let Some(i) = self.mouse_pty_pane.take() {
            if let Some(pane) = self.active_tab().and_then(|t| t.pane_at(i)) {
                if let Some((_mode, sgr)) = mouse_mode_for(pane) {
                    if let Some(p) = self.pixel_to_cell(i, self.cursor_pos.x, self.cursor_pos.y) {
                        let bytes = encode_mouse(sgr, 0, p.col, p.row, false);
                        pane.send_input(&bytes);
                    }
                }
            }
            return;
        }
        self.mouse_dragging = false;
        self.paste_modal_dragging = false;
        if let Some(sel) = self.selection {
            if sel.is_empty() {
                self.selection = None;
            } else if let Some(text) = self.selection_text() {
                // The default no longer auto-copies (users disliked PRIMARY
                // being clobbered on every mouse drag — see
                // feedback_rterm_clipboard memory). Plugins that want the
                // X11-style auto-copy can opt in by handling this event:
                //   `rterm.on("selection.end", function(text) rterm.copy(text) end)`
                self.events.emit("selection.end", &text);
            }
        }
    }

    /// Build the plain text representation of the current selection. Returns
    /// `None` when there is no selection or the text is empty after trimming
    /// trailing-space padding on each line.
    ///
    /// Iterates by ABSOLUTE row index so copy continues to capture
    /// the right content even when the selection has been scrolled
    /// off-screen (or only partially visible). Each abs-row maps
    /// back to a (scrollback or grid) physical row via the same
    /// math `Terminal::visible_row` uses internally.
    fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let tab = self.active_tab()?;
        let pane = tab.pane_at(sel.pane_idx)?;
        let mut text = String::new();
        if let Ok(term) = pane.terminal.lock() {
            let sb_len = if term.is_on_alt_screen() {
                0
            } else {
                term.scrollback_len()
            };
            let grid_rows = term.size().rows;
            // Normalise the absolute-coord anchor / focus into a
            // [start, end] pair using the same lexicographic /
            // block-rectangle rules as `to_visible_norm`.
            let (abs_s_row, s_col, abs_e_row, e_col, block) =
                if matches!(sel.mode, SelectionMode::Block) {
                    let (rs, re) = if sel.anchor.abs_row <= sel.focus.abs_row {
                        (sel.anchor.abs_row, sel.focus.abs_row)
                    } else {
                        (sel.focus.abs_row, sel.anchor.abs_row)
                    };
                    let (cs, ce) = if sel.anchor.col <= sel.focus.col {
                        (sel.anchor.col, sel.focus.col + 1)
                    } else {
                        (sel.focus.col, sel.anchor.col + 1)
                    };
                    (rs, cs, re, ce, true)
                } else {
                    let a = (sel.anchor.abs_row, sel.anchor.col);
                    let f = (sel.focus.abs_row, sel.focus.col);
                    let (s, e) = if a <= f { (a, f) } else { (f, a) };
                    // `e.1` is the inclusive max column; the copy loop
                    // below uses `e_col` as an EXCLUSIVE upper bound
                    // (`row[lo..hi]`), so bump by one — the same +1 the
                    // block arm applies — or the last selected glyph is
                    // dropped from the copied text.
                    (s.0, s.1, e.0, e.1.saturating_add(1), false)
                };
            let total = sb_len as i64 + grid_rows as i64;
            for abs_r in abs_s_row..=abs_e_row {
                if abs_r < 0 || abs_r >= total {
                    continue;
                }
                // Re-use `visible_row` by deriving an offset that
                // places `abs_r` at viewport row 0 (when it's in
                // scrollback) or use offset=0 + the right grid row
                // (when it's in the live grid). Same projection as
                // `Terminal::visible_row` does internally — fewer
                // surprises than reaching into private state.
                let row_opt = if (abs_r as usize) < sb_len {
                    let off = (sb_len - abs_r as usize).min(u16::MAX as usize) as u16;
                    term.visible_row(off, 0)
                } else {
                    let g_row = (abs_r as usize - sb_len) as u16;
                    term.visible_row(0, g_row)
                };
                let Some(row) = row_opt else { continue };
                let (lo, hi) = if block {
                    (
                        (s_col as usize).min(row.len()),
                        (e_col as usize).min(row.len()),
                    )
                } else {
                    let lo = if abs_r == abs_s_row { s_col as usize } else { 0 };
                    let hi = if abs_r == abs_e_row {
                        (e_col as usize).min(row.len())
                    } else {
                        row.len()
                    };
                    (lo, hi)
                };
                if lo < hi {
                    for cell in &row[lo..hi] {
                        // Skip the right-half spacer of a wide glyph
                        // (CJK / emoji). Its `ch` is a literal space, so
                        // without this filter copied text gets `<wide>` +
                        // ` ` per double-width character.
                        if cell.attrs.contains(CellAttrs::WIDE_SPACER) {
                            continue;
                        }
                        text.push(cell.ch);
                    }
                }
                if abs_r < abs_e_row {
                    // Skip the newline when this row soft-wrapped into the
                    // next one (autowrap filled it to the margin — there's
                    // no real `\n` there), so a wrapped line copies as one
                    // logical line. Block selection keeps rows independent,
                    // so it always joins with `\n`.
                    let soft_wrapped = !block && {
                        if (abs_r as usize) < sb_len {
                            let off =
                                (sb_len - abs_r as usize).min(u16::MAX as usize) as u16;
                            term.row_wrapped(off, 0)
                        } else {
                            let g_row = (abs_r as usize - sb_len) as u16;
                            term.row_wrapped(0, g_row)
                        }
                    };
                    if !soft_wrapped {
                        text.push('\n');
                    }
                }
            }
        }
        let cleaned: String = text
            .lines()
            .map(|l| l.trim_end_matches(' '))
            .collect::<Vec<_>>()
            .join("\n");
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
    }

    fn copy_selection(&self) {
        let Some(text) = self.selection_text() else { return };
        clipboard_set(&text);
        self.events.emit("copy", &text);
    }

    /// Copy the URL currently under the mouse (auto-detected URL or OSC 8
    /// hyperlink) to the system clipboard. No-op when nothing is hovered.
    fn copy_hovered_url(&self) {
        let Some(url) = self.hover_url.clone() else { return };
        clipboard_set(&url);
        self.events.emit("copy", &url);
    }

    /// Open the URL currently under the mouse via the OS default
    /// handler. Pairs with the existing `Ctrl+click` interaction so
    /// keyboard-driven users can bind a shortcut and avoid reaching
    /// for the mouse just to follow a link.
    fn open_hovered_url(&self) {
        let Some(url) = self.hover_url.clone() else { return };
        if !rterm_core::is_safe_url(&url) {
            tracing::warn!(url = %url, "blocked URL with disallowed scheme");
            self.events.emit("link.blocked", &url);
            return;
        }
        if let Err(e) = open::that_detached(&url) {
            tracing::warn!("open URL failed: {e}");
        } else {
            self.events.emit("link.open", &url);
        }
    }

    /// Handle Shift+PageUp/PageDown/Home/End for keyboard scrollback nav.
    /// Returns `true` if the event was consumed.
    fn handle_scroll_key(&self, event: &KeyEvent) -> bool {
        if !self.modifiers.contains(ModifiersState::SHIFT) {
            return false;
        }
        if self.modifiers.contains(ModifiersState::CONTROL)
            || self.modifiers.contains(ModifiersState::ALT)
        {
            return false;
        }
        let Key::Named(named) = &event.logical_key else { return false };
        let kind = match named {
            NamedKey::PageUp => ScrollNav::PageUp,
            NamedKey::PageDown => ScrollNav::PageDown,
            NamedKey::Home => ScrollNav::Home,
            NamedKey::End => ScrollNav::End,
            _ => return false,
        };
        self.scroll_view(kind);
        true
    }

    /// Wipe the focused pane's scrollback ring (live grid is preserved).
    /// Also snaps the view back to live since the scrollback offset would
    /// otherwise hang over nothing.
    fn clear_active_scrollback(&self) {
        let Some(pane) = self.focused_pane() else { return };
        if let Ok(mut t) = pane.terminal.lock() {
            t.clear_scrollback();
        }
        pane.scroll_offset.store(0, Ordering::Relaxed);
        // Notify plugins — useful for a "history watcher" that wants to
        // know when the user wiped their session. Pairs with the
        // `scrollback.save` event for symmetry. Payload identifies the
        // focused pane via 1-based `<tab>:<pane>` so plugins managing
        // multiple panes can attribute the clear to the right one.
        let pane_idx = self
            .active_tab()
            .and_then(|t| t.focused_index())
            .map(|i| i + 1)
            .unwrap_or(0);
        let payload = format!("{}:{}", self.active_tab + 1, pane_idx);
        self.events.emit("scrollback.clear", &payload);
    }

    /// Move the focused pane's scrollback view by one of the named anchors.
    /// Also reachable via `AppAction::Scroll*` and Lua `rterm.run_action`.
    fn scroll_view(&self, kind: ScrollNav) {
        let Some(pane) = self.focused_pane() else { return };
        let (max, on_alt) = if let Ok(t) = pane.terminal.lock() {
            (t.scrollback_len() as i32, t.is_on_alt_screen())
        } else {
            (0, false)
        };
        // Alt screen pins viewport to alt grid (iter 263), so PageUp/Home
        // would just update dead state. Mirrors the wheel-scroll guard.
        if on_alt {
            return;
        }
        let page = self
            .state
            .as_ref()
            .map(|s| {
                let (_, rows) = s.text.cells_for(s.config.width, s.config.height, PAD);
                rows.saturating_sub(1) as i32
            })
            .unwrap_or(10);
        let cur = pane.scroll_offset.load(Ordering::Relaxed) as i32;
        let half = half_page_rows(page);
        let next = match kind {
            ScrollNav::PageUp => (cur + page).min(max),
            ScrollNav::PageDown => (cur - page).max(0),
            ScrollNav::HalfPageUp => (cur + half).min(max),
            ScrollNav::HalfPageDown => (cur - half).max(0),
            ScrollNav::LineUp => (cur + 1).min(max),
            ScrollNav::LineDown => (cur - 1).max(0),
            ScrollNav::Home => max,
            ScrollNav::End => 0,
        };
        pane.scroll_offset.store(clamp_scroll_offset(next as i64), Ordering::Relaxed);
    }

    fn open_palette(&mut self) {
        let custom = self.events.list_actions();
        let total = AppAction::ALL.len() + custom.len();
        self.palette = Some(PaletteState {
            query: String::new(),
            custom,
            filtered: (0..total).collect(),
            selected: 0,
            scroll_offset: 0,
        });
        self.events.emit("palette.open", "");
    }

    /// Total number of palette items: built-ins + plugin actions.
    fn palette_action_count(palette: &PaletteState) -> usize {
        AppAction::ALL.len() + palette.custom.len()
    }


    fn close_palette(&mut self) {
        if self.palette.take().is_some() {
            // Same UX touch as `end_search` — user is about to keep typing
            // in the shell, show a visible cursor immediately.
            self.reset_cursor_blink();
            self.events.emit("palette.close", "");
        }
    }

    fn refresh_palette(&mut self) {
        if let Some(p) = self.palette.as_mut() {
            let q = p.query.to_lowercase();
            let total = Self::palette_action_count(p);
            if q.is_empty() {
                p.filtered = (0..total).collect();
            } else {
                let mut scored: Vec<(usize, i32)> = (0..total)
                    .filter_map(|idx| {
                        let label = if idx < AppAction::ALL.len() {
                            AppAction::ALL[idx].1.to_lowercase()
                        } else {
                            p.custom[idx - AppAction::ALL.len()].to_lowercase()
                        };
                        fuzzy_score(&label, &q).map(|s| (idx, s))
                    })
                    .collect();
                // Higher score = better match; stable sort keeps original
                // order for ties so user-defined orderings still surface.
                scored.sort_by_key(|b| std::cmp::Reverse(b.1));
                p.filtered = scored.into_iter().map(|(i, _)| i).collect();
            }
            if p.selected >= p.filtered.len() {
                p.selected = 0;
            }
            p.scroll_offset = 0;
        }
    }

    fn palette_step(&mut self, delta: isize) {
        // Compute the rect-driven page size once with the immutable
        // borrow; THEN take the mutable borrow on `palette` to apply
        // it. Can't interleave the two borrows.
        let visible = self.palette_visible_rows().max(1);
        if let Some(p) = self.palette.as_mut() {
            if p.filtered.is_empty() {
                return;
            }
            let n = p.filtered.len() as isize;
            p.selected = (((p.selected as isize + delta) % n + n) % n) as usize;
            // Pull the viewport along so `selected` is always within
            // `scroll_offset..scroll_offset + visible`.
            let max_off = p.filtered.len().saturating_sub(visible);
            if p.selected < p.scroll_offset {
                p.scroll_offset = p.selected;
            } else if p.selected >= p.scroll_offset + visible {
                p.scroll_offset = p.selected + 1 - visible;
            }
            p.scroll_offset = p.scroll_offset.min(max_off);
        }
    }

    /// Execute the currently-highlighted action. Returns `true` when the
    /// action requests window exit (e.g. closing the last pane).
    fn execute_palette_selection(&mut self) -> bool {
        // Pull resolution out of the palette before closing — close_palette
        // takes ownership.
        let resolved: Option<(Option<AppAction>, Option<String>)> = self.palette.as_ref().and_then(|p| {
            let idx = *p.filtered.get(p.selected)?;
            if idx < AppAction::ALL.len() {
                Some((Some(AppAction::ALL[idx].0), None))
            } else {
                let name = p.custom[idx - AppAction::ALL.len()].clone();
                Some((None, Some(name)))
            }
        });
        self.close_palette();
        match resolved {
            Some((Some(a), _)) => self.dispatch_action(a),
            Some((None, Some(name))) => {
                self.events.run_action(&name);
                false
            }
            _ => false,
        }
    }

    fn dispatch_action(&mut self, a: AppAction) -> bool {
        match a {
            AppAction::NewTab => self.new_tab(),
            AppAction::CloseTab => return self.close_active_tab(),
            AppAction::NextTab => self.switch_tab(1),
            AppAction::PrevTab => self.switch_tab(-1),
            AppAction::FirstTab => self.select_tab(0),
            AppAction::LastTab => {
                if !self.tabs.is_empty() {
                    self.select_tab(self.tabs.len() - 1);
                }
            }
            AppAction::MoveTabLeft => self.move_active_tab(-1),
            AppAction::MoveTabRight => self.move_active_tab(1),
            // Action name convention follows tmux / iTerm: "split
            // horizontal" produces a HORIZONTAL divider line (panes
            // stacked top/bottom), "split vertical" produces a VERTICAL
            // divider (panes side-by-side). `SplitDir` itself describes
            // the LAYOUT axis, so the mapping is inverted from the
            // action name. Users expecting either convention now get
            // tmux-style results.
            AppAction::SplitHorizontal => self.split_active_pane(SplitDir::Vertical),
            AppAction::SplitVertical => self.split_active_pane(SplitDir::Horizontal),
            AppAction::SplitAuto => self.split_active_pane_auto(),
            AppAction::ClosePane => return self.close_focused_pane(),
            AppAction::FocusNextPane => self.focus_pane(1),
            AppAction::FocusPrevPane => self.focus_pane(-1),
            AppAction::FocusFirstPane => self.focus_pane_index(0),
            AppAction::FocusLastPane => {
                let last = self
                    .active_tab()
                    .map(|t| t.pane_count().saturating_sub(1));
                if let Some(i) = last {
                    self.focus_pane_index(i);
                }
            }
            AppAction::PasteClipboard => self.paste_clipboard(),
            AppAction::CopySelection => self.copy_selection(),
            AppAction::ClearSelection => {
                self.selection = None;
            }
            AppAction::StartSearch => self.start_search(),
            AppAction::JumpPrevPrompt => self.jump_to_prompt(-1),
            AppAction::JumpNextPrompt => self.jump_to_prompt(1),
            AppAction::JumpPrevCommand => self.jump_to_command(-1),
            AppAction::JumpNextCommand => self.jump_to_command(1),
            AppAction::ScrollPageUp => self.scroll_view(ScrollNav::PageUp),
            AppAction::ScrollPageDown => self.scroll_view(ScrollNav::PageDown),
            AppAction::ScrollHalfPageUp => self.scroll_view(ScrollNav::HalfPageUp),
            AppAction::ScrollHalfPageDown => self.scroll_view(ScrollNav::HalfPageDown),
            AppAction::ScrollLineUp => self.scroll_view(ScrollNav::LineUp),
            AppAction::ScrollLineDown => self.scroll_view(ScrollNav::LineDown),
            AppAction::ScrollHome => self.scroll_view(ScrollNav::Home),
            AppAction::ScrollEnd => self.scroll_view(ScrollNav::End),
            AppAction::ClearScrollback => self.clear_active_scrollback(),
            AppAction::ResizePaneLeft => self.resize_focused_pane(SpatialDir::Left, 0.05),
            AppAction::ResizePaneRight => self.resize_focused_pane(SpatialDir::Right, 0.05),
            AppAction::ResizePaneUp => self.resize_focused_pane(SpatialDir::Up, 0.05),
            AppAction::ResizePaneDown => self.resize_focused_pane(SpatialDir::Down, 0.05),
            AppAction::OpenPalette => self.open_palette(),
            AppAction::SaveScrollback => self.save_scrollback(),
            AppAction::ZoomPane => self.toggle_zoom(),
            AppAction::BalancePanes => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.tree.balance();
                }
                self.sync_terminal_size();
            }
            AppAction::Quit => return true,
            AppAction::FontIncrease => self.adjust_font_size(1.0),
            AppAction::FontDecrease => self.adjust_font_size(-1.0),
            AppAction::FontReset => {
                let initial = self.initial_font_size;
                self.set_font_size_absolute(initial);
            }
            // Opacity stepping mirrors font_increase / font_decrease: a
            // small fixed delta (5 %) per keypress so a held-down repeat
            // sweeps the full range in about 20 strokes.
            AppAction::OpacityIncrease => self.adjust_opacity(0.05),
            AppAction::OpacityDecrease => self.adjust_opacity(-0.05),
            AppAction::OpacityReset => {
                let initial = self.initial_opacity;
                self.opacity = initial;
                if let Some(state) = self.state.as_mut() {
                    state.set_opacity(initial);
                }
            }
            AppAction::ToggleLastTab => self.toggle_last_tab(),
            AppAction::CopyHoveredUrl => self.copy_hovered_url(),
            AppAction::ResetPane => {
                if let Some(pane) = self.focused_pane() {
                    // Reset OUR terminal-side VT state first (charsets,
                    // scroll region, modes). This makes the local view
                    // sane immediately even if the round-trip below
                    // doesn't reach ConPTY.
                    //
                    // Also drop the scrollback. Default RIS preserves
                    // saved lines per common xterm convention, but
                    // Ctrl+Shift+R is a user-initiated panic button
                    // and the scrollback is exactly what was making
                    // recovery painful: a Windows `cat picture.png`
                    // leaves ~10 000 lines of UTF-8-replacement
                    // garbage in scrollback, and the *next* inline
                    // image (e.g. via imgcat) gets an `abs_row` so
                    // high up the virtual stream that the renderer's
                    // viewport projection puts the quad mostly
                    // out-of-frame. Wiping scrollback as part of the
                    // reset gives the new image a low, sane abs_row
                    // and clears the visible junk on top.
                    if let Ok(mut t) = pane.terminal.lock() {
                        t.advance(b"\x1bc");
                        t.clear_scrollback();
                    }
                    // Drop the pane's cached OSC 0/1/2 title too.
                    // RIS already cleared `terminal.current_title`, but
                    // `pane.dynamic_title` is a snapshot the renderer
                    // pulls every frame via `Terminal::take_title()` —
                    // and `take_title` only updates the snapshot when
                    // the terminal has a new title to hand out, so a
                    // stale (now-None) terminal cache leaves the pane
                    // copy stuck on the pre-RIS garbage.
                    if let Ok(mut dt) = pane.dynamic_title.lock() {
                        *dt = None;
                    }
                    // Ask the shell to emit RIS itself, via two
                    // commands sent as SEPARATE input lines. Earlier
                    // semicolon-joined variant failed: bash treats
                    // `;` as an in-line separator and a syntax error
                    // anywhere on the line (the PowerShell half's
                    // `[Console]::Write` parses as bash `test` with
                    // unbalanced brackets) aborts the WHOLE line —
                    // including the printf half that would otherwise
                    // have emitted RIS. With `\r` between them each
                    // line commits to readline independently: the
                    // shell-appropriate command runs successfully,
                    // the other gets a recoverable error of its own,
                    // and the successful one's `ESC c` travels back
                    // through ConPTY's output parser to clear the
                    // stuck G0/G1 designators.
                    //
                    // Pure ASCII at the keystroke level (the `\033`
                    // is the four characters `\`, `0`, `3`, `3`, not
                    // an ESC byte), so PSReadLine on Windows doesn't
                    // intercept anything — earlier attempt with raw
                    // `\x1bc` lost the ESC to PSReadLine and only
                    // the trailing `c` made it to pwsh as input.
                    // Trailing `\x0c` (Ctrl+L) is a readline / PSReadLine
                    // binding that clears the screen and redraws the
                    // prompt at row 0. RIS already cleared most of
                    // the screen, but the half of the dual-command
                    // pair that DIDN'T match the current shell will
                    // have printed its `command not found` / `syntax
                    // error` complaint into the freshly-reset grid.
                    // Ctrl+L wipes that residual error so the user
                    // ends up on a clean prompt.
                    pane.send_input(
                        b"printf '\\033c'\r[Console]::Write([char]27+'c')\r\x0c",
                    );
                }
            }
            AppAction::SwapPaneNext => self.swap_focused_pane(1),
            AppAction::SwapPanePrev => self.swap_focused_pane(-1),
            AppAction::ToggleBellMute => self.toggle_focused_pane_bell_mute(),
            AppAction::OpenHoveredUrl => self.open_hovered_url(),
            AppAction::ToggleHelp => {
                self.show_help = !self.show_help;
                if !self.show_help {
                    self.reset_cursor_blink();
                }
            }
            AppAction::CycleTheme => self.cycle_theme(1),
            AppAction::CycleThemePrev => self.cycle_theme(-1),
            AppAction::OpenSettings => {
                self.show_settings = !self.show_settings;
                if self.show_settings {
                    // Mutually-exclusive with the help overlay.
                    self.show_help = false;
                }
                self.reset_cursor_blink();
                if let Some(state) = self.state.as_ref() {
                    state.window.request_redraw();
                }
            }
            AppAction::RenameTab => self.start_tab_rename(self.active_tab),
            AppAction::SnapWindowLeft => self.snap_window(SnapDir::Left),
            AppAction::SnapWindowRight => self.snap_window(SnapDir::Right),
            AppAction::SnapWindowTop => self.snap_window(SnapDir::Top),
            AppAction::SnapWindowBottom => self.snap_window(SnapDir::Bottom),
            AppAction::MaximizeToggle => {
                if let Some(state) = self.state.as_ref() {
                    let is_max = state.window.is_maximized();
                    state.window.set_maximized(!is_max);
                }
            }
            AppAction::MinimizeWindow => {
                if let Some(state) = self.state.as_ref() {
                    state.window.set_minimized(true);
                }
            }
            AppAction::RestoreWindow => self.restore_window(),
            AppAction::ToggleGuake => self.toggle_guake(),
        }
        false
    }

    /// Open the rename overlay for tab `idx`. Pre-fills the buffer with
    /// the current custom title (or the auto-derived display title) so
    /// the user can tweak rather than retype.
    fn start_tab_rename(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        // Close other modals so the rename has sole keyboard focus.
        self.show_help = false;
        self.show_settings = false;
        self.show_app_menu = false;
        if self.palette.is_some() {
            self.close_palette();
        }
        self.context_menu = None;
        let tab = &self.tabs[idx];
        let initial = tab
            .custom_title
            .clone()
            .or_else(|| tab.focused_pane().map(|p| p.display_title()))
            .unwrap_or_default();
        let cursor = initial.len();
        self.rename_tab = Some(RenameTabState {
            tab_idx: idx,
            buffer: initial,
            cursor,
            pristine: true,
        });
        self.reset_cursor_blink();
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }

    /// Commit the rename buffer to the tab's custom title and close the
    /// overlay. Empty input clears any existing override so the tab
    /// falls back to its auto-derived title.
    fn commit_tab_rename(&mut self) {
        let Some(rt) = self.rename_tab.take() else { return };
        if let Some(tab) = self.tabs.get_mut(rt.tab_idx) {
            let trimmed = rt.buffer.trim();
            tab.custom_title = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
            let payload = tab_title_payload(rt.tab_idx + 1, tab.custom_title.as_deref().unwrap_or(""));
            self.events.emit("tab.title", &payload);
        }
        self.update_title();
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }

    /// Close the rename overlay without applying any changes.
    fn cancel_tab_rename(&mut self) {
        self.rename_tab = None;
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }

    /// Build the rename overlay text spans. The caret blinks (driven
    /// by the same `cursor_blink_anchor` as the pane cursor) and the
    /// pristine prefill is rendered with a "selection" accent so the
    /// user sees that typing will replace it.
    fn rename_spans<'a>(
        &self,
        rt: &RenameTabState,
        storage: &'a mut Vec<String>,
    ) -> Vec<(&'a str, [u8; 3], bool)> {
        storage.clear();
        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        let fg = palette::default_fg();
        let muted = fg.map(|c| c.saturating_sub(80));
        let accent: [u8; 3] = [120, 180, 240];
        let selection: [u8; 3] = [255, 204, 102];
        storage.push(format!("Rename tab {}\n\n", rt.tab_idx + 1));
        spans.push((storage.len() - 1, accent, true));
        storage.push("  > ".to_string());
        spans.push((storage.len() - 1, fg, false));
        let blink_on = self.cursor_blink_on();
        let split = rt.cursor.min(rt.buffer.len());
        let (left, right) = rt.buffer.split_at(split);
        if rt.pristine && !rt.buffer.is_empty() {
            // Whole prefilled buffer rendered in "selection" colour so
            // the user notices the next keypress will replace it.
            storage.push(rt.buffer.clone());
            spans.push((storage.len() - 1, selection, true));
        } else {
            storage.push(left.to_string());
            spans.push((storage.len() - 1, fg, false));
            // Blinking caret. `▏` when on, single space when off — keeps
            // the line width stable so right-side text doesn't jitter.
            storage.push(if blink_on { "▏".to_string() } else { " ".to_string() });
            spans.push((storage.len() - 1, accent, true));
            storage.push(right.to_string());
            spans.push((storage.len() - 1, fg, false));
        }
        storage.push("\n\n".to_string());
        spans.push((storage.len() - 1, fg, false));
        storage.push(
            "  Enter to apply · Esc to cancel · Empty clears the title\n  \
             ← → Home End move · Ctrl+W word · Ctrl+U clear\n"
                .to_string(),
        );
        spans.push((storage.len() - 1, muted, false));
        spans
            .into_iter()
            .map(|(i, c, b)| (storage[i].as_str(), c, b))
            .collect()
    }

    /// Keyboard handler for the rename overlay. Supports basic line
    /// editing: arrows + Home/End move the caret, Ctrl+W deletes the
    /// previous word, Ctrl+U clears, Backspace pops one grapheme. The
    /// "pristine" flag mimics browser select-all-on-focus: the first
    /// printable character replaces the prefilled title in one shot.
    /// Keyboard input while the paste-confirmation modal is up.
    ///
    /// In `Confirm` mode:
    /// * `Tab` / `Shift+Tab` cycle the focused button.
    /// * `←` / `→` also cycle (mirror Tab nav — both feel natural).
    /// * `Enter` activates the focused button.
    /// * `Esc` cancels outright.
    /// * `P` / `E` / `C` shortcuts jump straight to Paste / Edit /
    ///   Cancel respectively (first-letter accelerators).
    ///
    /// In `Edit` mode:
    /// * Printable chars + Enter → insert at cursor (Enter inserts
    ///   `\n`, NOT submit — multi-line content is the point).
    /// * `Backspace` / `Delete` → mutate.
    /// * `←` / `→` / `↑` / `↓` → cursor nav.
    /// * `Home` / `End` → start / end of current line.
    /// * `Ctrl+Enter` → finish editing, paste the buffer.
    /// * `Esc` → cancel outright (drops the edited buffer).
    fn handle_paste_confirmation_key(&mut self, event: &KeyEvent) {
        use paste_confirm::{PasteButton, PasteMode};
        // Compute the viewport up front: the PageUp / PageDown
        // arms need it for `edit_up_n` / `edit_down_n`, and calling
        // `paste_modal_visible_rows(&self)` from inside the
        // `&mut modal` scope below would trigger E0502.
        let viewport = self.paste_modal_visible_rows().saturating_sub(1).max(1);
        // Width that the editor wraps at — needed by the display-row
        // aware up/down navigation. Computed before the `&mut` borrow.
        let wrap_cols = self.paste_modal_wrap_cols();
        let Some(modal) = self.paste_confirmation.as_mut() else { return };
        let ctrl = self.modifiers.contains(ModifiersState::CONTROL);
        let shift = self.modifiers.contains(ModifiersState::SHIFT);
        match &mut modal.mode {
            PasteMode::Confirm { selected } => match &event.logical_key {
                Key::Named(NamedKey::Tab) | Key::Named(NamedKey::ArrowRight) => {
                    *selected = if shift { selected.prev() } else { selected.next() };
                }
                Key::Named(NamedKey::ArrowLeft) => {
                    *selected = selected.prev();
                }
                Key::Named(NamedKey::Enter) => {
                    let action = *selected;
                    self.activate_paste_button(action);
                }
                Key::Named(NamedKey::Escape) => {
                    self.paste_confirmation = None;
                }
                Key::Character(c) => match c.as_str() {
                    "p" | "P" => self.activate_paste_button(PasteButton::Paste),
                    "e" | "E" => self.activate_paste_button(PasteButton::Edit),
                    "c" | "C" => self.activate_paste_button(PasteButton::Cancel),
                    _ => {}
                },
                _ => {}
            },
            PasteMode::Edit { .. } => {
                // Shift+nav extends the selection; bare nav clears
                // it. Decide BEFORE the move so the anchor is in
                // place when `edit_*` updates the cursor.
                let is_nav = matches!(
                    &event.logical_key,
                    Key::Named(
                        NamedKey::ArrowLeft
                            | NamedKey::ArrowRight
                            | NamedKey::ArrowUp
                            | NamedKey::ArrowDown
                            | NamedKey::Home
                            | NamedKey::End
                            | NamedKey::PageUp
                            | NamedKey::PageDown,
                    ),
                );
                if is_nav {
                    if shift {
                        modal.start_extending();
                    } else {
                        modal.clear_selection();
                    }
                }
                match &event.logical_key {
                    Key::Named(NamedKey::Enter) if ctrl => {
                        // Ctrl+Enter applies the edited buffer.
                        let text = modal.text.clone();
                        let uid = modal.pane_uid;
                        self.paste_confirmation = None;
                        self.commit_paste_now(&text, Some(uid));
                    }
                    Key::Named(NamedKey::Enter) => {
                        modal.edit_insert('\n');
                    }
                    Key::Named(NamedKey::Escape) => {
                        self.paste_confirmation = None;
                    }
                    Key::Named(NamedKey::Backspace) => modal.edit_backspace(),
                    Key::Named(NamedKey::Delete) => modal.edit_delete(),
                    Key::Named(NamedKey::ArrowLeft) => modal.edit_left(),
                    Key::Named(NamedKey::ArrowRight) => modal.edit_right(),
                    Key::Named(NamedKey::ArrowUp) => modal.edit_up(wrap_cols),
                    Key::Named(NamedKey::ArrowDown) => modal.edit_down(wrap_cols),
                    Key::Named(NamedKey::Home) => modal.edit_home(),
                    Key::Named(NamedKey::End) => modal.edit_end(),
                    Key::Named(NamedKey::PageUp) => modal.edit_up_n(viewport, wrap_cols),
                    Key::Named(NamedKey::PageDown) => modal.edit_down_n(viewport, wrap_cols),
                    Key::Named(NamedKey::Tab) => modal.edit_insert('\t'),
                    Key::Named(NamedKey::Space) => modal.edit_insert(' '),
                    // Ctrl shortcuts come before the generic
                    // Character arm so they don't fall through to
                    // `edit_insert_str("v")`. Both Ctrl+X and
                    // Ctrl+Shift+X are accepted — Linux terminals
                    // historically used the Shift form to avoid
                    // collision with shell signals, but the modal
                    // is past the shell so either feels natural.
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("a") => {
                        modal.select_all();
                    }
                    // Copy/cut route through `clipboard_set`, NOT a
                    // throwaway `arboard::Clipboard` — on X11/Wayland
                    // selection ownership dies with the connection, so
                    // a clipboard dropped at the end of this arm lost
                    // the copy the moment the handler returned (see
                    // clipboard.rs for the owner-thread rationale).
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("c") => {
                        if let Some(sel) = modal.selected_text() {
                            clipboard_set(sel);
                        }
                    }
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("x") => {
                        if let Some(sel) = modal.selected_text() {
                            clipboard_set(sel);
                            modal.delete_selection();
                        }
                    }
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("v") => {
                        // Skip the multi-line safety prompt — we're
                        // already inside it. Just splice the
                        // clipboard text in at the cursor (replacing
                        // the active selection if any).
                        if let Ok(mut cb) = arboard::Clipboard::new() {
                            if let Ok(text) = cb.get_text() {
                                // Mirror `new_confirm`'s CRLF
                                // collapse so a Windows clipboard
                                // payload doesn't render as
                                // double-spaced.
                                let normalized = if text.contains('\r') {
                                    text.replace("\r\n", "\n").replace('\r', "\n")
                                } else {
                                    text
                                };
                                modal.edit_insert_str(&normalized);
                            }
                        }
                    }
                    Key::Character(c) => modal.edit_insert_str(c.as_str()),
                    _ => {}
                }
            }
        }
        // Any cursor-moving key (arrows, typing, PgUp/PgDn, ...)
        // may have moved the caret out of the visible viewport;
        // pull it back in. NOT called on wheel — wheel deliberately
        // decouples viewport from cursor so the user can scan above
        // / below the caret without it being yanked back.
        self.clamp_paste_modal_scroll();
    }

    /// Mouse-click on the paste-confirmation modal. Confirm mode:
    /// hit-test the button row. Edit mode: project the click onto
    /// a (line, column) in the buffer and move the cursor there.
    fn handle_paste_confirmation_press(&mut self, x: f64, y: f64) {
        use paste_confirm::PasteMode;
        let Some(modal) = self.paste_confirmation.clone() else { return };
        match modal.mode {
            PasteMode::Confirm { .. } => {
                self.paste_modal_press_confirm(&modal, x, y);
            }
            PasteMode::Edit { .. } => {
                self.paste_modal_press_edit(x, y);
            }
        }
    }

    /// Button hit-test for the Confirm dialog. Clicks outside any
    /// button are absorbed but ignored — the modal stays up.
    fn paste_modal_press_confirm(
        &mut self,
        modal: &paste_confirm::PasteConfirmation,
        x: f64,
        y: f64,
    ) {
        let Some(rect) = self.paste_confirmation_rect(modal) else { return };
        let Some(state) = self.state.as_ref() else { return };
        let cell_w = state.text.cell_width();
        let line_h = state.text.line_height();
        if cell_w <= 0.0 || line_h <= 0.0 {
            return;
        }
        // Re-derive the button-row Y from the same layout the
        // renderer used: header (1) + blank (1) + preview rows +
        // optional "+N more" line + blank (1) = button row index.
        let preview_total = modal.line_count();
        let preview_rendered = preview_total.min(12);
        let more_line = usize::from(preview_total > 12);
        let button_row_index = 1 + 1 + preview_rendered + more_line + 1;
        // Hit-zone is 4 rows tall: the actual button row plus 3
        // rows below it (the blank row, the hint line, and one row
        // of slack). Real-world clicks routinely landed 1–2 rows
        // below the visual button when the modal was densely
        // packed with a long preview; widening the zone catches
        // them without overlapping the preview text above.
        let yf = y as f32;
        let row_top = rect.top + button_row_index as f32 * line_h;
        let row_bottom = rect.top + (button_row_index + 4) as f32 * line_h;
        if yf < row_top || yf >= row_bottom {
            return;
        }
        let xf = x as f32;
        let mut col = paste_confirm::BUTTON_LEADING_CELLS as f32;
        for btn in [
            paste_confirm::PasteButton::Paste,
            paste_confirm::PasteButton::Edit,
            paste_confirm::PasteButton::Cancel,
        ] {
            let left = rect.left + col * cell_w;
            let right = left + paste_confirm::BUTTON_LABEL_CELLS as f32 * cell_w;
            if xf >= left && xf < right {
                self.activate_paste_button(btn);
                return;
            }
            col += paste_confirm::BUTTON_LABEL_CELLS as f32
                + paste_confirm::BUTTON_GAP_CELLS as f32;
        }
    }

    /// Project a pixel `(x, y)` inside the paste modal's Edit area
    /// onto a byte offset in the buffer. Returns `None` when the
    /// click landed on the modal header / footer / outside the rect.
    /// Shared by the click-down handler (which seeds the selection
    /// anchor) and the drag handler (which extends from the anchor).
    fn paste_modal_hit_test(&self, x: f64, y: f64) -> Option<usize> {
        let state = self.state.as_ref()?;
        let cell_w = state.text.cell_width();
        let line_h = state.text.line_height();
        if cell_w <= 0.0 || line_h <= 0.0 {
            return None;
        }
        let modal_ref = self.paste_confirmation.as_ref()?;
        let rect = self.paste_confirmation_rect(modal_ref)?;
        let yf = y as f32;
        let xf = x as f32;
        if xf < rect.left
            || xf >= rect.left + rect.width
            || yf < rect.top
            || yf >= rect.top + rect.height
        {
            return None;
        }
        const HEADER_ROWS: usize = 2;
        let row_in_modal = ((yf - rect.top) / line_h) as usize;
        if row_in_modal < HEADER_ROWS {
            return None;
        }
        let viewport_row = row_in_modal - HEADER_ROWS;
        let visible_rows = self.paste_modal_visible_rows();
        if viewport_row >= visible_rows {
            return None;
        }
        let cursor_byte = match modal_ref.mode {
            paste_confirm::PasteMode::Edit { cursor, .. } => cursor.min(modal_ref.text.len()),
            _ => return None,
        };
        // Project onto soft-wrapped DISPLAY rows, not raw logical
        // lines — the renderer slices the same rows, so `scroll +
        // viewport_row` lands on exactly the row under the cursor.
        let wrap_cols = self.paste_modal_wrap_cols();
        let rows = paste_confirm::display_rows(&modal_ref.text, wrap_cols);
        let scroll = modal_ref.scroll_line();
        let absolute_row = scroll + viewport_row;
        if absolute_row >= rows.len() {
            return Some(modal_ref.text.len());
        }
        let dr = rows[absolute_row];
        let cursor_row = paste_confirm::cursor_row_idx(&rows, cursor_byte);
        const LINE_INDENT_CELLS: f32 = 2.0;
        let raw_col_cells = ((xf - rect.left) / cell_w - LINE_INDENT_CELLS).max(0.0);
        let mut target_col = raw_col_cells as usize;
        // Cursor mark `▏` occupies a full cell on its row; clicks
        // past the caret pick up one extra column visually. Pull it
        // back so the projected byte matches the source text.
        if absolute_row == cursor_row {
            let cursor_col_chars =
                modal_ref.text[dr.start..cursor_byte.min(dr.end)].chars().count();
            if target_col > cursor_col_chars {
                target_col = target_col.saturating_sub(1);
            }
        }
        // Map the column (char count) within this display row to a
        // byte offset in the buffer.
        let seg = &modal_ref.text[dr.start..dr.end];
        let mut byte_in_seg = seg.len();
        for (i, (offset, _)) in seg.char_indices().enumerate() {
            if i == target_col {
                byte_in_seg = offset;
                break;
            }
        }
        Some(dr.start + byte_in_seg)
    }

    /// Click-to-position in the Edit mini-editor. Pixel coords →
    /// (visible_row, cell_col) → absolute (line, char column) →
    /// byte offset in the buffer, then `cursor` jumps there.
    /// Also plants the selection anchor at the new cursor position
    /// and arms the drag flag so a subsequent mouse-move extends
    /// the selection from this point.
    fn paste_modal_press_edit(&mut self, x: f64, y: f64) {
        let Some(new_cursor) = self.paste_modal_hit_test(x, y) else { return };
        if let Some(m) = self.paste_confirmation.as_mut() {
            let pos = new_cursor.min(m.text.len());
            if let paste_confirm::PasteMode::Edit { cursor, selection_anchor, .. } = &mut m.mode {
                *cursor = pos;
                // Anchor at the click position so a subsequent
                // drag extends from here. A release without any
                // drag leaves anchor == cursor → no selection.
                *selection_anchor = Some(pos);
            }
        }
        self.paste_modal_dragging = true;
        self.clamp_paste_modal_scroll();
    }

    /// Mouse-move while LMB is held inside the modal Edit area.
    /// Moves the cursor to the new pixel position; the anchor that
    /// `paste_modal_press_edit` planted stays put, so the selection
    /// range grows / shrinks as the user drags.
    fn paste_modal_drag_edit(&mut self, x: f64, y: f64) {
        let Some(new_cursor) = self.paste_modal_hit_test(x, y) else { return };
        if let Some(m) = self.paste_confirmation.as_mut() {
            let pos = new_cursor.min(m.text.len());
            if let paste_confirm::PasteMode::Edit { cursor, .. } = &mut m.mode {
                *cursor = pos;
            }
        }
        self.clamp_paste_modal_scroll();
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }

    /// Mouse-wheel handling for the paste-confirmation modal. In
    /// edit mode, the wheel shifts the VIEWPORT (not the cursor) by
    /// a few lines per tick — same convention as every text editor.
    /// The cursor stays where the user left it; if they keep
    /// scrolling past it, the caret simply scrolls off-screen. As
    /// soon as they type / arrow, the clamp pulls the viewport back.
    /// In confirm mode, the wheel is absorbed silently to keep a
    /// stray scroll from leaking through to the pane below.
    fn handle_paste_confirmation_wheel(&mut self, delta: MouseScrollDelta) {
        use paste_confirm::PasteMode;
        // Take viewport rows in a pre-pass so we don't hold a
        // mut borrow across the call.
        let visible_rows = self.paste_modal_visible_rows();
        let wrap_cols = self.paste_modal_wrap_cols();
        let Some(modal) = self.paste_confirmation.as_mut() else { return };
        let PasteMode::Edit { .. } = modal.mode else {
            return; // Confirm mode: absorb only.
        };
        // ~3 lines per wheel notch matches browser / VSCode
        // defaults. Touchpad pixel-deltas are normalised against
        // the same scale.
        let step = match delta {
            MouseScrollDelta::LineDelta(_, y) => y * 3.0,
            MouseScrollDelta::PixelDelta(p) => (p.y as f32) / 12.0,
        };
        let lines = step.round() as i32;
        if lines == 0 {
            return;
        }
        // Wheel up (positive delta in winit) → scroll buffer UP
        // visually = view EARLIER lines. We shift the viewport
        // WITHOUT moving the cursor — matches every editor's
        // wheel-scroll convention. Cursor stays where the user
        // left it; the renderer's clamp will pull the viewport
        // back when the user types again.
        modal.scroll_by(-(lines as i64), visible_rows, wrap_cols);
    }

    /// Clamp the paste modal's `scroll_line` so the cursor stays
    /// inside the viewport. Idempotent — when the cursor is
    /// already in view, `scroll_line` is untouched. Called from
    /// every cursor-moving path (key handler, click positioner,
    /// `enter_edit_mode`). DELIBERATELY NOT called per frame:
    /// doing so would yank the viewport back to the cursor and
    /// fight the wheel handler's intentional decoupling.
    fn clamp_paste_modal_scroll(&mut self) {
        let visible = self.paste_modal_visible_rows();
        let wrap_cols = self.paste_modal_wrap_cols();
        if let Some(modal) = self.paste_confirmation.as_mut() {
            modal.ensure_cursor_visible(visible, wrap_cols);
        }
    }

    /// Number of visible buffer rows in the paste-modal's edit
    /// viewport. Mirrors the math in
    /// `paste_confirmation_spans`'s Edit branch so PageUp / PageDown
    /// move by exactly one viewport-minus-one regardless of the
    /// current rect height. Fallback (cell metrics not ready
    /// yet) of 10 rows is conservative enough to feel responsive
    /// without overshooting.
    fn paste_modal_visible_rows(&self) -> usize {
        let Some(modal) = self.paste_confirmation.as_ref() else { return 10 };
        let line_h = self
            .state
            .as_ref()
            .map(|s| s.text.line_height())
            .filter(|h| *h > 0.0)
            .unwrap_or(20.0);
        let rect_h = self
            .paste_confirmation_rect(modal)
            .map(|r| r.height)
            .unwrap_or(400.0);
        let total_rows = (rect_h / line_h) as usize;
        // Same reservation as the renderer: 2 header rows + 1
        // bottom-padding row.
        total_rows.saturating_sub(3).max(1)
    }

    /// Text columns available per row in the paste-modal editor, after
    /// reserving the left margin (line-start indent / `↪ ` marker) and
    /// one trailing cell for the caret `▏` / `↵` glyph. The renderer,
    /// hit-test, and selection quads all derive their wrap point from
    /// this single source so they never disagree. Falls back to a wide
    /// default before cell metrics are ready.
    fn paste_modal_wrap_cols(&self) -> usize {
        let cell_w = self
            .state
            .as_ref()
            .map(|s| s.text.cell_width())
            .filter(|w| *w > 0.0)
            .unwrap_or(8.0);
        let rect_w = self
            .paste_confirmation
            .as_ref()
            .and_then(|m| self.paste_confirmation_rect(m))
            .map(|r| r.width)
            .unwrap_or(680.0);
        let inner_cols = (rect_w / cell_w) as usize;
        inner_cols
            .saturating_sub(PASTE_EDIT_INDENT_CELLS + 1)
            .max(1)
    }

    /// Rectangles for the active text selection inside the paste-
    /// modal Edit area. One quad per visible line that intersects
    /// the selection range. Sits behind the overlay text so the
    /// selected glyphs render on top with their normal colour.
    /// Returns an empty vec when there's no modal / no selection
    /// or when the modal isn't in Edit mode.
    fn paste_modal_selection_quads(&self) -> Vec<bg::BgQuad> {
        let modal = match self.paste_confirmation.as_ref() {
            Some(m) => m,
            None => return Vec::new(),
        };
        let (sel_lo, sel_hi) = match modal.selection_range() {
            Some(r) => r,
            None => return Vec::new(),
        };
        let Some(state) = self.state.as_ref() else { return Vec::new() };
        let cell_w = state.text.cell_width();
        let line_h = state.text.line_height();
        if cell_w <= 0.0 || line_h <= 0.0 {
            return Vec::new();
        }
        let Some(rect) = self.paste_confirmation_rect(modal) else { return Vec::new() };
        const HEADER_ROWS: usize = 2;
        const LINE_INDENT_CELLS: f32 = 2.0;
        let visible_rows = self.paste_modal_visible_rows();
        let scroll = modal.scroll_line();
        let text = modal.text.as_str();
        // Highlight per soft-wrapped DISPLAY row, matching the renderer.
        let wrap_cols = self.paste_modal_wrap_cols();
        let rows = paste_confirm::display_rows(text, wrap_cols);
        let end_visible = (scroll + visible_rows).min(rows.len());
        let mut quads: Vec<bg::BgQuad> = Vec::new();
        let accent = palette::default_fg().map(|c| c.saturating_sub(40));
        for (offset, dr) in rows[scroll..end_visible].iter().enumerate() {
            // Range of the selection that falls within this row's
            // content [dr.start, dr.end).
            let lo = sel_lo.max(dr.start);
            let hi = sel_hi.min(dr.end);
            if hi < lo {
                continue;
            }
            // A selection that swallows the row's real newline gets a
            // 1-cell trailing highlight so the user sees the `\n` was
            // taken too — matches VS Code. Only hard-newline rows have
            // a newline to select; soft-wrap boundaries don't.
            let trailing_newline = dr.ends_newline && sel_hi > dr.end;
            if hi == lo && !trailing_newline {
                continue;
            }
            let pre_chars = text[dr.start..lo].chars().count();
            let in_chars = text[lo..hi].chars().count();
            let left = rect.left + (LINE_INDENT_CELLS + pre_chars as f32) * cell_w;
            let mut width = in_chars as f32 * cell_w;
            if trailing_newline {
                width += cell_w;
            }
            if width <= 0.0 {
                continue;
            }
            let row_top = rect.top + (HEADER_ROWS + offset) as f32 * line_h;
            quads.push(bg::BgQuad::from_srgb(
                [left, row_top],
                [width, line_h],
                accent,
                0.35,
            ));
        }
        quads
    }

    /// Apply the user's button choice in the confirm modal.
    fn activate_paste_button(&mut self, btn: paste_confirm::PasteButton) {
        use paste_confirm::PasteButton;
        match btn {
            PasteButton::Paste => {
                if let Some(modal) = self.paste_confirmation.take() {
                    self.commit_paste_now(&modal.text, Some(modal.pane_uid));
                }
            }
            PasteButton::Edit => {
                if let Some(modal) = self.paste_confirmation.as_mut() {
                    modal.enter_edit_mode();
                }
                // Caret lands at end-of-buffer on entry; pull the
                // viewport down to it so a long paste doesn't open
                // with the cursor invisibly past the bottom.
                self.clamp_paste_modal_scroll();
            }
            PasteButton::Cancel => {
                self.paste_confirmation = None;
            }
        }
    }

    /// Process a keystroke while the suggestion popup is visible.
    /// Returns `true` when the key was consumed (don't forward to
    /// the PTY); `false` lets the key reach the shell.
    ///
    /// UX rules — refined after two rounds of user feedback:
    /// * Both `↑` and `↓` pass through to the shell WITHOUT a
    ///   selection — the user must be able to use the shell's
    ///   own history-recall (readline / psreadline) at any time.
    /// * `Ctrl+Space` focuses the popup (= selects the top row).
    ///   Standard IDE "show / focus completion" gesture.
    /// * Once focused, `↑` / `↓` navigate; `TAB` injects; `Esc`
    ///   blurs the popup (back to shell-arrow-passthrough); a
    ///   second `Esc` closes the popup outright.
    /// * `Enter` always closes (the user is submitting); the
    ///   keystroke itself passes through to the PTY.
    /// * Mouse click on an entry injects directly — handled in
    ///   the mouse-click path, not here.
    fn handle_suggestion_popup_key(&mut self, event: &KeyEvent) -> bool {
        let visible_rows = self.history_popup_cfg.popup_rows.max(1) as usize;
        let Some(popup) = self.suggestion_popup.as_mut() else { return false };
        let ctrl = self.modifiers.contains(ModifiersState::CONTROL);
        match &event.logical_key {
            Key::Named(NamedKey::Space) if ctrl => {
                // Ctrl+Space focuses the popup. Selects row 0 if
                // nothing selected; otherwise advances by one (so
                // repeated presses cycle through suggestions).
                popup.nav_down(visible_rows);
                true
            }
            Key::Named(NamedKey::ArrowDown) if popup.has_selection() => {
                popup.nav_down(visible_rows);
                true
            }
            Key::Named(NamedKey::ArrowUp) if popup.has_selection() => {
                popup.nav_up(visible_rows);
                true
            }
            Key::Named(NamedKey::Escape) => {
                if popup.has_selection() {
                    // First Esc blurs the popup (= deselect).
                    popup.selected = None;
                    popup.scroll = 0;
                } else {
                    // Second Esc closes outright.
                    self.suggestion_popup = None;
                }
                true
            }
            Key::Named(NamedKey::Tab) if popup.has_selection() => {
                let text = popup.take_selected();
                self.suggestion_popup = None;
                if let Some(text) = text {
                    // ^U clears the line in bash/zsh/fish and in
                    // PowerShell+psreadline (BackwardDeleteLine);
                    // then we type the suggestion in literally.
                    // Both bytes go through send_input so our own
                    // capture buffer ends up holding the
                    // suggestion text for the next Enter.
                    if let Some(pane) = self.focused_pane() {
                        pane.send_input(b"\x15");
                        pane.send_input(text.as_bytes());
                    }
                }
                true
            }
            Key::Named(NamedKey::Enter) => {
                // User is submitting — close the popup but let
                // Enter through to the PTY (the regular submit
                // path will record the command via the capture).
                self.suggestion_popup = None;
                false
            }
            _ => false,
        }
    }

    /// Mouse-click on the suggestion popup. Returns `true` when the
    /// click landed inside the popup rect (and was therefore
    /// consumed); `false` lets the caller treat the click as
    /// "outside" and close the popup before continuing normal
    /// dispatch.
    ///
    /// Hit-test: convert the click's row inside the popup to an
    /// entry index via `(y - rect.top - pad) / line_h + scroll`.
    /// Out-of-range rows (e.g. clicking the "↓ N more" trailer
    /// line) still count as inside the rect, but no row is
    /// selected — the popup closes and nothing is injected.
    fn handle_suggestion_popup_press(&mut self, x: f64, y: f64) -> bool {
        let Some(popup) = self.suggestion_popup.clone() else { return false };
        let Some(rect) = self.suggestion_popup_rect(&popup) else { return false };
        let xf = x as f32;
        let yf = y as f32;
        if xf < rect.left
            || xf >= rect.left + rect.width
            || yf < rect.top
            || yf >= rect.top + rect.height
        {
            return false;
        }
        // Convert pixel offset → entry index. The spans builder
        // uses `line_height()` per row and a 2px vertical pad.
        let line_h = self
            .state
            .as_ref()
            .map(|s| s.text.line_height())
            .filter(|h| *h > 0.0)
            .unwrap_or(16.0);
        let row_in_popup = ((yf - rect.top - 2.0) / line_h) as usize;
        let entry_idx = popup.scroll + row_in_popup;
        if let Some(entry) = popup.entries.get(entry_idx) {
            let text = entry.text.clone();
            self.suggestion_popup = None;
            if let Some(pane) = self.focused_pane() {
                pane.send_input(b"\x15");
                pane.send_input(text.as_bytes());
            }
        } else {
            // Click on the trailer / past the last row → just
            // close, no inject.
            self.suggestion_popup = None;
        }
        true
    }

    /// Per-frame refresh: query the history store when input has
    /// settled, install / dismiss the popup. Called from the redraw
    /// branch. The cost is one `Instant::now()` + one mutex lock on
    /// the focused pane's capture buffer when nothing changed, and
    /// a `SELECT … LIMIT N` against the SQLite store when the
    /// debounce window has elapsed.
    fn refresh_suggestion_popup(&mut self) {
        if !self.history_popup_cfg.enabled {
            // Master kill switch — never arm. Existing popup gets
            // closed if the user flipped the toggle hot.
            self.suggestion_popup = None;
            return;
        }
        let Some(history) = self.history.clone() else {
            self.suggestion_popup = None;
            return;
        };
        let Some(pane) = self.focused_pane() else {
            self.suggestion_popup = None;
            return;
        };
        let pane_uid = pane.uid;
        let current_input = pane.command_capture.current_input();
        let generation = pane.command_capture.generation();
        // Detect "did the user type something new?" by watching
        // the generation counter; bumps mean we should re-arm the
        // debouncer.
        let now = Instant::now();
        if generation != self.last_capture_generation {
            self.last_capture_generation = generation;
            self.last_input_change_at = Some(now);
        }
        let last_input_at = self.last_input_change_at;
        let existing_ref = self.suggestion_popup.as_ref();
        let transition = suggestion_popup::compute(
            &self.history_popup_cfg,
            &history,
            existing_ref,
            pane_uid,
            generation,
            &current_input,
            last_input_at,
            now,
        );
        match transition {
            suggestion_popup::StateTransition::Open(popup) => {
                self.suggestion_popup = Some(popup);
            }
            suggestion_popup::StateTransition::Close => {
                self.suggestion_popup = None;
            }
            suggestion_popup::StateTransition::Keep => {}
        }
    }

    fn handle_rename_key(&mut self, event: &KeyEvent) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        let ctrl = self.modifiers.contains(ModifiersState::CONTROL);
        let mut key_consumed = true;
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.cancel_tab_rename(),
            Key::Named(NamedKey::Enter) => self.commit_tab_rename(),
            Key::Named(NamedKey::Backspace) => {
                if let Some(rt) = self.rename_tab.as_mut() {
                    rt.pristine = false;
                    if rt.cursor > 0 {
                        let mut idx = rt.cursor.saturating_sub(1);
                        while idx > 0 && !rt.buffer.is_char_boundary(idx) {
                            idx -= 1;
                        }
                        rt.buffer.replace_range(idx..rt.cursor, "");
                        rt.cursor = idx;
                    }
                }
            }
            Key::Named(NamedKey::Delete) => {
                if let Some(rt) = self.rename_tab.as_mut() {
                    rt.pristine = false;
                    let mut end = rt.cursor + 1;
                    while end <= rt.buffer.len() && !rt.buffer.is_char_boundary(end) {
                        end += 1;
                    }
                    if end <= rt.buffer.len() {
                        rt.buffer.replace_range(rt.cursor..end, "");
                    }
                }
            }
            Key::Named(NamedKey::ArrowLeft) => {
                if let Some(rt) = self.rename_tab.as_mut() {
                    rt.pristine = false;
                    if rt.cursor > 0 {
                        let mut idx = rt.cursor.saturating_sub(1);
                        while idx > 0 && !rt.buffer.is_char_boundary(idx) {
                            idx -= 1;
                        }
                        rt.cursor = idx;
                    }
                }
                self.reset_cursor_blink();
            }
            Key::Named(NamedKey::ArrowRight) => {
                if let Some(rt) = self.rename_tab.as_mut() {
                    rt.pristine = false;
                    let mut idx = rt.cursor + 1;
                    while idx <= rt.buffer.len() && !rt.buffer.is_char_boundary(idx) {
                        idx += 1;
                    }
                    rt.cursor = idx.min(rt.buffer.len());
                }
                self.reset_cursor_blink();
            }
            Key::Named(NamedKey::Home) => {
                if let Some(rt) = self.rename_tab.as_mut() {
                    rt.pristine = false;
                    rt.cursor = 0;
                }
                self.reset_cursor_blink();
            }
            Key::Named(NamedKey::End) => {
                if let Some(rt) = self.rename_tab.as_mut() {
                    rt.pristine = false;
                    rt.cursor = rt.buffer.len();
                }
                self.reset_cursor_blink();
            }
            Key::Character(c) if ctrl => {
                // Ctrl+letter shortcuts inside the rename overlay.
                match c.as_str().to_ascii_lowercase().as_str() {
                    "u" => {
                        // Ctrl+U: clear the line, matches readline /
                        // search overlay convention.
                        if let Some(rt) = self.rename_tab.as_mut() {
                            rt.buffer.clear();
                            rt.cursor = 0;
                            rt.pristine = false;
                        }
                    }
                    "w" => {
                        // Ctrl+W: delete previous word.
                        if let Some(rt) = self.rename_tab.as_mut() {
                            rt.pristine = false;
                            let bytes = rt.buffer.as_bytes();
                            let mut idx = rt.cursor;
                            // Skip trailing whitespace.
                            while idx > 0
                                && bytes[idx - 1].is_ascii_whitespace()
                            {
                                idx -= 1;
                            }
                            // Skip preceding non-whitespace.
                            while idx > 0
                                && !bytes[idx - 1].is_ascii_whitespace()
                            {
                                idx -= 1;
                            }
                            while idx > 0 && !rt.buffer.is_char_boundary(idx) {
                                idx -= 1;
                            }
                            rt.buffer.replace_range(idx..rt.cursor, "");
                            rt.cursor = idx;
                        }
                    }
                    _ => key_consumed = false,
                }
            }
            Key::Character(c) => {
                if let Some(rt) = self.rename_tab.as_mut() {
                    // Pristine → first printable char replaces the
                    // prefilled title entirely (Chrome/Firefox URL bar
                    // "select-all on focus" feel without true
                    // selection).
                    if rt.pristine {
                        rt.buffer.clear();
                        rt.cursor = 0;
                        rt.pristine = false;
                    }
                    let s = c.as_str();
                    rt.buffer.insert_str(rt.cursor, s);
                    rt.cursor += s.len();
                }
            }
            _ => key_consumed = false,
        }
        if key_consumed {
            self.reset_cursor_blink();
        }
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
        false
    }

    /// Step the global palette to the next built-in theme. `dir = +1` or
    /// `-1` (forward / backward). The active theme name is stored in
    /// `self.active_theme` so the settings overlay can show what's on,
    /// and so `cycle_theme` from any starting palette lands somewhere
    /// predictable.
    fn cycle_theme(&mut self, dir: i32) {
        let themes = palette::builtin_themes();
        if themes.is_empty() {
            return;
        }
        let cur = themes
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(&self.active_theme))
            .unwrap_or(0);
        let n = themes.len() as i32;
        let next = ((cur as i32 + dir).rem_euclid(n)) as usize;
        let (name, pal) = themes[next];
        self.apply_theme(name, pal);
    }

    /// Install `pal` as the active palette and record the canonical
    /// name. Fires `theme` plugin event with the new name and invokes
    /// the persistence callback (if any) so rterm-app can write the
    /// choice back to `~/.config/rterm/config.toml`.
    fn apply_theme(&mut self, name: &str, pal: palette::Palette) {
        palette::init_palette(pal);
        self.active_theme = name.to_string();
        self.events.emit("theme", name);
        if let Some(cb) = self.on_theme_change.as_ref() {
            cb(name);
        }
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }

    /// Persist the list of tabs (focused pane's cwd per tab) to
    /// `self.session_path` so `restore_session` can re-spawn them next
    /// launch. Layout (splits) is not preserved.
    fn write_session(&self) {
        let Some(path) = self.session_path.as_ref() else { return };
        let mut out = String::new();
        out.push_str("# rterm session — generated on exit.\n");
        out.push_str(&format!("active = {}\n\n", self.active_tab));
        for tab in &self.tabs {
            out.push_str("[[tab]]\n");
            // Same fallback chain as the live snapshot: OSC 7-reported cwd
            // first, then `/proc/<pid>/cwd` on Linux for shells (dash, some
            // fish setups) that don't emit OSC 7. Without this, sessions
            // restored from such shells would always start in the rterm
            // process cwd instead of where the user actually was.
            let focused = tab.focused_pane();
            let cwd = focused
                .and_then(|p| p.terminal.lock().ok().and_then(|t| t.cwd().map(String::from)))
                .or_else(|| focused.and_then(|p| pid_cwd_fallback(p.io.process_id())));
            if let Some(c) = cwd {
                out.push_str(&format!("cwd = \"{}\"\n", toml_escape_basic_string(&c)));
            }
            if let Some(title) = tab.custom_title.as_ref() {
                if !title.is_empty() {
                    out.push_str(&format!("title = \"{}\"\n", toml_escape_basic_string(title)));
                }
            }
            out.push('\n');
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match write_user_private(path, out.as_bytes()) {
            Ok(()) => tracing::info!(path = %path.display(), "session saved"),
            Err(e) => tracing::warn!("session save failed: {e}"),
        }
    }

    /// Dump the focused pane's scrollback + live grid to a file under
    /// `$XDG_CACHE_HOME/rterm/` (or `~/.cache/rterm/`).
    fn save_scrollback(&self) {
        let Some(pane) = self.focused_pane() else { return };
        let Ok(term) = pane.terminal.lock() else { return };
        // Same WIDE_SPACER skip as `selection_text` / snapshot grid_text —
        // the spacer's `ch` is a literal ' ' and saving it would inject a
        // space between every double-width glyph (CJK / emoji).
        let mut buf = String::new();
        for i in 0..term.scrollback_len() {
            if let Some(row) = term.scrollback_line(i) {
                for c in row {
                    if c.attrs.contains(CellAttrs::WIDE_SPACER) {
                        continue;
                    }
                    buf.push(c.ch);
                }
                buf.push('\n');
            }
        }
        let rows = term.size().rows;
        for r in 0..rows {
            if let Some(row) = term.grid().row(r) {
                for c in row {
                    if c.attrs.contains(CellAttrs::WIDE_SPACER) {
                        continue;
                    }
                    buf.push(c.ch);
                }
                buf.push('\n');
            }
        }
        drop(term);
        let cleaned: String = buf
            .lines()
            .map(|l| l.trim_end_matches(' '))
            .collect::<Vec<_>>()
            .join("\n");

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let dir = std::env::var_os("XDG_CACHE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| {
                    let mut p = std::path::PathBuf::from(h);
                    p.push(".cache");
                    p
                })
            });

        if let Some(mut path) = dir {
            path.push("rterm");
            if let Err(e) = std::fs::create_dir_all(&path) {
                tracing::warn!("scrollback dir mkdir failed: {e}");
                return;
            }
            path.push(format!("scrollback-{}.txt", timestamp));
            match write_user_private(&path, cleaned.as_bytes()) {
                Ok(()) => {
                    tracing::info!(path = %path.display(), "scrollback saved");
                    self.events.emit("scrollback.save", &path.display().to_string());
                }
                Err(e) => tracing::warn!("scrollback save failed: {e}"),
            }
        }
    }

    fn start_search(&mut self) {
        let Some(tab) = self.active_tab() else { return };
        let pane_idx = tab.focused_index().unwrap_or(0);
        self.search = Some(SearchState {
            pane_idx,
            query: String::new(),
            regex_mode: false,
            regex_error: false,
            matches: Vec::new(),
            current: 0,
        });
        // The search prompt reserves a strip at the bottom of the
        // window, so `outer_rect` just shrunk by one row. Reflow the
        // PTY grid to match — otherwise the shell would think it has
        // more rows than the renderer is displaying.
        self.sync_terminal_size();
        self.events.emit("search.start", "");
    }

    fn end_search(&mut self) {
        if self.search.is_some() {
            self.search = None;
            // Reset blink phase so the cursor is visible the moment the
            // user types again — they just exited search and want feedback.
            self.reset_cursor_blink();
            // Pane area just grew back by one row (the search bar
            // unreserved its space). Re-sync the PTY grid so the
            // shell starts using the freed bottom row instead of
            // leaving it blank below the cursor.
            self.sync_terminal_size();
            self.events.emit("search.end", "");
        }
    }

    /// Re-scan the focused pane's logical lines (scrollback + grid) for the
    /// current query. Substring (case-insensitive) by default; case-sensitive
    /// regex when `state.regex_mode`. Jumps to the last match on success.
    fn refresh_matches(&mut self) {
        let Some(state) = self.search.as_mut() else { return };
        state.matches.clear();
        state.current = 0;
        state.regex_error = false;
        if state.query.is_empty() {
            return;
        }
        let pane = match self.tabs.get(self.active_tab).and_then(|t| t.pane_at(state.pane_idx)) {
            Some(p) => p,
            None => return,
        };

        // Compile the regex up-front (only in regex mode). Smart-case:
        // queries containing no uppercase letter are matched case-insensitively;
        // any uppercase letter forces case-sensitive matching. Substring mode
        // applies the same rule below.
        let regex = if state.regex_mode {
            let pattern = if state.query.chars().any(|c| c.is_uppercase()) {
                state.query.clone()
            } else {
                format!("(?i){}", state.query)
            };
            match regex::Regex::new(&pattern) {
                Ok(r) => Some(r),
                Err(_) => {
                    state.regex_error = true;
                    return;
                }
            }
        } else {
            None
        };
        // Substring mode mirrors regex smart-case: any uppercase character
        // in the query → case-sensitive; otherwise compare after lowercasing.
        let case_sensitive = state.query.chars().any(|c| c.is_uppercase());
        let q_chars: Vec<char> = if case_sensitive {
            state.query.chars().collect()
        } else {
            state.query.to_lowercase().chars().collect()
        };
        let Ok(term) = pane.terminal.lock() else { return };
        // On alt screen the renderer pins the viewport to the alt grid
        // (iter 263), so primary scrollback is unreachable — pretend it's
        // empty so search only finds matches the user can actually scroll
        // to within the current TUI app's buffer.
        let sb_len = if term.is_on_alt_screen() { 0 } else { term.scrollback_len() };
        let total_lines = sb_len + term.size().rows as usize;
        for line_idx in 0..total_lines {
            let row_cells: &[rterm_core::Cell] = if line_idx < sb_len {
                match term.scrollback_line(line_idx) {
                    Some(r) => r,
                    None => continue,
                }
            } else {
                let g_row = (line_idx - sb_len) as u16;
                match term.grid().row(g_row) {
                    Some(r) => r,
                    None => continue,
                }
            };
            if let Some(re) = &regex {
                // Build row as a String so regex can scan it; we map byte
                // offsets back to column indices by counting chars.
                let row_str: String = row_cells.iter().map(|c| c.ch).collect();
                for m in re.find_iter(&row_str) {
                    if m.range().is_empty() {
                        continue;
                    }
                    let start_col = row_str[..m.start()].chars().count();
                    let end_col = row_str[..m.end()].chars().count();
                    state.matches.push((line_idx, start_col as u16, end_col as u16));
                }
            } else {
                let mut start = 0usize;
                while start + q_chars.len() <= row_cells.len() {
                    let m = (0..q_chars.len()).all(|i| {
                        let cell_ch = row_cells[start + i].ch;
                        if case_sensitive {
                            cell_ch == q_chars[i]
                        } else {
                            cell_ch.to_lowercase().eq(q_chars[i].to_lowercase())
                        }
                    });
                    if m {
                        state
                            .matches
                            .push((line_idx, start as u16, (start + q_chars.len()) as u16));
                        start += q_chars.len();
                    } else {
                        start += 1;
                    }
                }
            }
        }
        drop(term);

        if !state.matches.is_empty() {
            state.current = state.matches.len() - 1;
        }
        self.jump_to_current_match();
    }

    fn jump_to_current_match(&self) {
        let Some(state) = self.search.as_ref() else { return };
        if state.matches.is_empty() {
            return;
        }
        let (line_idx, _, _) = state.matches[state.current];
        let pane = match self.tabs.get(self.active_tab).and_then(|t| t.pane_at(state.pane_idx)) {
            Some(p) => p,
            None => return,
        };
        let Ok(term) = pane.terminal.lock() else { return };
        let sb_len = term.scrollback_len();
        let rows = term.size().rows as usize;
        // Pick offset so the match's logical line sits about a third from the
        // top of the visible region.
        let target_r = (rows / 3) as i64;
        // logical_index = sb_len - offset + r  →  offset = sb_len - L + r
        let mut offset = (sb_len as i64) - (line_idx as i64) + target_r;
        offset = offset.clamp(0, sb_len as i64);
        drop(term);
        pane.scroll_offset.store(clamp_scroll_offset(offset), Ordering::Relaxed);
    }

    fn search_step(&mut self, delta: isize) {
        let (current, total) = {
            let Some(state) = self.search.as_mut() else { return };
            if state.matches.is_empty() {
                return;
            }
            let n = state.matches.len() as isize;
            state.current = (((state.current as isize + delta) % n + n) % n) as usize;
            (state.current + 1, state.matches.len())
        };
        self.jump_to_current_match();
        self.events
            .emit("search.step", &format!("{current}/{total}"));
    }

    /// Map the current match into a viewport `NormSelection` for highlighting.
    fn current_match_selection(&self) -> Option<(usize, NormSelection)> {
        let state = self.search.as_ref()?;
        if state.matches.is_empty() {
            return None;
        }
        let (line_idx, start, end) = state.matches[state.current];
        let pane = self.tabs.get(self.active_tab)?.pane_at(state.pane_idx)?;
        let offset = pane.scroll_offset.load(Ordering::Relaxed);
        let term = pane.terminal.lock().ok()?;
        let sb_len = term.scrollback_len();
        // visible row r ↔ logical line (sb_len - offset + r). Solve for r.
        let r_signed = line_idx as i64 - sb_len as i64 + offset as i64;
        let rows = term.size().rows as i64;
        if r_signed < 0 || r_signed >= rows {
            return None;
        }
        let row = r_signed as u16;
        Some((
            state.pane_idx,
            NormSelection {
                start: SelectionPoint { row, col: start },
                end: SelectionPoint { row, col: end },
                block: false,
            },
        ))
    }

    fn handle_scroll(&mut self, delta: MouseScrollDelta) {
        // Translate the wheel delta into integral lines first so we
        // can decide what to scroll. Apply a 3× multiplier — the raw
        // wheel notch was uncomfortably slow on most mice; tuned by
        // feel against Chrome / VSCode.
        const WHEEL_SPEED_MULT: f32 = 3.0;
        let lines_f = match delta {
            MouseScrollDelta::LineDelta(_, y) => y * WHEEL_SPEED_MULT,
            MouseScrollDelta::PixelDelta(p) => {
                let line_h = self
                    .state
                    .as_ref()
                    .map(|s| s.text.line_height())
                    .unwrap_or(16.0)
                    .max(1.0);
                (p.y as f32) / line_h * WHEEL_SPEED_MULT
            }
        };
        let step = lines_f.round() as i32;
        if step == 0 {
            return;
        }
        // Modal overlays consume the wheel before anything else so
        // it doesn't bleed through to the panes / tab strip below.
        // Convention (matches key handlers): wheel UP (step > 0)
        // moves the viewport up in help, selects the previous palette
        // item, etc. — i.e. the same direction as Arrow-Up.
        if self.show_help {
            if step > 0 {
                self.help_scroll = self.help_scroll.saturating_sub(step as usize);
            } else {
                self.help_scroll =
                    self.help_scroll.saturating_add((-step) as usize);
            }
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
            return;
        }
        if self.palette.is_some() {
            // `palette_step` uses positive delta = move DOWN the list,
            // matching Arrow-Down. Wheel up (step > 0) is the opposite,
            // so flip the sign.
            self.palette_step(-step as isize);
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
            return;
        }
        if self.show_settings || self.rename_tab.is_some() {
            // Settings is a static overlay (no scroll); rename is a
            // single line. Consume the wheel anyway so it doesn't
            // accidentally scroll the pane sitting underneath the modal.
            return;
        }
        if let Some(menu) = self.context_menu.as_mut() {
            // Move the hover cursor through the menu items.
            let n = menu.items.len() as isize;
            if n > 0 {
                let cur = menu.hovered.map(|h| h as isize).unwrap_or(-1);
                let delta_i = if step > 0 { -1 } else { 1 };
                let next = ((cur + delta_i).rem_euclid(n)) as usize;
                menu.hovered = Some(next);
                if let Some(state) = self.state.as_ref() {
                    state.window.request_redraw();
                }
            }
            return;
        }
        // Wheel over the tab bar scrolls the tab strip horizontally
        // (VSCode convention). Wheel up = scroll LEFT (towards first
        // tab), down = scroll RIGHT (towards last tab). Shift+wheel
        // keeps the older Firefox/Chrome behaviour of switching tabs.
        // Falls through to pane-scroll when outside the header.
        if let Some(rect) = self.header_rect() {
            let y = self.cursor_pos.y as f32;
            if y >= rect.top && y < rect.top + rect.height {
                if self.modifiers.contains(ModifiersState::SHIFT) {
                    let dir = -step.signum() as isize;
                    self.switch_tab(dir);
                } else {
                    // One "step" of wheel = roughly one tab's worth
                    // of horizontal scroll. Use cell_w × 8 cells per
                    // step (an average tab is ~10-14 cells with
                    // padding); 8 keeps a single notch from sliding
                    // past a short tab while still feeling
                    // responsive on long tab strips. Down = scroll
                    // forward (deeper into the list); up = back.
                    let cell_w = self
                        .state
                        .as_ref()
                        .map(|s| s.text.cell_width() as f64)
                        .unwrap_or(8.0);
                    let delta_px = -(step as f64) * cell_w * 8.0;
                    if self.scroll_tab_strip(delta_px) {
                        if let Some(state) = self.state.as_ref() {
                            state.window.request_redraw();
                        }
                    }
                }
                return;
            }
        }
        // Scroll the pane UNDER THE CURSOR, not the focused one — that's
        // what every other terminal/editor does and it keeps wheel
        // mouse-reporting coordinates inside the pane the cursor is
        // actually over (otherwise `pixel_to_cell` clamps an
        // out-of-rect position to edge cells and sends bogus coords to
        // the TUI). Falls back to the focused pane when the cursor is
        // over a gap / chrome rather than any pane.
        let target_idx = self
            .pane_at(self.cursor_pos.x, self.cursor_pos.y)
            .or_else(|| self.active_tab().and_then(|t| t.focused_index()))
            .unwrap_or(0);
        let Some(pane) = self.active_tab().and_then(|t| t.pane_at(target_idx)) else {
            return;
        };
        // If the shell wants mouse events, forward wheel as button 64 / 65.
        if let Some((_mode, sgr)) = mouse_mode_for(pane) {
            let p = self
                .pixel_to_cell(target_idx, self.cursor_pos.x, self.cursor_pos.y)
                .unwrap_or(SelectionPoint { row: 0, col: 0 });
            let button = if step > 0 { 64 } else { 65 };
            for _ in 0..step.unsigned_abs() {
                let bytes = encode_mouse(sgr, button, p.col, p.row, true);
                pane.send_input(&bytes);
            }
            return;
        }
        // Else: scroll the local scrollback view. No-op when on alt screen
        // since `visible_row` pins the viewport to the alt grid there —
        // updating `scroll_offset` would just be dead state.
        let (max_offset, on_alt) = if let Ok(term) = pane.terminal.lock() {
            (term.scrollback_len() as i32, term.is_on_alt_screen())
        } else {
            (0, false)
        };
        if on_alt {
            return;
        }
        let cur = pane.scroll_offset.load(Ordering::Relaxed) as i32;
        let next = (cur + step).clamp(0, max_offset);
        pane.scroll_offset.store(clamp_scroll_offset(next as i64), Ordering::Relaxed);
        self.events.emit("scroll", &next.to_string());
    }

    fn paste_clipboard(&mut self) {
        let text = match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("clipboard read failed: {e}");
                return;
            }
        };
        self.write_paste(&text);
    }

    /// Read PRIMARY selection (X11 / Wayland) and feed it to the focused
    /// pane. On non-Linux falls back to the regular clipboard so the
    /// middle-click gesture still does something sensible.
    fn paste_primary(&mut self) {
        #[cfg(target_os = "linux")]
        let text = {
            use arboard::{GetExtLinux, LinuxClipboardKind};
            arboard::Clipboard::new()
                .ok()
                .and_then(|mut cb| cb.get().clipboard(LinuxClipboardKind::Primary).text().ok())
        };
        #[cfg(not(target_os = "linux"))]
        let text = arboard::Clipboard::new()
            .ok()
            .and_then(|mut cb| cb.get_text().ok());
        if let Some(t) = text {
            self.write_paste(&t);
        }
    }

    /// Common paste path — line-ending normalisation + optional bracketed
    /// paste wrap, then writes to the focused pane's PTY.
    fn write_paste(&mut self, text: &str) {
        let Some(pane) = self.focused_pane() else { return };
        let pane_uid = pane.uid;
        // Multi-line paste safety prompt. The modal blocks the
        // actual PTY write until the user resolves it via Paste /
        // Edit / Cancel; if armed, return immediately — the modal's
        // own dispatch will replay the (possibly edited) text
        // through `commit_paste_now` later.
        if paste_confirm::should_confirm(text, &self.paste_confirm_cfg) {
            self.paste_confirmation = Some(
                paste_confirm::PasteConfirmation::new_confirm(text.to_string(), pane_uid),
            );
            // Force a redraw so the modal appears even if the
            // continuous-redraw chain was idle.
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
            return;
        }
        // No modal: synchronous paste to the currently focused pane —
        // focus can't change between here and the write, so `None`
        // (focused) is correct.
        self.commit_paste_now(text, None);
    }

    /// Send `text` to the focused pane's PTY without going through
    /// the multi-line safety prompt. Resolves bracketed-paste-aware
    /// line-ending normalisation + strips embedded markers. Used by
    /// both the unconfirmed paste path and the modal's "Paste" /
    /// "Apply" actions.
    /// Commit a paste to a pane. `target_uid = Some(uid)` sends to that
    /// specific pane (the confirmation modal records the uid of the
    /// pane it was opened for, so a focus change while the modal was up
    /// — pane death + refocus, a plugin `focus_pane`, etc. — can't
    /// redirect the paste into the wrong shell). `None` uses the
    /// currently focused pane (direct, non-modal paste). A target uid
    /// that no longer resolves drops the paste.
    fn commit_paste_now(&mut self, text: &str, target_uid: Option<u64>) {
        let pane = match target_uid {
            Some(uid) => match self.find_pane_by_uid(uid) {
                Some(p) => p,
                None => {
                    tracing::warn!(uid, "paste target pane is gone — dropping paste");
                    return;
                }
            },
            None => match self.focused_pane() {
                Some(p) => p,
                None => return,
            },
        };
        // Strip embedded bracketed-paste markers first so a malicious
        // clipboard payload can't include a literal `\x1b[201~` to "close"
        // our paste early and have the following bytes interpreted as
        // shell commands. We deliberately keep `\n` as-is when wrapping in
        // bracketed-paste (the shell's bracketed-paste handler treats
        // newlines literally and adds them to the input buffer rather than
        // executing). Outside bracketed paste, collapse newlines into a
        // single `\r` so multi-line clipboard contents look like one
        // command rather than a script the shell auto-executes.
        let stripped: String = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
        // Drop empty pastes silently — emits no PTY bytes, no `paste`
        // event, no spurious bracketed-paste markers. Plugins listening
        // for `paste` get a real payload or nothing.
        if stripped.is_empty() {
            return;
        }
        let bracketed = pane
            .terminal
            .lock()
            .map(|t| t.bracketed_paste())
            .unwrap_or(false);
        let to_send: String = if bracketed {
            // Bracketed paste: keep newlines as `\n`. Shells (zsh/bash/fish)
            // collect the whole payload as a single input "burst" — so
            // pressing Enter after the paste runs it, not as part of it.
            stripped.replace("\r\n", "\n")
        } else {
            // Plain paste: legacy xterm behaviour collapses to `\r`.
            stripped.replace("\r\n", "\r").replace('\n', "\r")
        };
        if bracketed {
            let mut out = Vec::with_capacity(to_send.len() + 12);
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(to_send.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
            pane.send_input(&out);
        } else {
            pane.send_input(to_send.as_bytes());
        }
        // Ensure the next frame renders the shell's echo even if the
        // continuous-redraw chain was stalled between events — without
        // this, the paste's cursor advance can be visible while the
        // freshly-printed cells stay off-screen until another input event
        // re-pumps the loop. Symptom users reported: "two-finger paste,
        // cursor moves but text doesn't appear; pressing space reveals it."
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
        self.events.emit("paste", &to_send);
    }

    fn forward_key_to_pty(&self, event: &KeyEvent) {
        let Some(pane) = self.focused_pane() else { return };
        // Any keystroke jumps the view back to the live grid.
        pane.scroll_offset.store(0, Ordering::Relaxed);
        // Note: the App-level handler clears `self.selection` and resets the
        // cursor blink phase after this.
        let ctrl = self.modifiers.contains(ModifiersState::CONTROL);
        let alt = self.modifiers.contains(ModifiersState::ALT);
        let app_cursor = pane
            .terminal
            .lock()
            .map(|t| t.app_cursor_keys())
            .unwrap_or(false);

        if let Key::Named(named) = &event.logical_key {
            if let Some(bytes) = named_key_bytes(*named, self.modifiers, app_cursor) {
                self.events.emit("key", &format!("{:?}", named));
                pane.send_input(&bytes);
                return;
            }
        }
        let text = event.text.as_ref().map(|s| s.as_str()).unwrap_or("");
        if !text.is_empty() {
            self.events.emit("key", text);
            if ctrl && text.len() == 1 {
                let b = text.as_bytes()[0];
                if let Some(m) = ctrl_byte(b) {
                    if alt {
                        pane.send_input(&[0x1b, m]);
                    } else {
                        pane.send_input(&[m]);
                    }
                    return;
                }
            }
            if alt {
                let mut out = Vec::with_capacity(text.len() + 1);
                out.push(0x1b);
                out.extend_from_slice(text.as_bytes());
                pane.send_input(&out);
            } else {
                pane.send_input(text.as_bytes());
            }
        }
    }

    /// Top-level keyboard entry. Returns `true` if the window should exit.
    fn handle_key(&mut self, event: &KeyEvent) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        // Paste-confirmation modal owns the keyboard. It either
        // resolves the paste (Paste / Apply / Cancel) and clears
        // itself, or stays open absorbing every keystroke.
        if self.paste_confirmation.is_some() {
            self.handle_paste_confirmation_key(event);
            return false;
        }
        // Suggestion popup gets first dibs on ↓ / ↑ / TAB / Esc /
        // Enter so a popup-driven nav doesn't accidentally also
        // walk the cursor in the shell. Returns `true` when the
        // popup handled the key (no further forwarding); falls
        // through otherwise. GATED on no modal overlay being up: the
        // popup has the LOWEST render precedence (any overlay hides
        // it), so without this guard an invisible popup would steal
        // Esc / Tab from the visible palette / search / help / menu.
        let modal_overlay_up = self.rename_tab.is_some()
            || self.context_menu.is_some()
            || self.palette.is_some()
            || self.show_help
            || self.show_settings
            || self.search.is_some();
        if !modal_overlay_up
            && self.suggestion_popup.is_some()
            && self.handle_suggestion_popup_key(event)
        {
            return false;
        }
        // Rename tab overlay owns the keyboard while editing.
        if self.rename_tab.is_some() {
            return self.handle_rename_key(event);
        }
        // Context menu absorbs all keys until closed.
        if self.context_menu.is_some() {
            return self.handle_context_menu_key(event);
        }
        // Command palette hijacks the keyboard.
        if self.palette.is_some() {
            return self.handle_palette_key(event);
        }
        // Help overlay swallows almost all input — toggle, Esc, and
        // scroll keys (↑/↓/PgUp/PgDn/Home/End) to move through the
        // keybinding cheat-sheet when it overflows.
        if self.show_help {
            if matches!(&event.logical_key, Key::Named(NamedKey::Escape)) {
                self.show_help = false;
                self.help_scroll = 0;
                self.reset_cursor_blink();
                return false;
            }
            match &event.logical_key {
                Key::Named(NamedKey::ArrowDown) => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                    return false;
                }
                Key::Named(NamedKey::ArrowUp) => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                    return false;
                }
                Key::Named(NamedKey::PageDown) => {
                    let page = self.help_visible_lines();
                    self.help_scroll = self.help_scroll.saturating_add(page);
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                    return false;
                }
                Key::Named(NamedKey::PageUp) => {
                    let page = self.help_visible_lines();
                    self.help_scroll = self.help_scroll.saturating_sub(page);
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                    return false;
                }
                Key::Named(NamedKey::Home) => {
                    self.help_scroll = 0;
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                    return false;
                }
                Key::Named(NamedKey::End) => {
                    // Clamp happens in `help_spans`; an absurdly large
                    // value here is fine.
                    self.help_scroll = usize::MAX / 2;
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                    return false;
                }
                _ => {}
            }
            // Allow Ctrl+Shift+H to toggle off too.
            if self.handle_app_shortcut(event).is_some() {
                return false;
            }
            return false;
        }
        // Settings overlay also hijacks the keyboard. Single-letter keys
        // adjust live settings; Esc / Ctrl+Shift+, exit. See
        // `handle_settings_key` for the active bindings.
        if self.show_settings {
            return self.handle_settings_key(event);
        }
        // Search mode hijacks the keyboard.
        if self.search.is_some() {
            self.handle_search_key(event);
            return false;
        }
        // User-defined bindings take precedence over built-ins.
        if let Some(exit) = self.check_user_bindings(event) {
            return exit;
        }
        if let Some(exit) = self.handle_app_shortcut(event) {
            return exit;
        }
        if self.handle_scroll_key(event) {
            return false;
        }
        // Bare modifier keypresses (Ctrl, Shift, Alt, Super pressed by
        // themselves) must NOT clear the on-screen selection — otherwise
        // the very act of pressing `Ctrl+Shift+C` to copy nukes the
        // selection mid-chord. Modifiers also can't be forwarded as PTY
        // bytes on their own, so just return.
        if is_bare_modifier_key(&event.logical_key) {
            return false;
        }
        // Any forwarded key clears the on-screen selection and resets the
        // cursor blink so the cursor is visible right after typing.
        self.selection = None;
        self.reset_cursor_blink();
        self.forward_key_to_pty(event);
        false
    }

    /// Returns `true` if the action selected via the palette wants the
    /// window to exit (e.g. closing the last pane).
    fn handle_palette_key(&mut self, event: &KeyEvent) -> bool {
        // Readline-style word/line edits on the query.
        if self.modifiers.contains(ModifiersState::CONTROL) {
            if let Key::Character(c) = &event.logical_key {
                match c.as_str().to_ascii_lowercase().as_str() {
                    "w" => {
                        if let Some(p) = self.palette.as_mut() {
                            // `word_back_delete_index` returns a char-
                            // boundary-safe byte index. The previous
                            // inline `rfind(..).map(|i| i + 1)` copy
                            // panicked in `truncate` on multi-byte
                            // whitespace (NBSP / U+3000) — the same bug
                            // the search overlay already fixed.
                            let drop_from = word_back_delete_index(&p.query);
                            p.query.truncate(drop_from);
                        }
                        self.refresh_palette();
                        return false;
                    }
                    "u" => {
                        if let Some(p) = self.palette.as_mut() {
                            p.query.clear();
                        }
                        self.refresh_palette();
                        return false;
                    }
                    _ => {}
                }
            }
        }
        if let Key::Named(named) = &event.logical_key {
            match named {
                NamedKey::Escape => {
                    self.close_palette();
                    return false;
                }
                NamedKey::Enter => {
                    return self.execute_palette_selection();
                }
                NamedKey::Backspace => {
                    if let Some(p) = self.palette.as_mut() {
                        p.query.pop();
                    }
                    self.refresh_palette();
                    return false;
                }
                NamedKey::ArrowDown => {
                    self.palette_step(1);
                    return false;
                }
                NamedKey::ArrowUp => {
                    self.palette_step(-1);
                    return false;
                }
                NamedKey::PageDown => {
                    let page = self.palette_visible_rows().max(1) as isize;
                    self.palette_step(page);
                    return false;
                }
                NamedKey::PageUp => {
                    let page = self.palette_visible_rows().max(1) as isize;
                    self.palette_step(-page);
                    return false;
                }
                NamedKey::Home => {
                    if let Some(p) = self.palette.as_mut() {
                        p.selected = 0;
                        p.scroll_offset = 0;
                    }
                    return false;
                }
                NamedKey::End => {
                    let visible = self.palette_visible_rows();
                    if let Some(p) = self.palette.as_mut() {
                        if !p.filtered.is_empty() {
                            p.selected = p.filtered.len() - 1;
                            p.scroll_offset = p.filtered.len().saturating_sub(visible);
                        }
                    }
                    return false;
                }
                _ => {}
            }
        }
        if let Some(text) = event.text.as_ref() {
            if let Some(c) = text.chars().next() {
                if !c.is_control() {
                    if let Some(p) = self.palette.as_mut() {
                        p.query.push_str(text);
                    }
                    self.refresh_palette();
                }
            }
        }
        false
    }


    fn handle_search_key(&mut self, event: &KeyEvent) {
        // Ctrl+R toggles regex mode (re-runs the query). Ctrl+W deletes the
        // previous word from the query (readline-style).
        if self.modifiers.contains(ModifiersState::CONTROL) {
            if let Key::Character(c) = &event.logical_key {
                match c.as_str().to_ascii_lowercase().as_str() {
                    "r" => {
                        if let Some(s) = self.search.as_mut() {
                            s.regex_mode = !s.regex_mode;
                        }
                        self.refresh_matches();
                        return;
                    }
                    "w" => {
                        if let Some(s) = self.search.as_mut() {
                            s.query.truncate(word_back_delete_index(&s.query));
                        }
                        self.refresh_matches();
                        return;
                    }
                    "u" => {
                        // Ctrl+U: clear the entire query (also readline-style).
                        if let Some(s) = self.search.as_mut() {
                            s.query.clear();
                        }
                        self.refresh_matches();
                        return;
                    }
                    _ => {}
                }
            }
        }
        if let Key::Named(named) = &event.logical_key {
            match named {
                NamedKey::Escape => {
                    self.end_search();
                    return;
                }
                NamedKey::Enter => {
                    let delta = if self.modifiers.contains(ModifiersState::SHIFT) {
                        -1
                    } else {
                        1
                    };
                    self.search_step(delta);
                    return;
                }
                NamedKey::Backspace => {
                    if let Some(s) = self.search.as_mut() {
                        s.query.pop();
                    }
                    self.refresh_matches();
                    return;
                }
                NamedKey::ArrowDown => {
                    self.search_step(1);
                    return;
                }
                NamedKey::ArrowUp => {
                    self.search_step(-1);
                    return;
                }
                _ => {}
            }
        }
        if let Some(text) = event.text.as_ref() {
            if let Some(c) = text.chars().next() {
                if !c.is_control() {
                    if let Some(s) = self.search.as_mut() {
                        s.query.push_str(text);
                    }
                    self.refresh_matches();
                }
            }
        }
    }
}

/// Xterm "modifyOtherKeys" / xterm-extended modifier code:
/// 1=none, 2=Shift, 3=Alt, 4=Shift+Alt, 5=Ctrl, 6=Ctrl+Shift, 7=Ctrl+Alt, 8=All.
/// Compute the (anchor, focus) pair for a Word- or Line-mode drag in
/// progress. `pivot` is the original word/line range (inclusive ends)
/// captured on the initial multi-click; `drag_start` / `drag_end_incl`
/// are the inclusive ends of the word/line under the drag point.
///
/// When `row_only` is true (Line mode) the forward/backward direction is
/// decided purely by row — so dragging within the same row never flips
/// the selection. Otherwise (Word mode) the full (row, col) tuple
/// decides direction.
fn snap_drag_to_range(
    pivot: (AbsPoint, AbsPoint),
    drag_start: AbsPoint,
    drag_end_incl: AbsPoint,
    row_only: bool,
) -> (AbsPoint, AbsPoint) {
    let (piv_start, piv_end_incl) = pivot;
    let backward = if row_only {
        drag_start.abs_row < piv_start.abs_row
    } else {
        (drag_start.abs_row, drag_start.col) < (piv_start.abs_row, piv_start.col)
    };
    if backward {
        (piv_end_incl, drag_start)
    } else {
        (piv_start, drag_end_incl)
    }
}

/// Right-click context menu items shown over a pane. `has_selection`
/// toggles whether the "Copy" row is enabled.
fn pane_context_items(has_selection: bool) -> Vec<MenuItem> {
    vec![
        MenuItem::Action { label: "Copy", action: AppAction::CopySelection, enabled: has_selection },
        MenuItem::Action { label: "Paste", action: AppAction::PasteClipboard, enabled: true },
        MenuItem::Action { label: "Clear selection", action: AppAction::ClearSelection, enabled: has_selection },
        MenuItem::Separator,
        MenuItem::Action { label: "New tab", action: AppAction::NewTab, enabled: true },
        MenuItem::Action { label: "Split horizontal ─", action: AppAction::SplitHorizontal, enabled: true },
        MenuItem::Action { label: "Split vertical │", action: AppAction::SplitVertical, enabled: true },
        MenuItem::Action { label: "Close pane", action: AppAction::ClosePane, enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "Search", action: AppAction::StartSearch, enabled: true },
        MenuItem::Action { label: "Reset terminal", action: AppAction::ResetPane, enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "Settings…", action: AppAction::OpenSettings, enabled: true },
        MenuItem::Action { label: "Help", action: AppAction::ToggleHelp, enabled: true },
    ]
}

/// Right-click menu over a tab in the header bar.
fn tab_context_items() -> Vec<MenuItem> {
    vec![
        MenuItem::Action { label: "Rename tab…", action: AppAction::RenameTab, enabled: true },
        MenuItem::Action { label: "Close tab", action: AppAction::CloseTab, enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "Move tab left", action: AppAction::MoveTabLeft, enabled: true },
        MenuItem::Action { label: "Move tab right", action: AppAction::MoveTabRight, enabled: true },
        MenuItem::Action { label: "Zoom pane", action: AppAction::ZoomPane, enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "New tab", action: AppAction::NewTab, enabled: true },
    ]
}

/// Right-click over the header bar's empty area (not on a tab).
fn header_context_items() -> Vec<MenuItem> {
    vec![
        MenuItem::Action { label: "New tab", action: AppAction::NewTab, enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "Command palette", action: AppAction::OpenPalette, enabled: true },
        MenuItem::Action { label: "Cycle theme", action: AppAction::CycleTheme, enabled: true },
        MenuItem::Action { label: "Settings…", action: AppAction::OpenSettings, enabled: true },
        MenuItem::Action { label: "Help", action: AppAction::ToggleHelp, enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "Quit", action: AppAction::Quit, enabled: true },
    ]
}

/// Static items shown by the hamburger `≡` menu in the header bar.
fn app_menu_items() -> Vec<MenuItem> {
    vec![
        MenuItem::Action { label: "New tab", action: AppAction::NewTab, enabled: true },
        MenuItem::Action { label: "Split horizontal ─", action: AppAction::SplitHorizontal, enabled: true },
        MenuItem::Action { label: "Split vertical │", action: AppAction::SplitVertical, enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "Command palette", action: AppAction::OpenPalette, enabled: true },
        MenuItem::Action { label: "Search", action: AppAction::StartSearch, enabled: true },
        MenuItem::Action { label: "Save scrollback", action: AppAction::SaveScrollback, enabled: true },
        MenuItem::Action { label: "Clear scrollback", action: AppAction::ClearScrollback, enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "Cycle theme", action: AppAction::CycleTheme, enabled: true },
        MenuItem::Action { label: "Settings…", action: AppAction::OpenSettings, enabled: true },
        MenuItem::Action { label: "Help", action: AppAction::ToggleHelp, enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "Snap window: left ◧",   action: AppAction::SnapWindowLeft,   enabled: true },
        MenuItem::Action { label: "Snap window: right ◨",  action: AppAction::SnapWindowRight,  enabled: true },
        MenuItem::Action { label: "Snap window: top ⬒",    action: AppAction::SnapWindowTop,    enabled: true },
        MenuItem::Action { label: "Snap window: bottom ⬓", action: AppAction::SnapWindowBottom, enabled: true },
        MenuItem::Action { label: "Toggle maximize",         action: AppAction::MaximizeToggle,   enabled: true },
        MenuItem::Action { label: "Minimize",                action: AppAction::MinimizeWindow,   enabled: true },
        MenuItem::Action { label: "Restore size",            action: AppAction::RestoreWindow,    enabled: true },
        MenuItem::Separator,
        MenuItem::Action { label: "Quit", action: AppAction::Quit, enabled: true },
    ]
}

/// True for the OS window-switcher chord (Alt+Tab / Alt+Shift+Tab).
/// rterm swallows it so the meta-Tab (`ESC` + `\t`) never reaches the
/// PTY, where a shell or TUI would read it as forward/back-tab and flip
/// its own tab. The compositor still performs the real window switch.
/// `Ctrl` is excluded so a deliberate Ctrl+Alt+Tab isn't captured here.
fn is_window_switch_chord(key: &Key, alt: bool, ctrl: bool) -> bool {
    alt && !ctrl && matches!(key, Key::Named(NamedKey::Tab))
}

/// True when `key` is a "bare modifier" — a logical key that, pressed
/// on its own, only modifies subsequent keystrokes (Ctrl, Shift, Alt,
/// Super, AltGraph, Fn, lock keys). Such presses must not destroy the
/// on-screen selection: the user is mid-chord toward `Ctrl+Shift+C`.
fn is_bare_modifier_key(key: &Key) -> bool {
    matches!(
        key,
        Key::Named(
            NamedKey::Control
                | NamedKey::Shift
                | NamedKey::Alt
                | NamedKey::Super
                | NamedKey::Meta
                | NamedKey::Hyper
                | NamedKey::AltGraph
                | NamedKey::CapsLock
                | NamedKey::NumLock
                | NamedKey::ScrollLock
                | NamedKey::Fn
                | NamedKey::FnLock
                | NamedKey::Symbol
                | NamedKey::SymbolLock
        )
    )
}

fn xterm_mod_code(mods: ModifiersState) -> u8 {
    let mut m = 0u8;
    if mods.contains(ModifiersState::SHIFT) {
        m |= 1;
    }
    if mods.contains(ModifiersState::ALT) {
        m |= 2;
    }
    if mods.contains(ModifiersState::CONTROL) {
        m |= 4;
    }
    if m == 0 {
        1
    } else {
        m + 1
    }
}

fn direction_letter(k: NamedKey) -> Option<char> {
    Some(match k {
        NamedKey::ArrowUp => 'A',
        NamedKey::ArrowDown => 'B',
        NamedKey::ArrowRight => 'C',
        NamedKey::ArrowLeft => 'D',
        NamedKey::Home => 'H',
        NamedKey::End => 'F',
        _ => return None,
    })
}

fn tilde_code(k: NamedKey) -> Option<u8> {
    Some(match k {
        NamedKey::PageUp => 5,
        NamedKey::PageDown => 6,
        NamedKey::Insert => 2,
        NamedKey::Delete => 3,
        NamedKey::F5 => 15,
        NamedKey::F6 => 17,
        NamedKey::F7 => 18,
        NamedKey::F8 => 19,
        NamedKey::F9 => 20,
        NamedKey::F10 => 21,
        NamedKey::F11 => 23,
        NamedKey::F12 => 24,
        // Xterm tilde codes for F13–F20. The 27 / 30 gaps in the
        // numbering are deliberate — they're reserved for legacy
        // DEC bindings (`KP_PF{1..4}` etc.) that we don't emit.
        NamedKey::F13 => 25,
        NamedKey::F14 => 26,
        NamedKey::F15 => 28,
        NamedKey::F16 => 29,
        NamedKey::F17 => 31,
        NamedKey::F18 => 32,
        NamedKey::F19 => 33,
        NamedKey::F20 => 34,
        _ => return None,
    })
}

fn f1_f4_letter(k: NamedKey) -> Option<char> {
    Some(match k {
        NamedKey::F1 => 'P',
        NamedKey::F2 => 'Q',
        NamedKey::F3 => 'R',
        NamedKey::F4 => 'S',
        _ => return None,
    })
}

fn named_key_bytes(k: NamedKey, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    let mod_code = xterm_mod_code(mods);
    let modified = mod_code > 1;

    if let Some(c) = direction_letter(k) {
        return Some(if modified {
            format!("\x1b[1;{}{}", mod_code, c).into_bytes()
        } else if app_cursor {
            format!("\x1bO{}", c).into_bytes()
        } else {
            format!("\x1b[{}", c).into_bytes()
        });
    }
    if let Some(n) = tilde_code(k) {
        return Some(if modified {
            format!("\x1b[{};{}~", n, mod_code).into_bytes()
        } else {
            format!("\x1b[{}~", n).into_bytes()
        });
    }
    if let Some(c) = f1_f4_letter(k) {
        return Some(if modified {
            format!("\x1b[1;{}{}", mod_code, c).into_bytes()
        } else {
            format!("\x1bO{}", c).into_bytes()
        });
    }
    // Shift+Tab → CBT (cursor back tab, `ESC [ Z`). Apps like `bash`
    // and `readline` reverse-cycle completion menus with this. Bare
    // Tab keeps sending `\t` so forward completion still works.
    if matches!(k, NamedKey::Tab) && mods.contains(ModifiersState::SHIFT) {
        return Some(b"\x1b[Z".to_vec());
    }
    // Plain keys with optional alt-prefix.
    let bytes: &'static [u8] = match k {
        NamedKey::Enter => b"\r",
        NamedKey::Backspace => b"\x7f",
        NamedKey::Tab => b"\t",
        NamedKey::Escape => b"\x1b",
        _ => return None,
    };
    Some(if mods.contains(ModifiersState::ALT) {
        let mut v = vec![0x1b];
        v.extend_from_slice(bytes);
        v
    } else {
        bytes.to_vec()
    })
}

/// Encode a mouse event as an escape sequence. `button` follows xterm:
/// 0=L, 1=M, 2=R, 64/65=wheel up/down. `press=false` is a release.
fn encode_mouse(sgr: bool, button: u32, col: u16, row: u16, press: bool) -> Vec<u8> {
    if sgr {
        let suffix = if press { 'M' } else { 'm' };
        format!("\x1b[<{};{};{}{}", button, col + 1, row + 1, suffix).into_bytes()
    } else {
        // Legacy X10. Release encodes as button 3.
        let b = if press { (button & 0xff) as u8 + 32 } else { 3 + 32 };
        let x = ((col + 1).saturating_add(32)).min(255) as u8;
        let y = ((row + 1).saturating_add(32)).min(255) as u8;
        vec![0x1b, b'[', b'M', b, x, y]
    }
}

/// Read mouse mode of a pane's terminal. Returns Some((mode, sgr)) when on.
fn mouse_mode_for(pane: &Pane) -> Option<(MouseTracking, bool)> {
    let t = pane.terminal.lock().ok()?;
    let m = t.mouse_tracking();
    if m == MouseTracking::Off {
        None
    } else {
        Some((m, t.sgr_mouse()))
    }
}

/// Returns the leftmost leaf path that starts with `from`. Used to pick a
/// focus target after a leaf is removed and the parent collapsed.
fn descend_leftmost(tree: &tree::Tree<Pane>, from: &[bool]) -> tree::TreePath {
    tree.leaf_paths()
        .into_iter()
        .filter(|p| p.len() >= from.len() && p[..from.len()] == *from)
        .min()
        .unwrap_or_else(|| from.to_vec())
}

/// Clear colour for a bell visual flash: a SMALL, neutral lift of all
/// three channels — a soft dim pulse, not a screen-wide flash. On a dark
/// theme a bigger lift reads as a near-white "flashbang", so keep it
/// low (and `bell_visual = false` disables it entirely). The configured
/// alpha is preserved so a transparent window doesn't snap opaque for
/// the flash. Tunable via `FLASH_LIFT`.
///
/// `intensity` (0..=1) scales the lift so the pulse can FADE OUT over a
/// few frames instead of snapping off — the hard on/off step is what
/// reads as "blinking" when a shell rings BEL on every ignored
/// backspace. Squared so the tail eases out instead of cutting.
fn flash_clear_color(base: wgpu::Color, intensity: f64) -> wgpu::Color {
    const FLASH_LIFT: f64 = 0.003;
    let lift = FLASH_LIFT * (intensity.clamp(0.0, 1.0)).powi(2);
    wgpu::Color {
        r: (base.r + lift).min(1.0),
        g: (base.g + lift).min(1.0),
        b: (base.b + lift).min(1.0),
        a: base.a,
    }
}

/// Whether a composite-alpha mode actually honours the surface alpha
/// (i.e. can produce a see-through window). `Opaque` (and anything
/// that isn't a pre/post-multiplied blend) cannot.
fn alpha_mode_is_transparent(mode: wgpu::CompositeAlphaMode) -> bool {
    matches!(
        mode,
        wgpu::CompositeAlphaMode::PreMultiplied | wgpu::CompositeAlphaMode::PostMultiplied
    )
}

/// Choose the surface composite-alpha mode. When the user asked for
/// transparency (`opacity < 1.0`) prefer a mode that honours the
/// surface alpha; if the surface offers none, fall back to its default
/// (typically `Opaque`) — the caller warns in that case. With full
/// opacity, just take the platform default.
fn pick_alpha_mode(
    opacity: f32,
    available: &[wgpu::CompositeAlphaMode],
) -> wgpu::CompositeAlphaMode {
    let default = available
        .first()
        .copied()
        .unwrap_or(wgpu::CompositeAlphaMode::Opaque);
    if opacity < 1.0 {
        available
            .iter()
            .copied()
            .find(|m| alpha_mode_is_transparent(*m))
            .unwrap_or(default)
    } else {
        default
    }
}

/// Whether a key event's PHYSICAL key is the `0` of the Ctrl+Shift+0
/// "reset font size" chord. Matched physically (not by character) so
/// it fires on layouts where Shift+0 is not `)` — German, Spanish,
/// French, etc.
fn is_font_reset_key(physical: &PhysicalKey) -> bool {
    matches!(physical, PhysicalKey::Code(KeyCode::Digit0))
}

/// Map a US-keyboard shifted-digit symbol back to its digit character.
/// Returns `None` for inputs that aren't one of the known shifted digits.
fn shifted_digit_to_digit(s: &str) -> Option<char> {
    match s {
        "!" => Some('1'),
        "@" => Some('2'),
        "#" => Some('3'),
        "$" => Some('4'),
        "%" => Some('5'),
        "^" => Some('6'),
        "&" => Some('7'),
        "*" => Some('8'),
        "(" => Some('9'),
        ")" => Some('0'),
        _ => None,
    }
}

/// Score a fuzzy match of `needle` (already lowercased) inside `haystack`
/// (already lowercased). Returns `None` if `needle`'s characters cannot be
/// found in `haystack` in order. Higher scores are better.
///
/// Tier 1 (exact substring) dominates everything: a contiguous match anywhere
/// inside the label gives 800–1000 points, biased by position. Falling back to
/// generic subsequence matching gives at most ~150 points, so substring hits
/// always rank above fuzzy ones.
fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    if let Some(idx) = haystack.find(needle) {
        // Exact substring: earlier matches and shorter labels rank higher.
        let mut s = 1000 - idx as i32;
        if idx == 0 {
            s += 100;
        }
        // Word-boundary substring: e.g. " tab" in "next tab".
        if idx > 0 {
            if let Some(prev) = haystack[..idx].chars().next_back() {
                if matches!(prev, ' ' | '_' | '-' | '/' | '.') {
                    s += 50;
                }
            }
        }
        s -= haystack.chars().count() as i32 / 4;
        return Some(s);
    }
    // Tier 2: greedy subsequence with boundary / consecutive bonuses.
    let hay: Vec<char> = haystack.chars().collect();
    let nee: Vec<char> = needle.chars().collect();
    let mut score: i32 = 0;
    let mut hi = 0usize;
    let mut prev_match: Option<usize> = None;
    for &nc in &nee {
        let pos = hi + hay[hi..].iter().position(|&c| c == nc)?;
        let at_boundary = pos > 0
            && matches!(hay.get(pos - 1), Some(&c) if c == ' ' || c == '_' || c == '-' || c == '/' || c == '.');
        if at_boundary {
            score += 20;
        }
        if let Some(prev) = prev_match {
            if pos == prev + 1 {
                score += 15;
            } else {
                score -= (pos - prev) as i32;
            }
        } else {
            score -= pos as i32 / 2;
        }
        prev_match = Some(pos);
        hi = pos + 1;
    }
    score -= (hay.len() as i32) / 4;
    Some(score)
}

fn trim_label(s: &str, max: usize) -> String {
    // Tab labels are typically shell paths like "/bin/bash" — show the last
    // path segment, trimmed to `max` display cells (not chars: a CJK glyph
    // takes 2 cells, an emoji takes 2, ASCII takes 1).
    use unicode_width::UnicodeWidthChar;
    let tail = s.rsplit('/').next().unwrap_or(s);
    let total: usize = tail.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= max {
        return tail.to_string();
    }
    // Build the truncated prefix one char at a time, accounting for
    // the trailing `…` which is itself one cell wide. Stop just before
    // we'd overflow `max - 1` cells.
    let mut out = String::with_capacity(tail.len());
    let mut width = 0usize;
    for ch in tail.chars() {
        let w = ch.width().unwrap_or(0);
        if width + w > max - 1 {
            break;
        }
        out.push(ch);
        width += w;
    }
    out.push('…');
    out
}


/// Best-effort cwd lookup for a shell that hasn't sent OSC 7. On Linux we
/// read `/proc/<pid>/cwd` (a symlink to the directory). On other platforms
/// the symlink doesn't exist, so we just return `None` and let the caller
/// stick with no cwd.
/// Sniff for WSL2 (and earlier WSL1). The kernel release string contains
/// "microsoft" (case-insensitive) on every WSL flavour Microsoft ships.
/// Used to default the wgpu backend to GL there — Vulkan via WSL's mesa
/// reliably stalls instance creation on many setups.
fn is_wsl() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn pid_cwd_fallback(pid: Option<u32>) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let pid = pid?;
        let link = format!("/proc/{}/cwd", pid);
        std::fs::read_link(link)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Outcome of an Alt+N numeric-pane-focus request.
#[derive(Debug, PartialEq, Eq)]
enum FocusIndexAction {
    /// Apply the focus change to this 0-based DFS index.
    Apply(usize),
    /// User pressed the key for the pane they're already on — no event,
    /// but still pulse the cursor so the keypress feels acknowledged.
    AlreadyThere,
}

/// Decide what `focus_pane_index` should do given the current focus
/// `prev`, the number of panes `path_count`, and the requested index
/// `idx`. Returns `None` when `idx` is out of range. Pure so the
/// branching can be unit-tested without spinning up a full `App`.
fn focus_index_decision(
    prev: usize,
    path_count: usize,
    idx: usize,
) -> Option<FocusIndexAction> {
    if idx >= path_count {
        return None;
    }
    if idx == prev {
        return Some(FocusIndexAction::AlreadyThere);
    }
    Some(FocusIndexAction::Apply(idx))
}

/// Severity ranking for the OSC 9;4 progress `state` byte. Higher
/// values dominate when aggregating across panes of one tab:
///   error (2) > warn (4) > indeterminate (3) > set (1).
/// State 0 ("clear") returns 0 — it never appears in our aggregator
/// because cleared panes hold `None` rather than `Some((0, _))`.
/// Format the payload for a `pane.split` plugin event:
/// `<tab_1based>:<pane_1based>\t<h|v>\t<uid>`. The trailing fields are
/// additive — plugins that previously parsed only `<tab>:<pane>` keep
/// working (Lua's `match("(%d+):(%d+)")` ignores the suffix); layout-
/// aware plugins can distinguish a horizontal from a vertical split,
/// and uid-aware plugins can stash the new pane's stable identifier
/// without a follow-up `list_panes()` walk.
fn pane_split_payload(
    tab_1based: usize,
    pane_1based: usize,
    dir: SplitDir,
    uid: u64,
) -> String {
    let dir_tag = match dir {
        SplitDir::Horizontal => 'h',
        SplitDir::Vertical => 'v',
    };
    format!("{}:{}\t{}\t{}", tab_1based, pane_1based, dir_tag, uid)
}

/// Format the payload for a pane-attribute change event:
/// `<tab+1>:<pane+1>\t<uid>\t<value>`. The shape is shared by
/// `pane.cursor_shape`, `pane.cursor_blink`, `pane.cursor_visible`,
/// and `pane.mouse_mode` — keeping the construction in one place
/// avoids per-site drift if the format ever needs to grow another
/// trailing field.
fn pane_attr_payload<V: std::fmt::Display>(
    tab_1based: usize,
    pane_1based: usize,
    uid: u64,
    value: V,
) -> String {
    format!("{}:{}\t{}\t{}", tab_1based, pane_1based, uid, value)
}

/// Format the payload for a pane-edge event with no extra value:
/// `<tab+1>:<pane+1>\t<uid>`. Used by `pane.alt_enter` /
/// `pane.alt_leave` and `pane.scrollback_enter` / `_leave`.
fn pane_edge_payload(tab_1based: usize, pane_1based: usize, uid: u64) -> String {
    format!("{}:{}\t{}", tab_1based, pane_1based, uid)
}

/// Format the payload for a pane-text event:
/// `<tab+1>:<pane+1>\t<text>`. Used by `pane.title` and
/// `pane.cwd`. Semantically distinct from `pane_edge_payload`
/// (which carries a uid) so legacy parsers that splice the
/// trailing field into a UI label don't accidentally inherit a
/// numeric uid when the schema looks identical at the byte
/// level.
fn pane_text_payload(tab_1based: usize, pane_1based: usize, text: &str) -> String {
    format!("{}:{}\t{}", tab_1based, pane_1based, text)
}

/// Format the payload for a tab-level event that carries the
/// tab's focused-pane uid: `<tab+1>\t<uid>`. Shared by
/// `tab.switch` and `tab.alt_enter` / `tab.alt_leave` so uid-aware
/// plugins can answer "is my watched pane in this tab?" without
/// re-resolving the tab's contents.
fn tab_event_payload(tab_1based: usize, uid: u64) -> String {
    format!("{}\t{}", tab_1based, uid)
}

/// Format the payload for `tab.title`: `<tab+1>\t<title>`. Same
/// byte shape as `tab_event_payload` but the trailing field is a
/// free-form string rather than a uid — kept separate so a future
/// schema change (e.g. uid suffix) only touches one site, and so
/// the reader sees the intent of the trailing payload.
fn tab_title_payload(tab_1based: usize, title: &str) -> String {
    format!("{}\t{}", tab_1based, title)
}

/// Format the payload for `tab.progress`:
/// `<tab+1>\t<state>\t<pct>`. State 0 with pct 0 means "cleared"
/// (the aggregate went from Some progress to None this frame).
fn tab_progress_payload(tab_1based: usize, state: u8, percent: u8) -> String {
    format!("{}\t{}\t{}", tab_1based, state, percent)
}

/// Format the payload for `tab.drag_end`:
/// `<src_tab+1>\t<moved>` where `moved` is "true" / "false".
/// Plugins read this to distinguish a canceled drag (release
/// on the same tab) from a successful reorder.
fn tab_drag_end_payload(src_1based: usize, moved: bool) -> String {
    format!("{}\t{}", src_1based, moved)
}

/// Number of rows the `scroll_half_page_*` actions move per
/// invocation, given the visible page height in rows.
/// `⌈page/2⌉` (computed as `(page/2).max(1)` for positive page),
/// so even a 1-row pane still advances one line per keypress
/// instead of getting stuck at 0. Page values of `0` are clamped
/// to `1` for the same reason — a half-page on an unknown page
/// size is still a meaningful "move at least one line".
fn half_page_rows(page: i32) -> i32 {
    if page <= 0 {
        1
    } else {
        (page / 2).max(1)
    }
}

/// Saturate a computed scrollback offset into the `u16` the per-pane
/// `scroll_offset` atomic holds. Scrollback can exceed 65 535 lines
/// (the limit is plugin-settable up to 1M), so a bare `as u16` would
/// WRAP — flinging the viewport to a wrong, far-off line and making
/// search-jump land on unrelated content. Clamping pins it at the
/// representable maximum instead. (The tail past 65 535 stays
/// unreachable until the atomic is widened — a separate, larger
/// change; this only kills the wrap bug.)
fn clamp_scroll_offset(value: i64) -> u16 {
    value.clamp(0, u16::MAX as i64) as u16
}

/// Format the payload for the OSC 9;4 `progress` event:
/// `<tab+1>:<pane+1>\t<state>\t<percent>`. State 0 means "clear"
/// (percent ignored). Kept here so a future schema extension
/// (e.g. uid suffix) lands in one place — currently the event has
/// the same prefix as `pane_edge_payload` so legacy parsers still
/// extract tab/pane.
fn progress_payload(tab_1based: usize, pane_1based: usize, state: u8, percent: u8) -> String {
    format!("{}:{}\t{}\t{}", tab_1based, pane_1based, state, percent)
}

/// Format the payload for pane events that carry a value BEFORE the
/// uid: `<tab+1>:<pane+1>\t<value>\t<uid>`. Used by `pane.bell_mute`
/// (`value` = muted bool) and `pane.shell_exit` (`value` = exit
/// code). Opposite trailing order from `pane_attr_payload` —
/// preserved for backwards compatibility with plugins that already
/// parse these emissions in this order.
fn pane_value_uid_payload<V: std::fmt::Display>(
    tab_1based: usize,
    pane_1based: usize,
    value: V,
    uid: u64,
) -> String {
    format!("{}:{}\t{}\t{}", tab_1based, pane_1based, value, uid)
}

/// Canonical cursor-shape names, indexed by the u8 code that
/// `last_cursor_shape` stores. The string at index `i` is the name
/// corresponding to code `i`. Shared between `cursor_shape_code`
/// and the snapshot builder so a renaming touches one place.
const CURSOR_SHAPE_NAMES: [&str; 3] = ["block", "underline", "bar"];

/// Canonical mouse-mode names, indexed by the u8 code that
/// `last_mouse_mode` stores. Same source-of-truth role as
/// `CURSOR_SHAPE_NAMES`.
const MOUSE_MODE_NAMES: [&str; 4] = ["off", "x10", "btn", "any"];

/// Encode a cursor-shape name into a stable u8 used by the per-pane
/// `last_cursor_shape` edge-trigger gate. Unknown values fall back
/// to `0` (block) so a bad rename in the snapshot path is silent in
/// production but still pinned by the helper's unit tests.
fn cursor_shape_code(name: &str) -> u8 {
    CURSOR_SHAPE_NAMES
        .iter()
        .position(|&n| n == name)
        .map(|i| i as u8)
        .unwrap_or(0)
}

/// Encode the mouse-tracking-mode name (matching the strings the
/// snapshot builder emits) into a stable u8 for `last_mouse_mode`.
/// Unknown -> 0 (off) so a typo in the snapshot path falls into the
/// safe "no event each frame" path instead of churning.
fn mouse_mode_code(name: &str) -> u8 {
    MOUSE_MODE_NAMES
        .iter()
        .position(|&n| n == name)
        .map(|i| i as u8)
        .unwrap_or(0)
}

/// Format the payload for a `pane.command_finish` plugin event:
/// `<tab+1>:<pane+1>\t<exit_code>\t<duration_ms_or_empty>\t<uid>`.
/// Duration is the wall-clock time between OSC 133;C and 133;D — empty
/// when we didn't observe a matching `;C` (e.g. the shell only emits
/// `;D` for some reason, or two `;D`s arrived between drains). Plugins
/// reading the duration column should treat empty string as "unknown"
/// rather than zero so a fast command isn't misattributed. The trailing
/// uid lets plugins match the event to a previously-stashed identifier.
fn pane_command_finish_payload(
    tab_1based: usize,
    pane_1based: usize,
    exit_code: i32,
    duration_ms: Option<u64>,
    uid: u64,
) -> String {
    let dur_str = duration_ms.map(|d| d.to_string()).unwrap_or_default();
    format!(
        "{}:{}\t{}\t{}\t{}",
        tab_1based, pane_1based, exit_code, dur_str, uid,
    )
}

fn pane_exit_payload(
    tab_1based: usize,
    pane_1based: usize,
    exit_code: Option<i32>,
    uid: u64,
) -> String {
    // Format: `<tab>:<pane>\t<code_or_empty>\t<uid>`. Always three
    // tab-separated fields so `split('\t')` yields a stable column
    // count; empty middle column means "no shell-integration `;D`
    // observed". Legacy `<tab>:<pane>` parsers still work since the
    // colon-separated prefix is untouched.
    let code_str = exit_code.map(|c| c.to_string()).unwrap_or_default();
    format!("{}:{}\t{}\t{}", tab_1based, pane_1based, code_str, uid)
}

fn progress_severity(state: u8) -> u8 {
    match state {
        2 => 4,
        4 => 3,
        3 => 2,
        1 => 1,
        _ => 0,
    }
}

/// Snapshot a terminal's visible grid as a plain `\n`-joined string.
/// Trailing spaces are trimmed per row and `WIDE_SPACER` cells are
/// dropped (mirroring the `selection_text` and snapshot-grid-text
/// conventions) so the output reads like what the user sees, not the
/// raw cell array. Used both for `TerminalSnapshot::grid_text` and the
/// per-pane variant exposed via `rterm.terminal_text(tab, pane)`.
/// Maximum number of scrollback lines included in the per-frame
/// snapshot pushed to plugins for the focused pane. Bounds the
/// worst-case allocation at ~SCROLLBACK_SNAPSHOT_MAX × cols bytes per
/// frame for the focused pane; plugins that need a longer tail can
/// keep a rolling buffer of their own from `pane.output` events.
const SCROLLBACK_SNAPSHOT_MAX: usize = 500;

/// Per-pane (non-focused) scrollback cap. Smaller than the focused
/// cap because every pane pays this cost on every frame —
/// `SCROLLBACK_TAIL_MAX × cols × pane_count` bytes per snapshot.
/// 100 lines × ~80 cols × ~10 panes ≈ 80 KiB per frame; trade-off
/// chosen for "give me the last build error from a background pane"
/// use cases.
const SCROLLBACK_TAIL_MAX: usize = 100;

/// Snapshot the focused pane's recent scrollback as a `\n`-joined string.
/// Returns the most-recent `SCROLLBACK_SNAPSHOT_MAX` lines so plugins can
/// screen-scrape "what just rolled off the visible grid" without
/// subscribing to `pane.output` and maintaining their own buffer. Trailing
/// spaces on each line are stripped, mirroring `grid_text_snapshot`.
/// Empty scrollback yields the empty string (no allocation past header).
fn scrollback_text_snapshot(t: &Terminal) -> String {
    scrollback_text_snapshot_capped(t, SCROLLBACK_SNAPSHOT_MAX)
}

/// Same as `scrollback_text_snapshot` but with an explicit cap — used
/// for the per-pane (background) variant with a tighter limit so the
/// total per-frame allocation stays bounded as the pane count grows.
fn scrollback_text_snapshot_capped(t: &Terminal, max_lines: usize) -> String {
    let total = t.scrollback_len();
    let start = total.saturating_sub(max_lines);
    let take = total - start;
    if take == 0 {
        return String::new();
    }
    // Average ~40 bytes per line is a reasonable starting estimate; the
    // grow-as-needed path is cheap if the actual lines are longer.
    let mut out = String::with_capacity(take * 40);
    for i in start..total {
        if let Some(row) = t.scrollback_line(i) {
            let mut line: String = row
                .iter()
                .filter(|c| !c.attrs.contains(CellAttrs::WIDE_SPACER))
                .map(|c| c.ch)
                .collect();
            let trimmed_len = line.trim_end_matches(' ').len();
            line.truncate(trimmed_len);
            out.push_str(&line);
        }
        if i + 1 < total {
            out.push('\n');
        }
    }
    out
}

fn grid_text_snapshot(t: &Terminal) -> String {
    let size = t.size();
    let mut out = String::with_capacity(size.rows as usize * size.cols as usize);
    for r in 0..size.rows {
        if let Some(row) = t.grid().row(r) {
            let mut line: String = row
                .iter()
                .filter(|c| !c.attrs.contains(CellAttrs::WIDE_SPACER))
                .map(|c| c.ch)
                .collect();
            let trimmed_len = line.trim_end_matches(' ').len();
            line.truncate(trimmed_len);
            out.push_str(&line);
        }
        if r + 1 < size.rows {
            out.push('\n');
        }
    }
    out
}

/// Read `/proc/<pid>/comm` on Linux — the executable basename of the
/// foreground process group leader. Returns `None` on non-Linux or when
/// the process has exited between snapshot frames. The result is trimmed
/// of trailing newline; the kernel caps `comm` at TASK_COMM_LEN = 16
/// bytes (15 + NUL) so allocation is bounded.
fn read_proc_comm_or_none(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{}/comm", pid);
        let raw = std::fs::read_to_string(path).ok()?;
        let trimmed = raw.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Replace the user's home dir prefix with `~` so the status bar isn't
/// dominated by `/home/<user>/...`. Falls back to the original path.
/// Escape a string for embedding inside a TOML basic string literal
/// (`"..."`). Used by `write_session` to serialize cwd/title fields whose
/// raw bytes could otherwise include literal control characters (Unix
/// allows `\n` and friends inside filenames). The mapping is the minimal
/// set TOML requires; this isn't a general-purpose escaper.
fn toml_escape_basic_string(s: &str) -> String {
    s.replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Write `contents` to `path` with restrictive permissions where the
/// platform supports them (`0o600` on Unix). rterm writes two kinds of
/// files that may contain sensitive data — saved scrollback (can carry
/// shell history, env dumps, accidentally-pasted secrets) and the
/// session restore file (carries each tab's cwd). Default umask on most
/// Linux distributions yields world-readable files; on a shared host
/// any other local user could `cat ~/.cache/rterm/scrollback-*.txt`.
///
/// On non-Unix the file is created via plain `fs::write` — Windows
/// inherits the parent directory's ACL, which is typically user-only
/// inside `%LOCALAPPDATA%` anyway.
fn write_user_private(
    path: &std::path::Path,
    contents: &[u8],
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents)?;
        f.flush()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

fn abbreviate_home(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Some(home_s) = home.to_str() {
            if let Some(rest) = path.strip_prefix(home_s) {
                if rest.is_empty() {
                    return "~".to_string();
                }
                if rest.starts_with('/') {
                    return format!("~{}", rest);
                }
            }
        }
    }
    path.to_string()
}

fn ctrl_byte(b: u8) -> Option<u8> {
    match b {
        b'@'..=b'_' => Some(b & 0x1f),
        b'a'..=b'z' => Some(b - b'a' + 1),
        b'?' => Some(0x7f),
        _ => None,
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let mut attrs = WindowAttributes::default()
            .with_title(&self.title)
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.initial_size.0 as f64,
                self.initial_size.1 as f64,
            ))
            // Enforce a floor on window dimensions so the user can't
            // drag the edge in and end up with a "strip" that hides
            // the tab bar (or worse, makes the surface 0×0 and crashes
            // wgpu). 720×360 logical px: comfortably fits a ~80-col
            // shell prompt plus the chrome (hamburger / tabs / + /
            // window controls), so dragging narrower doesn't wrap
            // typical command output mid-line.
            .with_min_inner_size(winit::dpi::LogicalSize::new(720.0, 360.0))
            .with_transparent(self.opacity < 1.0)
            // Hide the OS-drawn titlebar so our Chrome-style header
            // strip owns the entire top edge. Drag/resize is handled
            // client-side via `Window::drag_window` and
            // `Window::drag_resize_window` from the header / window
            // edge hit tests below. Falls back to OS decorations on
            // platforms that don't support borderless windows.
            .with_decorations(self.os_decorations);
        if let Some(icon) = self.icon.as_ref() {
            // `Icon::from_rgba` validates `rgba.len() == 4 * w * h`
            // and refuses anything else — pass through the error
            // path rather than crashing if the build pipeline ever
            // produces mismatched dimensions.
            match winit::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height) {
                Ok(winit_icon) => {
                    attrs = attrs.with_window_icon(Some(winit_icon));
                }
                Err(e) => tracing::warn!("window icon rejected: {e}"),
            }
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("window create failed: {e}");
                event_loop.exit();
                return;
            }
        };
        // Font size travels to the renderer in PHYSICAL pixels — the
        // wgpu surface is physical-sized, so shaping at the logical
        // point size on a HiDPI display would draw half-size glyphs.
        let scale = (window.scale_factor() as f32).max(0.1);
        match pollster::block_on(GpuState::new(
            window.clone(),
            self.font_size * scale,
            self.font_family.clone(),
            self.opacity,
        )) {
            Ok(state) => {
                self.state = Some(state);
                // Hand the spawner the wake handle BEFORE the first
                // pane is spawned, so every PTY reader thread (incl.
                // the initial one) can kick the idle event loop when
                // its shell produces output.
                if let Some(w) = self.waker.clone() {
                    self.spawner.set_waker(w);
                }
                if !self.session_restore.is_empty() {
                    let entries = std::mem::take(&mut self.session_restore);
                    for entry in entries {
                        self.new_tab_in(entry.cwd.as_deref());
                        if let Some(title) = entry.title {
                            if let Some(tab) = self.tabs.last_mut() {
                                tab.custom_title = if title.is_empty() {
                                    None
                                } else {
                                    Some(title.clone())
                                };
                            }
                            // Notify plugins listening for tab.title —
                            // without this, status-line plugins that
                            // re-render on the event would miss the
                            // restored title and show the auto-derived
                            // one until the next title change.
                            let tab_idx = self.tabs.len();
                            self.events.emit(
                                "tab.title",
                                &tab_title_payload(tab_idx, &title),
                            );
                        }
                    }
                    let active = self.session_active.take().unwrap_or(0);
                    if !self.tabs.is_empty() {
                        self.active_tab = active.min(self.tabs.len() - 1);
                    }
                } else {
                    self.new_tab();
                }
                if self.tabs.is_empty() {
                    tracing::error!("initial pane failed to spawn; exiting");
                    event_loop.exit();
                    return;
                }
                self.events.emit("ready", "");
                // Explicit cursor claim. Without it, Wayland surfaces
                // can show "no cursor" on first hover because the
                // compositor never received an explicit cursor surface
                // from us (it only knows the WM-default for chrome).
                // We default to the text I-beam since that's what the
                // user will mostly see over the grid; specific zones
                // (gap drag, hyperlink) override via `update_cursor_icon`.
                if let Some(state) = self.state.as_ref() {
                    state.window.set_cursor(CursorIcon::Text);
                    self.last_cursor_icon = CursorIcon::Text;
                }
                // Kick the redraw chain. On Wayland the surface is not
                // mapped (visible) until the client submits its first
                // frame — without an explicit request_redraw here, the
                // event loop sits idle on a never-mapped window.
                if let Some(state) = self.state.as_ref() {
                    tracing::info!("requesting initial redraw");
                    state.window.request_redraw();
                }
            }
            Err(e) => {
                tracing::error!("gpu init failed: {e:#}");
                event_loop.exit();
            }
        }
    }

    /// Last call before the event loop tears down. Used to give plugins a
    /// chance to clean up (write state to disk, etc.) via a `"shutdown"`
    /// event handler.
    /// Side-channel event from a background thread (see [`UserEvent`]).
    /// Dispatches in the same scope as a window event so plugin
    /// handlers fire identically to the in-app keybind path.
    /// Timer wake-ups (`ControlFlow::WaitUntil` reached) don't emit a
    /// `RedrawRequested` on their own — request one so the scheduled
    /// cursor-blink toggle / PTY-resize flush / plugin heartbeat
    /// actually paints. Cheap and idempotent; the redraw arm decides
    /// the next wake.
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if matches!(cause, winit::event::StartCause::ResumeTimeReached { .. }) {
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::GuakeGlobalHotkey => {
                // Bring the window forward before flipping the
                // drop-down state. If the hotkey fired while a
                // different app owned focus, `toggle_guake` alone
                // would reshape the window but leave keyboard input
                // going to the previously-focused window — opposite
                // of Guake's classic UX.
                if let Some(state) = self.state.as_ref() {
                    state.window.set_minimized(false);
                    state.window.focus_window();
                }
                self.toggle_guake();
            }
            UserEvent::Wake => {
                // A PTY reader produced output while the loop was idle.
                // Repaint to show it; `schedule_after_frame` then picks
                // the next wake.
                if let Some(state) = self.state.as_ref() {
                    state.window.request_redraw();
                }
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if self.save_scrollback_on_exit {
            self.save_scrollback();
        }
        if self.session_save {
            self.write_session();
        }
        self.events.emit("shutdown", "");
        // Drop the GPU state (wgpu surface + device + glyphon atlas +
        // bg pipeline + the Arc<Window>) while the winit event loop is
        // still alive. On WSL2 (Mesa llvmpipe GL + EGL + Wayland) the
        // teardown segfaults if it happens after `run_app` returns —
        // by that point winit has begun unwinding the Wayland connection
        // and Mesa's GL calls land on freed memory. Forcing the drop
        // here keeps the EGL display + Wayland surface valid through
        // every `glDelete*` and `eglDestroySurface` call.
        self.state = None;
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        // Under `ControlFlow::Wait` the loop only renders when asked.
        // Every window event other than the redraw itself may have
        // changed visible state (input, resize, focus, hover), so
        // request a repaint after handling it — the catch-all means a
        // handler that forgets its own `request_redraw` can't strand a
        // stale frame. The redraw arm schedules its OWN next wake via
        // `schedule_after_frame`, so exclude it here.
        let needs_repaint = !matches!(event, WindowEvent::RedrawRequested);
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(state) = self.state.as_mut() {
                    state.resize(size.width, size.height);
                }
                // Window grew or shrank — clamp the tab-strip scroll
                // offset in case the new strip width can already show
                // all tabs without scrolling.
                let max = self.max_tab_scroll();
                if self.tab_scroll_offset > max {
                    self.tab_scroll_offset = max;
                }
                // Update the wgpu surface size immediately so the
                // next frame renders at the right dimensions, but
                // DEFER the PTY SIGWINCH until the resize storm
                // settles — see `pending_pty_resize_at`.
                self.pending_pty_resize_at = Some(Instant::now());
                // Window-level resize event (distinct from the per-pane
                // `resize` events emitted from sync_terminal_size). Plugins
                // can use this to re-anchor overlays or recompute layouts.
                self.events
                    .emit("window.resize", &format!("{}x{}", size.width, size.height));
                // Wayland's configure → Resized event is the trigger
                // that lets us submit our first frame and have the
                // surface get mapped. Re-arm the redraw chain here so
                // a missed `RedrawRequested` event during resumed()
                // doesn't strand the window invisible.
                if let Some(state) = self.state.as_ref() {
                    state.window.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Window moved to a monitor with a different DPI (or
                // the user changed the display scale). Re-derive the
                // physical font pixel size from the logical point size
                // so glyphs stay the same visual size instead of
                // shrinking/growing with the scale change. The
                // follow-up `Resized` event handles the surface.
                tracing::info!(scale_factor, "scale factor changed — re-deriving font size");
                let logical = self.font_size;
                self.set_font_size_absolute(logical);
            }
            WindowEvent::ModifiersChanged(Modifiers { .. }) => {
                if let WindowEvent::ModifiersChanged(m) = &event {
                    self.modifiers = m.state();
                }
            }
            WindowEvent::Focused(focused) => {
                self.window_focused = focused;
                if !focused {
                    // Focus left (commonly via Alt+Tab): the OS grabs the
                    // chord, so we never see the key-release for any held
                    // modifier and the cached state would go stale — a
                    // "stuck" Alt then mangles the next keystroke (turning
                    // a plain Tab into meta-Tab, etc.). Reset to a clean
                    // slate; the next ModifiersChanged repopulates it.
                    self.modifiers = ModifiersState::empty();
                }
                if focused {
                    // Clear any taskbar urgent flag the bell may have set.
                    if let Some(s) = self.state.as_ref() {
                        s.window.request_user_attention(None);
                    }
                }
                self.events
                    .emit("window.focus", if focused { "true" } else { "false" });
                // CSI ?1004 focus tracking: notify every pane that asked.
                let seq: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
                for tab in &self.tabs {
                    for pane in tab.panes() {
                        let tracking = pane
                            .terminal
                            .lock()
                            .map(|t| t.focus_tracking())
                            .unwrap_or(false);
                        if tracking {
                            pane.send_input(seq);
                        }
                    }
                }
            }
            WindowEvent::KeyboardInput { event: key_event, .. }
                if self.handle_key(&key_event) =>
            {
                event_loop.exit();
            }
            WindowEvent::KeyboardInput { .. } => {}
            WindowEvent::MouseWheel { delta, .. } => {
                // Paste-confirmation modal grabs wheel events while
                // up. In edit mode the wheel scrolls the cursor
                // through the buffer; in confirm mode wheel is
                // absorbed so a stray scroll doesn't accidentally
                // page the pane underneath.
                if self.paste_confirmation.is_some() {
                    self.handle_paste_confirmation_wheel(delta);
                    return;
                }
                // Ctrl+wheel = font zoom (common UX). Threshold step at 0.5
                // so high-resolution touchpad scrolls don't fire on every
                // tick; one notch ≈ one point.
                if self.modifiers.contains(ModifiersState::CONTROL) {
                    let step = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p) => (p.y as f32) / 40.0,
                    };
                    if step.abs() >= 0.5 {
                        self.adjust_font_size(step.signum());
                    }
                } else {
                    self.handle_scroll(delta);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = position;
                // Three drag flavours fire `handle_drag`: text selection
                // inside a pane (`mouse_dragging`), split-divider drag
                // (`gap_dragging`), and tab-strip live-reorder
                // (`tab_dragging`). Without the third gate the tab drag
                // payload never reached `handle_drag` and tabs only
                // shuffled on release.
                if self.mouse_dragging
                    || self.gap_dragging.is_some()
                    || self.tab_dragging.is_some()
                {
                    self.handle_drag(position.x, position.y);
                }
                if self.paste_modal_dragging {
                    self.paste_modal_drag_edit(position.x, position.y);
                }
                if self.context_menu.is_some() {
                    self.update_context_menu_hover(position.x, position.y);
                }
                self.update_cursor_icon(position.x, position.y);
                // Hover-state changes in the header (tab chips, ≡,
                // window-control buttons, resize edges) need a redraw
                // to reflect the new highlight. Cheap: bg-quad rebuild
                // is bounded by tab count + chrome.
                let over_header = self
                    .header_rect()
                    .map(|r| {
                        let yf = position.y as f32;
                        yf >= r.top && yf < r.top + r.height
                    })
                    .unwrap_or(false);
                let near_edge =
                    !self.os_decorations && self.window_edge_at(position.x, position.y).is_some();
                if over_header || near_edge {
                    if let Some(state) = self.state.as_ref() {
                        state.window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { button: MouseButton::Left, state: ElementState::Pressed, .. } => {
                let (x, y) = (self.cursor_pos.x, self.cursor_pos.y);
                if self.handle_press(x, y) {
                    event_loop.exit();
                }
            }
            WindowEvent::MouseInput { button: MouseButton::Left, state: ElementState::Released, .. } => {
                self.handle_release();
            }
            WindowEvent::MouseInput { button: MouseButton::Middle, state: ElementState::Pressed, .. } => {
                // Middle-click on a tab closes it (browser/tmux
                // convention). Anywhere else falls through to the
                // historical "paste from PRIMARY selection".
                let (x, y) = (self.cursor_pos.x, self.cursor_pos.y);
                if let Some(t) = self.tab_at(x, y) {
                    let prev_active = self.active_tab;
                    self.active_tab = t;
                    let exit = self.close_active_tab();
                    if exit {
                        event_loop.exit();
                    } else if prev_active != t && prev_active < self.tabs.len() {
                        // Restore focus to the tab the user was on,
                        // shifted down by one if it sat after the
                        // closed slot. close_active_tab leaves us on
                        // `tabs.len() - 1`, which isn't what we want
                        // when the user middle-clicked a background tab.
                        let restore = if prev_active > t {
                            prev_active - 1
                        } else {
                            prev_active
                        };
                        if restore < self.tabs.len() {
                            self.active_tab = restore;
                            self.sync_terminal_size();
                            self.update_title();
                        }
                    }
                } else {
                    self.paste_primary();
                }
            }
            // Right-click pastes from the system clipboard — matches the
            // xterm / Linux convention. Shells that enable mouse tracking
            // Right-click → open a context menu at the cursor. The menu
            // content depends on what's under the click (tab vs pane vs
            // header empty area). Middle-click is still PRIMARY/clip
            // paste, so users who want the old "right-click pastes"
            // behaviour can rebind via Lua or just press
            // `Ctrl+Shift+V` / middle-click.
            WindowEvent::MouseInput { button: MouseButton::Right, state: ElementState::Pressed, .. } => {
                let (x, y) = (self.cursor_pos.x, self.cursor_pos.y);
                self.open_context_menu(x, y);
            }
            WindowEvent::RedrawRequested => {
                {
                    static FIRST_REDRAW: std::sync::Once = std::sync::Once::new();
                    FIRST_REDRAW.call_once(|| {
                        tracing::info!(
                            tabs = self.tabs.len(),
                            "first RedrawRequested received",
                        );
                    });
                }
                // Clear the pending-wake flag up front: any PTY output
                // that lands DURING this frame re-arms a fresh wake
                // (rather than being coalesced into the one we're
                // already servicing and then lost).
                if let Some(w) = self.waker.as_ref() {
                    w.mark_handled();
                }
                // Re-arm / refresh / dismiss the suggestion popup
                // every frame. Cheap when nothing has changed
                // (compare-against-generation in the popup-state
                // compute), at most one SQLite query when the
                // debounce window elapses.
                self.refresh_suggestion_popup();
                // Trim the glyph atlas once a minute. glyphon's
                // atlas grows lazily up to the GPU's max texture
                // dimension (often 8192² → ~64 MiB per atlas, two
                // atlases = ~128 MiB) and never evicts on its own,
                // so a session that's cycled through several font
                // sizes / themes accumulates dead glyphs. The
                // trim removes anything that hasn't been rendered
                // since the previous trim — cheap when the working
                // set is stable, big GPU-memory win after churn.
                const ATLAS_TRIM_INTERVAL: Duration = Duration::from_secs(60);
                let now = Instant::now();
                let should_trim = match self.last_atlas_trim {
                    Some(prev) => now.duration_since(prev) >= ATLAS_TRIM_INTERVAL,
                    None => true,
                };
                if should_trim {
                    if let Some(state) = self.state.as_mut() {
                        state.text.trim_atlas();
                    }
                    self.last_atlas_trim = Some(now);
                }
                // Wayland/wgpu egg-and-chicken: the compositor only sends
                // its `configure` after the client commits a buffer, and
                // the client doesn't know its actual surface size until
                // configure arrives. Submit a guaranteed-fast clear-only
                // frame the very first time RedrawRequested fires so the
                // compositor sees something and maps the surface. We
                // DON'T return after — the full render path runs in the
                // same RedrawRequested, so the user sees real content as
                // soon as the surface is live. Without continuing, some
                // Wayland setups (WSL2 GL backend) never deliver another
                // RedrawRequested and the window stays at the seed
                // clear-color forever.
                if !self.first_frame_done {
                    if let Some(state) = self.state.as_mut() {
                        let result = state.render_clear_only();
                        match &result {
                            Ok(()) => {
                                tracing::info!("first clear-only frame presented");
                            }
                            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                                // Surface needs reconfigure before any
                                // present can succeed. Resize to the
                                // current window size and let the
                                // upcoming full-render branch retry.
                                let s = state.window.inner_size();
                                tracing::warn!(
                                    width = s.width,
                                    height = s.height,
                                    "first clear-only frame lost/outdated — reconfiguring surface",
                                );
                                state.resize(s.width, s.height);
                            }
                            Err(e) => {
                                tracing::warn!("first clear-only frame failed: {e:?}");
                            }
                        }
                        self.first_frame_done = true;
                        // `--render-test`: print result line and exit
                        // without touching the heavy render path.
                        if self.render_test_only {
                            match result {
                                Ok(()) => println!("render test: OK"),
                                Err(e) => println!("render test: FAIL: {e:?}"),
                            }
                            event_loop.exit();
                            return;
                        }
                    }
                    // Fall through to the full RedrawRequested body.
                }
                // 0) Prune panes whose shell exited.
                if self.prune_dead_panes() {
                    event_loop.exit();
                    return;
                }
                {
                    static R_PRUNE: std::sync::Once = std::sync::Once::new();
                    R_PRUNE.call_once(|| tracing::debug!("redraw: past prune"));
                }

                // 0pre) Auto-scroll while drag-selecting past pane edges.
                if self.mouse_dragging && self.drag_scroll_dir != 0 {
                    let dir = self.drag_scroll_dir;
                    if let Some(sel_pane_idx) = self.selection.as_ref().map(|s| s.pane_idx) {
                        if let Some(pane) = self
                            .tabs
                            .get(self.active_tab)
                            .and_then(|t| t.pane_at(sel_pane_idx))
                        {
                            let (max_off, last_row, last_col) = {
                                let t = pane.terminal.lock().ok();
                                match t {
                                    Some(g) => (
                                        g.scrollback_len() as i32,
                                        g.size().rows.saturating_sub(1),
                                        g.size().cols.saturating_sub(1),
                                    ),
                                    None => (0, 0, 0),
                                }
                            };
                            let cur = pane.scroll_offset.load(Ordering::Relaxed) as i32;
                            // dir=-1 means cursor above top → scroll INTO history (offset++).
                            // dir=+1 means cursor below bottom → scroll toward live (offset--).
                            let next = (cur - dir).clamp(0, max_off);
                            pane.scroll_offset.store(clamp_scroll_offset(next as i64), Ordering::Relaxed);
                            let edge_sp = if dir < 0 {
                                SelectionPoint { row: 0, col: 0 }
                            } else {
                                SelectionPoint { row: last_row, col: last_col }
                            };
                            if let Some(edge_abs) = self.abs_point(sel_pane_idx, edge_sp) {
                                if let Some(sel) = self.selection.as_mut() {
                                    sel.focus = edge_abs;
                                }
                            }
                        }
                    }
                }

                // 0a) Clear activity on the active tab (user is watching it).
                if let Some(active_tab) = self.tabs.get(self.active_tab) {
                    for pane in active_tab.panes() {
                        pane.activity.store(false, Ordering::Relaxed);
                    }
                }

                // 0b) Keep the scroll view anchored to its content: if new
                // lines pushed into scrollback while user was scrolled up,
                // bump `scroll_offset` by the same amount so they keep
                // reading the same logical line.
                for tab in &self.tabs {
                    for pane in tab.panes() {
                        let cur_sb = pane
                            .terminal
                            .lock()
                            .map(|t| t.scrollback_len())
                            .unwrap_or(0);
                        let prev_sb = pane.last_sb_len.swap(cur_sb, Ordering::Relaxed);
                        let grew = cur_sb.saturating_sub(prev_sb);
                        if grew > 0 {
                            if self.scroll_on_output {
                                pane.scroll_offset.store(0, Ordering::Relaxed);
                            } else {
                                let cur_off = pane.scroll_offset.load(Ordering::Relaxed);
                                if cur_off > 0 {
                                    let next = (cur_off as usize + grew).min(cur_sb).min(u16::MAX as usize);
                                    pane.scroll_offset.store(next as u16, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }

                {
                    static R_SBANCH: std::sync::Once = std::sync::Once::new();
                    R_SBANCH.call_once(|| tracing::debug!("redraw: past scrollback anchor"));
                }
                // 1) Drain pending OSC events for ALL panes in ALL tabs:
                //    titles get stored on each pane (so bg tab labels can
                //    update too), bells trigger flash + plugin event, and
                //    each completed shell line fires an "output.line" event.
                let mut window_title_changed = false;
                let mut bell_pane: Option<usize> = None;
                let mut output_lines: Vec<String> = Vec::new();
                let mut pane_output: Vec<(usize, usize, String)> = Vec::new();
                let mut exit_codes: Vec<i32> = Vec::new();
                let mut pane_exits: Vec<(usize, usize, rterm_core::CommandFinish)> = Vec::new();
                for (tab_idx, tab) in self.tabs.iter().enumerate() {
                    for (i, pane) in tab.panes().into_iter().enumerate() {
                        let drained = if let Ok(mut t) = pane.terminal.lock() {
                            (
                                t.take_title(),
                                t.take_bell(),
                                t.take_completed_lines(),
                                t.take_command_finishes(),
                                t.take_osc_responses(),
                                t.take_notifications(),
                                t.take_pending_palette_changes(),
                                t.take_progress(),
                            )
                        } else {
                            (
                                None,
                                false,
                                Vec::new(),
                                Vec::new(),
                                Vec::new(),
                                Vec::new(),
                                Vec::new(),
                                Vec::new(),
                            )
                        };
                        let (
                            took_title,
                            took_bell,
                            took_lines,
                            took_exits,
                            took_osc,
                            took_notifs,
                            took_palette,
                            took_progress,
                        ) = drained;
                        if !took_palette.is_empty() {
                            // Fold OSC 4 / 10 / 11 SET updates into the
                            // live global palette in one swap.
                            let mut cur = palette::palette();
                            for upd in took_palette {
                                use rterm_core::PaletteUpdate::*;
                                match upd {
                                    Named(idx, rgb) if (idx as usize) < 16 => {
                                        cur.named[idx as usize] = rgb;
                                    }
                                    Named(_, _) => {}
                                    DefaultFg(rgb) => cur.default_fg = rgb,
                                    DefaultBg(rgb) => cur.default_bg = rgb,
                                }
                            }
                            palette::init_palette(cur);
                            self.events.emit("theme", "osc");
                        }
                        for msg in took_notifs {
                            self.events.emit("notification", &msg);
                            // Ping the taskbar if the window is in the
                            // background — same pattern as bell.
                            if !self.window_focused {
                                if let Some(s) = self.state.as_ref() {
                                    s.window.request_user_attention(Some(
                                        winit::window::UserAttentionType::Informational,
                                    ));
                                }
                            }
                        }
                        // OSC 9;4 progress reports. Payload format:
                        // `<tab+1>:<pane+1>\t<state>\t<percent>`. State 0
                        // means "clear" (percent ignored). Useful for
                        // plugins driving a task-bar progress indicator.
                        for (state, pct) in took_progress {
                            let payload = progress_payload(tab_idx + 1, i + 1, state, pct);
                            self.events.emit("progress", &payload);
                            // Sticky per-pane progress so `list_panes()`
                            // can render a "this build is at 30%" badge
                            // without each plugin tracking the event
                            // stream. State 0 ("clear") wipes the slot.
                            if let Ok(mut slot) = pane.progress.lock() {
                                *slot = if state == 0 { None } else { Some((state, pct)) };
                            }
                        }
                        // OSC responses go straight back to the PTY of the
                        // pane that asked the question.
                        for resp in took_osc {
                            pane.send_input(resp.as_bytes());
                        }
                        if let Some(title) = took_title {
                            let payload = pane_text_payload(tab_idx + 1, i + 1, &title);
                            if let Ok(mut dyn_t) = pane.dynamic_title.lock() {
                                *dyn_t = Some(title);
                            }
                            self.events.emit("pane.title", &payload);
                            if tab_idx == self.active_tab && Some(i) == tab.focused_index() {
                                window_title_changed = true;
                            }
                        }
                        if took_bell && !pane.bell_muted.load(Ordering::Relaxed) {
                            // Fire a plugin event for every bell, even on
                            // background tabs — "ping me when this bg
                            // build finishes" workflows depend on it.
                            // Payload: `<tab+1>:<pane+1>\t<uid>` —
                            // additive uid suffix lets plugins tracking
                            // a specific pane match without re-resolving
                            // the index pair. Legacy `(%d+):(%d+)`
                            // parsers still extract tab/pane from the
                            // colon prefix. Per-pane mute gates both
                            // the event and the visual flash; the parser
                            // still consumed the BEL byte so terminal
                            // state stays intact.
                            self.events.emit(
                                "bell",
                                &format!(
                                    "{}:{}\t{}",
                                    tab_idx + 1,
                                    i + 1,
                                    pane.uid,
                                ),
                            );
                            // Visual flash only applies to the active
                            // tab — background tabs use the activity dot.
                            if tab_idx == self.active_tab {
                                bell_pane = Some(i);
                            }
                        }
                        for line in took_lines {
                            pane_output.push((tab_idx, i, line.clone()));
                            output_lines.push(line);
                        }
                        for finish in took_exits {
                            pane_exits.push((tab_idx, i, finish));
                            exit_codes.push(finish.exit_code);
                        }
                    }
                }
                for line in output_lines {
                    self.events.emit("output.line", &line);
                }
                // Tab-level activity: each non-focused tab that drained at
                // least one line emits once per frame, so plugins can add an
                // unread indicator without doing per-line bookkeeping.
                let mut active_tabs: std::collections::BTreeSet<usize> =
                    std::collections::BTreeSet::new();
                let mut output_tabs: std::collections::BTreeSet<usize> =
                    std::collections::BTreeSet::new();
                for (tab_idx, _, _) in &pane_output {
                    output_tabs.insert(*tab_idx);
                    if *tab_idx != self.active_tab {
                        active_tabs.insert(*tab_idx);
                    }
                }
                // Arm the silence gate on *every* tab that produced output
                // (including the focused one) so the `tab.silence` event
                // fires after long-running commands finish in any tab.
                for tab_idx in &output_tabs {
                    if let Some(tab) = self.tabs.get_mut(*tab_idx) {
                        tab.silence_armed = true;
                    }
                }
                for tab_idx in active_tabs {
                    // Edge-trigger `tab.unread`: fires when an
                    // unfocused tab transitions from "no pending
                    // output" to "has pending output". `tab.activity`
                    // fires on every output frame and is noisy;
                    // `tab.unread` lets status-line plugins increment
                    // a badge counter exactly once per "needs your
                    // attention" episode.
                    let was_unread = self
                        .tabs
                        .get(tab_idx)
                        .map(|t| t.unread)
                        .unwrap_or(false);
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        tab.unread = true;
                    }
                    if !was_unread {
                        self.events.emit("tab.unread", &(tab_idx + 1).to_string());
                    }
                    self.events.emit("tab.activity", &(tab_idx + 1).to_string());
                }
                // Pane-scoped variants: `<tab+1>:<pane+1>\t<line>` for
                // `pane.output`, and per-rule `match` events with payload
                // `<tab+1>:<pane+1>\t<rule>\t<line>\t<g1>\t<g2>…` (capture
                // groups present only for regex rules; substring rules
                // emit just the line). Plugins that don't need the pane
                // scope can ignore the prefix; plugins that do can split
                // on the first `\t` to extract it.
                for (tab_idx, pane_idx, line) in pane_output {
                    // Arm the per-pane silence gate for this pane. The
                    // pane-level event fires once when the pane goes idle
                    // again, even if sibling panes in the same tab are
                    // still active (so the tab as a whole stays loud).
                    if let Some(tab) = self.tabs.get(tab_idx) {
                        if let Some(pane) = tab.pane_at(pane_idx) {
                            pane.silence_armed.store(true, Ordering::Relaxed);
                        }
                    }
                    let scope = format!("{}:{}", tab_idx + 1, pane_idx + 1);
                    // `<tab>:<pane>\t<line>` — same shape as `pane.title`
                    // / `pane.cwd`; reuse the helper so the format
                    // stays in lockstep.
                    let payload = pane_text_payload(tab_idx + 1, pane_idx + 1, &line);
                    self.events.emit("pane.output", &payload);
                    for (name, groups) in self.events.match_output_line(&line) {
                        let mut payload = format!("{}\t{}\t{}", scope, name, line);
                        for g in groups {
                            payload.push('\t');
                            payload.push_str(&g);
                        }
                        self.events.emit("match", &payload);
                    }
                }
                for code in &exit_codes {
                    self.events.emit("shell.exit", &code.to_string());
                }
                // Pane-scoped variant: `<tab+1>:<pane+1>\t<code>`. Lets
                // plugins attribute an exit to the originating pane, which
                // the flat `shell.exit` event can't disambiguate when
                // multiple commands finish in the same frame. Distinct
                // from `pane.exit` (which fires on PTY EOF / pane death),
                // since this is a shell *command* completing while the
                // pane is still alive.
                for (tab_idx, pane_idx, finish) in pane_exits {
                    let uid = self
                        .tabs
                        .get(tab_idx)
                        .and_then(|t| t.pane_at(pane_idx))
                        .map(|p| p.uid)
                        .unwrap_or(0);
                    let payload = pane_value_uid_payload(
                        tab_idx + 1, pane_idx + 1, finish.exit_code, uid,
                    );
                    self.events.emit("pane.shell_exit", &payload);
                    // Richer event: include duration when known so
                    // plugins can highlight slow commands. Payload:
                    // `<tab>:<pane>\t<code>\t<ms or empty>\t<uid>`.
                    let cf_uid = self
                        .tabs
                        .get(tab_idx)
                        .and_then(|t| t.pane_at(pane_idx))
                        .map(|p| p.uid)
                        .unwrap_or(0);
                    let rich = pane_command_finish_payload(
                        tab_idx + 1,
                        pane_idx + 1,
                        finish.exit_code,
                        finish.duration_ms,
                        cf_uid,
                    );
                    self.events.emit("pane.command_finish", &rich);
                    // Slow-command threshold: fire a dedicated event and
                    // ping the taskbar (when unfocused) so "long build
                    // finished" workflows don't have to special-case the
                    // pane.command_finish stream. `0` disables.
                    if let Some(d) = finish.duration_ms {
                        if self.slow_command_ms > 0 && d >= self.slow_command_ms {
                            self.events.emit("pane.slow_command", &rich);
                            if !self.window_focused {
                                if let Some(s) = self.state.as_ref() {
                                    s.window.request_user_attention(Some(
                                        winit::window::UserAttentionType::Informational,
                                    ));
                                }
                            }
                        }
                    }
                    // Sticky per-pane last-exit so plugins can query
                    // "did pane N's last command fail?" without tracking
                    // the event stream themselves.
                    if let Some(pane) = self
                        .tabs
                        .get(tab_idx)
                        .and_then(|t| t.pane_at(pane_idx))
                    {
                        if let Ok(mut last) = pane.last_exit_code.lock() {
                            *last = Some(finish.exit_code);
                        }
                    }
                }
                if let Some(&last) = exit_codes.last() {
                    self.last_exit_code = Some(last);
                }

                // `rterm.send_input(...)` migrated to the PluginCmd
                // channel (SendInput variant) — handled in the main
                // command-match block above.

                // Plugin-addressed input by stable uid. Walk live panes
                // to find the matching uid and forward bytes. Unknown
                // uid silently dropped — plugin may have queued a write
                // after the target pane exited.
                for (uid, bytes) in self.events.drain_pending_routed_input_by_uid() {
                    match self.find_pane_by_uid(uid) {
                        Some(p) => p.send_input(&bytes),
                        None => tracing::debug!(
                            uid = uid,
                            "send_to_pane_by_uid: target not found"
                        ),
                    }
                }

                // Plugin-addressed input — route to the specified pane.
                for ((tab_idx, pane_idx), bytes) in
                    self.events.drain_pending_routed_input()
                {
                    if let Some(pane) = self
                        .tabs
                        .get(tab_idx)
                        .and_then(|t| t.pane_at(pane_idx))
                    {
                        pane.send_input(&bytes);
                    } else {
                        tracing::debug!(
                            tab = tab_idx,
                            pane = pane_idx,
                            "send_to_pane: target not found"
                        );
                    }
                }

                // Plugin-set tab titles. Each entry carries an optional
                // index (None → active tab); empty `name` clears. Apply
                // in order so a later same-index entry can re-clear an
                // earlier set, and any out-of-range index is silently
                // dropped (plugin sent a stale snapshot).
                let titles = self.events.drain_pending_tab_titles();
                let mut window_needs_update = false;
                for (idx_opt, name) in titles {
                    let target = idx_opt.unwrap_or(self.active_tab);
                    if let Some(tab) = self.tabs.get_mut(target) {
                        tab.custom_title =
                            if name.is_empty() { None } else { Some(name.clone()) };
                        // `<tab+1>\t<name>` — empty name = cleared.
                        self.events
                            .emit("tab.title", &tab_title_payload(target + 1, &name));
                        if target == self.active_tab {
                            window_needs_update = true;
                        }
                    }
                }
                if window_needs_update {
                    self.update_title();
                }

                // Plugin-set window title — single Option<Option<>>; outer
                // Some means "update requested", inner Some(name)/None
                // means "set" / "clear override".
                if let Some(override_opt) = self.events.take_pending_window_title() {
                    self.custom_window_title = override_opt;
                    self.update_title();
                }

                // Plugin-set pane titles. Each entry routes to its specific
                // (tab, pane); an empty string clears that pane's dynamic
                // title so the static fallback (the shell program name) is
                // shown again.
                let mut pane_title_changed_focused = false;
                // Plugin-set titles addressed by stable pane uid. Resolve
                // each uid to its current (tab, pane) and forward into
                // the index-based queue below — keeps the dynamic-title
                // write + `pane.title` emission in one place.
                let mut uid_resolved: Vec<(usize, usize, String)> = Vec::new();
                for (uid, title) in self.events.drain_pending_pane_titles_by_uid() {
                    // A uid that no longer points at a live pane is
                    // silently dropped — mirrors `set_pane_title(tab,
                    // pane, …)` for a closed pane.
                    if let Some((ti, pi)) = self.find_pane_indices_by_uid(uid) {
                        uid_resolved.push((ti, pi, title));
                    }
                }
                for (tab_idx, pane_idx, title) in
                    uid_resolved
                        .into_iter()
                        .chain(self.events.drain_pending_pane_titles())
                {
                    if let Some(pane) = self
                        .tabs
                        .get(tab_idx)
                        .and_then(|t| t.pane_at(pane_idx))
                    {
                        let payload = pane_text_payload(tab_idx + 1, pane_idx + 1, &title);
                        if let Ok(mut dyn_t) = pane.dynamic_title.lock() {
                            *dyn_t = if title.is_empty() { None } else { Some(title) };
                        }
                        self.events.emit("pane.title", &payload);
                        if tab_idx == self.active_tab
                            && self.tabs[tab_idx].focused_index() == Some(pane_idx)
                        {
                            pane_title_changed_focused = true;
                        }
                    }
                }
                if pane_title_changed_focused {
                    self.update_title();
                }

                // Plugin-set scrollback limit applies to every pane on every
                // tab so the change is uniform across the session.
                if let Some(limit) = self.events.take_pending_scrollback_limit() {
                    for tab in &self.tabs {
                        for pane in tab.panes() {
                            let new_len = if let Ok(mut t) = pane.terminal.lock() {
                                t.set_scrollback_limit(limit);
                                t.scrollback_len()
                            } else {
                                continue;
                            };
                            // The trim may have dropped scrollback lines that
                            // our view was anchored to; pull the offset back
                            // to within the new range.
                            let cur = pane.scroll_offset.load(Ordering::Relaxed) as usize;
                            if cur > new_len {
                                pane.scroll_offset
                                    .store(new_len.min(u16::MAX as usize) as u16, Ordering::Relaxed);
                            }
                        }
                    }
                }

                // Hot-reloaded silence threshold (config watcher publishes it
                // on every TOML reload, plugins via `rterm.set_tab_silence_ms`).
                if let Some(new_ms) = self.events.take_pending_tab_silence_ms() {
                    self.tab_silence_ms = new_ms;
                }
                // Same hot-reload surface for the three `[terminal]`
                // boolean toggles. Picking up a config change without
                // restart matches what `palette` and `scrollback` do.
                if let Some(v) = self.events.take_pending_cursor_blink() {
                    self.cursor_blink = v;
                    if v {
                        // Re-anchor so the first blink phase after the
                        // toggle starts from "on" — avoids a half-cycle
                        // of darkness right after the user enables it.
                        self.reset_cursor_blink();
                    }
                }
                if let Some(v) = self.events.take_pending_show_scrollbar() {
                    self.show_scrollbar = v;
                }
                if let Some(v) = self.events.take_pending_scroll_on_output() {
                    self.scroll_on_output = v;
                }
                if let Some(v) = self.events.take_pending_bell_visual() {
                    self.bell_visual = v;
                }
                if let Some(v) = self.events.take_pending_bell_urgent() {
                    self.bell_urgent = v;
                }
                if let Some(new_family) = self.events.take_pending_font_family() {
                    // No-op check is inside set_font_family — avoids
                    // the expensive cell-width re-probe + buffer
                    // resync on a settling reload tick that didn't
                    // actually change the family name. Also forces
                    // a redraw so the new glyphs land on screen
                    // next frame without waiting for unrelated state.
                    if let Some(state) = self.state.as_mut() {
                        state.text.set_font_family(new_family);
                        state.window.request_redraw();
                    }
                    self.sync_terminal_size();
                }
                if let Some(new_guake) = self.events.take_pending_guake() {
                    // `None` = feature disabled. If we left the window
                    // in AlwaysOnTop while previously dropped, restore
                    // Normal level + clear the dropped flag so the
                    // next press doesn't try to "hide" a window the
                    // user has explicitly visible. The guard runs
                    // regardless of the new state — if guake was
                    // re-enabled with the window still dropped, the
                    // next `toggle_guake` correctly toggles to hide.
                    if new_guake.is_none() {
                        if let Some(state) = self.state.as_ref() {
                            if self.guake_dropped {
                                state.window.set_window_level(
                                    winit::window::WindowLevel::Normal,
                                );
                            }
                        }
                        self.guake_dropped = false;
                    }
                    self.guake = new_guake;
                    tracing::info!(
                        enabled = self.guake.as_ref().is_some_and(|g| g.enabled),
                        "guake config hot-reloaded",
                    );
                }
                for (tab_idx, pane_idx, muted) in
                    self.events.drain_pending_pane_bell_mute()
                {
                    if let Some(pane) =
                        self.tabs.get(tab_idx).and_then(|t| t.pane_at(pane_idx))
                    {
                        // Mirror the `toggle_bell_mute` action: fire
                        // `pane.bell_mute` only when the value actually
                        // flips so a plugin re-asserting the same state
                        // each frame doesn't spam handlers. Same
                        // `<tab+1>:<pane+1>\t<bool>\t<uid>` payload so
                        // status plugins don't have to special-case the
                        // source.
                        let prev = pane.bell_muted.swap(muted, Ordering::Relaxed);
                        if prev != muted {
                            self.events.emit(
                                "pane.bell_mute",
                                &pane_value_uid_payload(
                                    tab_idx + 1, pane_idx + 1, muted, pane.uid,
                                ),
                            );
                        }
                    }
                }
                // Uid-addressed toggles: resolve uid → (tab, pane)
                // by walking the tab tree, then apply via the same
                // edge-fire-on-change path. Entries for vanished
                // panes are silently dropped — matches how
                // `set_pane_title_by_uid` handles stale ids.
                for (uid, muted) in
                    self.events.drain_pending_pane_bell_mute_by_uid()
                {
                    if let Some((tab_idx, pane_idx)) = self.find_pane_indices_by_uid(uid) {
                        if let Some(pane) =
                            self.tabs.get(tab_idx).and_then(|t| t.pane_at(pane_idx))
                        {
                            let prev = pane.bell_muted.swap(muted, Ordering::Relaxed);
                            if prev != muted {
                                self.events.emit(
                                    "pane.bell_mute",
                                    &pane_value_uid_payload(
                                        tab_idx + 1, pane_idx + 1, muted, pane.uid,
                                    ),
                                );
                            }
                        }
                    }
                }
                if let Some(v) = self.events.take_pending_slow_command_ms() {
                    self.slow_command_ms = v;
                }

                // Plugin-triggered taskbar ping (`rterm.attention()`).
                if self.events.take_pending_attention() && !self.window_focused {
                    if let Some(s) = self.state.as_ref() {
                        s.window.request_user_attention(Some(
                            winit::window::UserAttentionType::Informational,
                        ));
                    }
                }

                // Unified plugin → app/renderer command bus. Drain
                // once per frame; match per variant. New variants
                // land here as their legacy `drain_pending_X` queues
                // get folded into the channel.
                // Unified plugin → app/renderer command bus. Drain
                // once per frame and match per variant. As more
                // legacy `drain_pending_X` queues migrate, the arm
                // list grows. Order across variants is preserved by
                // the channel — a plugin firing
                // `RunAction("split")` immediately followed by
                // `Notify("done")` sees them dispatched in order.
                let mut plugin_exit = false;
                for cmd in self.events.drain_pending_commands() {
                    match cmd {
                        rterm_core::PluginCmd::Notify(msg) => {
                            // Same path OSC 9 takes: fire the
                            // `notification` event and ping the
                            // taskbar when the window is unfocused.
                            self.events.emit("notification", &msg);
                            if !self.window_focused {
                                if let Some(s) = self.state.as_ref() {
                                    s.window.request_user_attention(Some(
                                        winit::window::UserAttentionType::Informational,
                                    ));
                                }
                            }
                        }
                        rterm_core::PluginCmd::RunAction(name) => {
                            if let Some(act) = AppAction::from_name(&name) {
                                if self.dispatch_action(act) {
                                    plugin_exit = true;
                                }
                            } else {
                                tracing::debug!(action = %name, "run_action: unknown");
                            }
                        }
                        rterm_core::PluginCmd::OpenUrl(url) => {
                            // Same scheme whitelist as mouse / keyboard
                            // hover-open — plugins are user-authored and
                            // trusted, but the URL inside
                            // `rterm.open_url(...)` might be assembled
                            // from shell output, so we keep the trust
                            // boundary at the system handler.
                            if !rterm_core::is_safe_url(&url) {
                                tracing::warn!(
                                    url = %url,
                                    "blocked plugin open_url with disallowed scheme",
                                );
                                self.events.emit("link.blocked", &url);
                                continue;
                            }
                            match open::that_detached(&url) {
                                Ok(_) => self.events.emit("link.open", &url),
                                Err(e) => {
                                    tracing::warn!(url = %url, "open_url failed: {e}")
                                }
                            }
                        }
                        rterm_core::PluginCmd::KillPaneByUid(uid) => {
                            // Walk live panes for the matching uid;
                            // flip `alive` so the prune pass collapses
                            // it next frame. Unknown uid is silently
                            // dropped (pane may have already exited).
                            'outer: for tab in &self.tabs {
                                for pane in tab.panes() {
                                    if pane.uid == uid {
                                        pane.alive.store(false, Ordering::Release);
                                        break 'outer;
                                    }
                                }
                            }
                        }
                        rterm_core::PluginCmd::KillTab(tab_idx) => {
                            if let Some(tab) = self.tabs.get(tab_idx) {
                                for pane in tab.panes() {
                                    pane.alive.store(false, Ordering::Release);
                                }
                            }
                        }
                        rterm_core::PluginCmd::KillPane(tab_idx, pane_idx) => {
                            if let Some(pane) = self
                                .tabs
                                .get(tab_idx)
                                .and_then(|t| t.pane_at(pane_idx))
                            {
                                pane.alive.store(false, Ordering::Release);
                            }
                        }
                        rterm_core::PluginCmd::Paste(bytes) => {
                            // Wrap in bracketed-paste markers when the
                            // destination shell asked for them (DECSET
                            // ?2004); otherwise forward raw. Mirrors
                            // the keyboard-Paste path's handling.
                            if let Some(pane) = self.focused_pane() {
                                let bracketed = pane
                                    .terminal
                                    .lock()
                                    .ok()
                                    .map(|t| t.bracketed_paste())
                                    .unwrap_or(false);
                                if bracketed {
                                    pane.send_input(b"\x1b[200~");
                                    pane.send_input(&bytes);
                                    pane.send_input(b"\x1b[201~");
                                } else {
                                    pane.send_input(&bytes);
                                }
                                self.events.emit("paste", &String::from_utf8_lossy(&bytes));
                            }
                        }
                        rterm_core::PluginCmd::SendInput(bytes) => {
                            // Plugin-injected input as if the user
                            // typed. Goes to the focused pane's PTY.
                            if let Some(pane) = self.focused_pane() {
                                pane.send_input(&bytes);
                            }
                        }
                        rterm_core::PluginCmd::Scroll(delta) => {
                            // Positive = into history; negative = toward
                            // live. `i32::MIN` is the `scroll_to_live`
                            // sentinel — clamp catches it.
                            if let Some(pane) = self.focused_pane() {
                                let max_off = pane
                                    .terminal
                                    .lock()
                                    .ok()
                                    .map(|t| t.scrollback_len() as i32)
                                    .unwrap_or(0);
                                let cur = pane.scroll_offset.load(Ordering::Relaxed) as i32;
                                let next = cur.saturating_add(delta).clamp(0, max_off);
                                pane.scroll_offset.store(clamp_scroll_offset(next as i64), Ordering::Relaxed);
                                self.events.emit("scroll", &next.to_string());
                            }
                        }
                        rterm_core::PluginCmd::NewTab(cwd) => {
                            self.new_tab_in(cwd.as_deref());
                        }
                        rterm_core::PluginCmd::Split(dir_str, cwd) => {
                            let dir = match dir_str.as_str() {
                                "h" | "horizontal" => SplitDir::Horizontal,
                                "v" | "vertical" => SplitDir::Vertical,
                                "auto" | "smart" => self.split_auto_direction(),
                                other => {
                                    tracing::debug!(
                                        dir = %other,
                                        "rterm.split: unknown direction",
                                    );
                                    continue;
                                }
                            };
                            self.split_active_pane_in(dir, cwd.as_deref());
                        }
                        rterm_core::PluginCmd::EmitEvent(name, body) => {
                            // Re-emit the plugin event on the App's
                            // event loop so registered handlers fire
                            // identically to a native source.
                            self.events.emit(&name, &body);
                        }
                        _ => {}
                    }
                }
                if plugin_exit {
                    event_loop.exit();
                    return;
                }

                // Plugin-requested clipboard write from `rterm.copy(text)`.
                // Uses the same helper as the keyboard Copy action so the
                // Linux-specific "outlive the call so a clipboard manager
                // can see it" path applies here too.
                if let Some(text) = self.events.take_pending_copy() {
                    clipboard_set(&text);
                    self.events.emit("copy", &text);
                }

                // `rterm.emit_event` / `rterm.new_tab` migrated to the
                // PluginCmd channel (EmitEvent / NewTab variants) —
                // handled in the main command-match block above.

                // Plugin-requested live font-size change.
                if let Some(size) = self.events.take_pending_font_size() {
                    self.set_font_size_absolute(size);
                }

                // Plugin-requested live opacity change.
                if let Some(alpha) = self.events.take_pending_opacity() {
                    self.opacity = if alpha.is_finite() {
                        alpha.clamp(0.0, 1.0)
                    } else {
                        self.opacity
                    };
                    if let Some(state) = self.state.as_mut() {
                        state.set_opacity(self.opacity);
                    }
                }

                // `rterm.split` migrated to the PluginCmd channel
                // (Split variant) — handled in the main command-match
                // block above.

                // `rterm.kill_pane` / `rterm.kill_tab` migrated to the
                // PluginCmd channel (KillPane / KillTab variants) —
                // handled in the main command-match block above.

                // Plugin-supplied full palette swap from `rterm.set_palette`.
                if let Some((fg, bg, cur, named)) = self.events.take_pending_palette() {
                    let mut p = palette::palette();
                    if let Some(c) = fg { p.default_fg = c; }
                    if let Some(c) = bg { p.default_bg = c; }
                    if cur.is_some() { p.cursor = cur; }
                    if let Some(n) = named { p.named = n; }
                    palette::init_palette(p);
                    self.events.emit("theme", "plugin");
                }

                // Plugin-requested switch to a built-in theme by name
                // (`rterm.set_theme("dracula")`). Looks up the palette in
                // the shipped table; unknown names are silently ignored
                // (the Lua side already returned `false` to the caller).
                if let Some(name) = self.events.take_pending_theme() {
                    if let Some((canon, pal)) = palette::theme_by_name(&name) {
                        self.apply_theme(canon, pal);
                    }
                }

                // Plugin-driven bell — visual flash + (when unfocused) a
                // taskbar attention ping, identical to a terminal BEL.
                // Each side is gated by its own `terminal.bell_*` toggle
                // so the user can keep one channel and silence the other.
                // Emit the same `<tab+1>:<pane+1>` payload as the VT BEL
                // path so plugin handlers don't have to special-case the
                // legacy "plugin" literal.
                if self.events.take_pending_bell() {
                    if self.bell_visual {
                        self.flash_until =
                            Some(Instant::now() + Duration::from_millis(BELL_FLASH_FADE_MS));
                    }
                    // Same shape as the VT BEL emission above:
                    // `<tab>:<pane>\t<uid>`. `0:0\t0` sentinel when no
                    // pane has focus (transient between tab creates).
                    let payload = self
                        .active_tab()
                        .and_then(|t| {
                            t.focused_index().and_then(|p| {
                                t.pane_at(p).map(|pane| (self.active_tab, p, pane.uid))
                            })
                        })
                        .map(|(t, p, uid)| format!("{}:{}\t{}", t + 1, p + 1, uid))
                        .unwrap_or_else(|| "0:0\t0".to_string());
                    self.events.emit("bell", &payload);
                    if self.bell_urgent && !self.window_focused {
                        if let Some(s) = self.state.as_ref() {
                            s.window.request_user_attention(Some(
                                winit::window::UserAttentionType::Informational,
                            ));
                        }
                    }
                }

                // Plugin-paste — route through `write_paste` so the same
                // `rterm.paste(text)` and `rterm.scroll(delta)`
                // migrated to the PluginCmd channel — see the
                // Paste / Scroll match arms in the main command
                // loop above. The renderer-side write_paste helper
                // is now invoked directly through the channel arm.

                // Absolute scroll from `rterm.scroll_to_line(line)`.
                // `scroll_offset = sb_len - line`, clamped. Anchors the
                // requested logical line at the top of the viewport.
                if let Some(line) = self.events.take_pending_scroll_to_line() {
                    if let Some(pane) = self.focused_pane() {
                        let sb_len = pane
                            .terminal
                            .lock()
                            .ok()
                            .map(|t| t.scrollback_len())
                            .unwrap_or(0);
                        let new_off =
                            sb_len.saturating_sub(line).min(u16::MAX as usize) as u16;
                        pane.scroll_offset.store(new_off, Ordering::Relaxed);
                        self.events.emit("scroll", &new_off.to_string());
                    }
                }

                // Plugin-driven `rterm.start_search(query, regex)` — opens
                // the overlay and optionally pre-fills the query. Refreshes
                // matches right away so the first jump is in place.
                if let Some((query, regex_mode)) =
                    self.events.take_pending_start_search()
                {
                    self.start_search();
                    if let Some(s) = self.search.as_mut() {
                        s.query = query;
                        s.regex_mode = regex_mode;
                    }
                    self.refresh_matches();
                    self.jump_to_current_match();
                }

                // Programmatic pane focus by stable uid. Resolve uid →
                // (tab, pane) and fall through to the index-based path
                // below. A uid that no longer points at a live pane is
                // silently dropped (same convention as
                // `set_pane_title_by_uid`).
                let by_uid = self
                    .events
                    .take_pending_focus_by_uid()
                    .and_then(|uid| self.find_pane_indices_by_uid(uid));
                // Programmatic pane focus from `rterm.focus_pane`. uid-
                // resolved focus takes priority — if a plugin fires both
                // in the same frame, prefer the uid since it's the more
                // stable address.
                if let Some((tab_idx, pane_idx)) = by_uid.or_else(|| self.events.take_pending_focus()) {
                    if tab_idx < self.tabs.len() {
                        let target_path = self.tabs[tab_idx]
                            .tree
                            .leaf_paths()
                            .into_iter()
                            .nth(pane_idx);
                        if let Some(path) = target_path {
                            if self.active_tab != tab_idx {
                                self.active_tab = tab_idx;
                                self.mark_tab_read(tab_idx);
                                self.end_search();
                                let payload = self.tab_switch_payload(tab_idx);
                                self.events.emit("tab.switch", &payload);
                            }
                            self.tabs[tab_idx].focus_path = path;
                            self.sync_terminal_size();
                            self.update_title();
                            let payload = self.pane_focus_payload(tab_idx, pane_idx);
                            self.events.emit("pane.focus", &payload);
                        }
                    }
                }

                // Plugin-requested tab focus (without disturbing which pane
                // is focused inside the tab). `select_tab` handles the no-op
                // case when `idx == active_tab` and the OOB case internally.
                if let Some(idx) = self.events.take_pending_tab_focus() {
                    self.select_tab(idx);
                }

                // Push a fresh terminal snapshot to plugins (so `rterm.cwd()`
                // etc. return current values).
                let mut all_panes: Vec<PaneSnapshotInfo> = Vec::new();
                // Whether anything reads the snapshot's text fields this
                // frame. When no plugin is loaded, skip the per-pane
                // grid + scrollback string building (the expensive part)
                // — the cheap metadata and side effects (foreground-
                // process caching for tab titles, silence events) still
                // run unconditionally.
                let want_text = self.events.wants_terminal_state();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                for (tab_idx, tab) in self.tabs.iter().enumerate() {
                    let focused_idx = tab.focused_index().unwrap_or(usize::MAX);
                    for (pane_idx, pane) in tab.panes().into_iter().enumerate() {
                        let last = pane.last_output_ms.load(Ordering::Relaxed);
                        let idle = now.saturating_sub(last);
                        let (alt, reverse_screen, pane_cwd, size, cursor, sb_len, cur_vis, mouse, cshape, cblink, pmarks, cmarks) = pane
                            .terminal
                            .lock()
                            .ok()
                            .map(|t| {
                                // Index into the canonical name tables
                                // so a rename of `CURSOR_SHAPE_NAMES` /
                                // `MOUSE_MODE_NAMES` propagates here
                                // without touching the match arms.
                                let mouse_idx = match t.mouse_tracking() {
                                    MouseTracking::Off => 0,
                                    MouseTracking::X10 => 1,
                                    MouseTracking::ButtonEvent => 2,
                                    MouseTracking::AnyEvent => 3,
                                };
                                let mouse_name = MOUSE_MODE_NAMES[mouse_idx];
                                let shape_idx = match t.cursor_shape() {
                                    rterm_core::CursorShape::Block => 0,
                                    rterm_core::CursorShape::Underline => 1,
                                    rterm_core::CursorShape::Bar => 2,
                                };
                                let shape_name = CURSOR_SHAPE_NAMES[shape_idx];
                                (
                                    t.is_on_alt_screen(),
                                    t.is_reverse_screen(),
                                    t.cwd().map(|s| s.to_string()),
                                    t.size(),
                                    t.cursor(),
                                    t.scrollback_len(),
                                    t.cursor_visible(),
                                    mouse_name,
                                    shape_name,
                                    t.cursor_should_blink(),
                                    t.prompt_marks().len(),
                                    t.command_marks().len(),
                                )
                            })
                            .unwrap_or((
                                false,
                                false,
                                None,
                                rterm_core::Size::default(),
                                rterm_core::Position::default(),
                                0,
                                true,
                                "off",
                                "block",
                                true,
                                0,
                                0,
                            ));
                        // Fire `pane.cursor_shape` on DECSCUSR change.
                        // Payload mirrors `pane.alt_enter` —
                        // `<tab>:<pane>\t<uid>\t<shape>` so legacy
                        // `(%d+):(%d+)` parsers still extract
                        // tab/pane and the new field tail-appends.
                        let shape_code = cursor_shape_code(cshape);
                        let prev_shape = pane.last_cursor_shape.swap(shape_code, Ordering::Relaxed);
                        if prev_shape != shape_code {
                            let payload = pane_attr_payload(
                                tab_idx + 1, pane_idx + 1, pane.uid, cshape,
                            );
                            self.events.emit("pane.cursor_shape", &payload);
                        }
                        // Same edge-trigger pattern for blink.
                        // DECSCUSR commonly pairs both — a plugin
                        // listening on only one of the two events
                        // still gets the relevant flip without
                        // having to subscribe to both and dedupe.
                        let prev_blink = pane.last_cursor_blink.swap(cblink, Ordering::Relaxed);
                        if prev_blink != cblink {
                            let payload =
                                pane_attr_payload(tab_idx + 1, pane_idx + 1, pane.uid, cblink);
                            self.events.emit("pane.cursor_blink", &payload);
                        }
                        // And again for ?25 visibility. Vim-style
                        // apps hide the cursor during long renders
                        // and restore on idle; plugins watching for
                        // "is the user editing now?" want this edge.
                        let prev_vis = pane.last_cursor_visible.swap(cur_vis, Ordering::Relaxed);
                        if prev_vis != cur_vis {
                            let payload =
                                pane_attr_payload(tab_idx + 1, pane_idx + 1, pane.uid, cur_vis);
                            self.events.emit("pane.cursor_visible", &payload);
                        }
                        // Mouse-tracking edge: useful for plugins that
                        // want to know "is a TUI capturing my clicks
                        // now?" — e.g. to disable a hover-tooltip when
                        // the focused pane just entered mouse-grab mode.
                        let mouse_code = mouse_mode_code(mouse);
                        let prev_mouse = pane.last_mouse_mode.swap(mouse_code, Ordering::Relaxed);
                        if prev_mouse != mouse_code {
                            let payload = pane_attr_payload(
                                tab_idx + 1, pane_idx + 1, pane.uid, mouse,
                            );
                            self.events.emit("pane.mouse_mode", &payload);
                        }
                        // Scrollback enter/leave: edge-triggered on the
                        // "is scroll_offset > 0?" boolean. Plugins that
                        // dim a status bar while the user is reading
                        // history watch this pair instead of polling
                        // `list_panes()[focused].scroll_offset` each
                        // frame.
                        let in_sb = pane.scroll_offset.load(Ordering::Relaxed) > 0;
                        let prev_in_sb = pane.last_in_scrollback.swap(in_sb, Ordering::Relaxed);
                        if prev_in_sb != in_sb {
                            let payload = pane_edge_payload(tab_idx + 1, pane_idx + 1, pane.uid);
                            let name = if in_sb {
                                "pane.scrollback_enter"
                            } else {
                                "pane.scrollback_leave"
                            };
                            self.events.emit(name, &payload);
                        }
                        // Fire alt-screen transition events. Comparing
                        // here (rather than inside the parser) keeps
                        // emission tied to render frames so multiple flips
                        // within a single frame coalesce naturally.
                        // DECSCNM ?5 edge: fires when the pane's
                        // reverse-screen flag flips. Plugins that
                        // mirror the visual flash in their own
                        // overlay (e.g. invert a status badge)
                        // subscribe here.
                        let prev_rev = pane
                            .last_reverse_screen
                            .swap(reverse_screen, Ordering::Relaxed);
                        if prev_rev != reverse_screen {
                            let payload = pane_attr_payload(
                                tab_idx + 1,
                                pane_idx + 1,
                                pane.uid,
                                reverse_screen,
                            );
                            self.events.emit("pane.reverse_screen", &payload);
                        }
                        let prev_alt = pane.last_alt_screen.swap(alt, Ordering::Relaxed);
                        if alt != prev_alt {
                            // Payload `<tab>:<pane>\t<uid>` — uid lets
                            // plugins answer "did MY watched pane just
                            // enter / leave the alt screen?" without
                            // re-resolving the index pair to a uid each
                            // frame. Legacy `(%d+):(%d+)` parsers still
                            // extract tab/pane from the prefix.
                            let payload = pane_edge_payload(tab_idx + 1, pane_idx + 1, pane.uid);
                            let name = if alt { "pane.alt_enter" } else { "pane.alt_leave" };
                            self.events.emit(name, &payload);
                            // Selection rects use viewport row indices, but the
                            // viewport just got replaced (alt grid swap). A
                            // leftover selection would highlight rows of
                            // completely different content, so drop it.
                            if let Some(sel) = self.selection.as_ref() {
                                if sel.pane_idx == pane_idx && tab_idx == self.active_tab {
                                    self.selection = None;
                                }
                            }
                            // Same logic for an open search overlay — its
                            // logical-line matches refer to the primary
                            // pane's grid+scrollback, none of which is
                            // valid against the freshly-swapped alt grid.
                            // Inlined `end_search` because the outer
                            // `self.tabs.iter()` is still borrowed.
                            let close_search = self
                                .search
                                .as_ref()
                                .map(|s| {
                                    s.pane_idx == pane_idx && tab_idx == self.active_tab
                                })
                                .unwrap_or(false);
                            if close_search {
                                self.search = None;
                                self.events.emit("search.end", "");
                            }
                        }
                        let pid = pane.io.process_id();
                        // OSC-7-less shells (dash, some fish setups) never
                        // announce their cwd. On Linux fall back to reading
                        // `/proc/<pid>/cwd` so plugins can still attribute a
                        // working directory to the pane. The read is one
                        // syscall and only fires when `cwd` is None.
                        let pane_cwd = pane_cwd.or_else(|| pid_cwd_fallback(pid));
                        // Edge-trigger a per-pane cwd event so plugins can
                        // react to a specific pane changing directory without
                        // having to diff `list_panes()` themselves.
                        if let Ok(mut last) = pane.last_cwd.lock() {
                            if *last != pane_cwd {
                                if let Some(c) = pane_cwd.as_ref() {
                                    let payload = pane_text_payload(
                                        tab_idx + 1, pane_idx + 1, c,
                                    );
                                    self.events.emit("pane.cwd", &payload);
                                }
                                *last = pane_cwd.clone();
                            }
                        }
                        let foreground_pgid = pane.io.foreground_pgid();
                        let foreground_process = foreground_pgid
                            .and_then(read_proc_comm_or_none);
                        // Cache for `display_title`'s fallback so the tab
                        // bar shows `vim` / `htop` etc. without needing the
                        // shell to emit OSC 0/1/2 on each foreground swap.
                        if let Ok(mut last) = pane.last_foreground_process.lock() {
                            *last = foreground_process.clone();
                        }
                        let bell_muted = pane.bell_muted.load(Ordering::Relaxed);
                        let last_exit_code = pane
                            .last_exit_code
                            .lock()
                            .ok()
                            .and_then(|g| *g);
                        let progress = pane.progress.lock().ok().and_then(|g| *g);
                        // Per-pane visible-grid text. The snapshot already
                        // held the terminal lock above to read size/cursor
                        // /alt — re-lock here is cheap and keeps the
                        // earlier scope tightly typed. Skipping the lock
                        // on contention is fine because the renderer
                        // re-snapshots every frame.
                        let pane_text = if want_text {
                            pane.terminal
                                .lock()
                                .ok()
                                .map(|t| grid_text_snapshot(&t))
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        // Per-pane scrollback tail (capped). Skipped on
                        // alt screen — the ring belongs to the suspended
                        // primary screen and surfacing it would mix in
                        // stale content the user isn't looking at.
                        let pane_tail = if want_text && !alt {
                            pane.terminal
                                .lock()
                                .ok()
                                .map(|t| {
                                    scrollback_text_snapshot_capped(&t, SCROLLBACK_TAIL_MAX)
                                })
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        all_panes.push(PaneSnapshotInfo {
                            tab: tab_idx,
                            pane: pane_idx,
                            uid: pane.uid,
                            title: pane.display_title(),
                            focused: tab_idx == self.active_tab && pane_idx == focused_idx,
                            idle_ms: idle,
                            scroll_offset: pane.scroll_offset.load(Ordering::Relaxed),
                            alt_screen: alt,
                            reverse_screen,
                            cwd: pane_cwd,
                            cols: size.cols,
                            rows: size.rows,
                            cursor_row: cursor.row + 1,
                            cursor_col: cursor.col + 1,
                            scrollback_len: sb_len,
                            cursor_visible: cur_vis,
                            cursor_shape: cshape,
                            cursor_blink: cblink,
                            mouse_mode: mouse,
                            prompt_marks: pmarks,
                            command_marks: cmarks,
                            pid,
                            foreground_pgid,
                            foreground_process,
                            bell_muted,
                            last_exit_code,
                            progress,
                            text: pane_text,
                            scrollback_tail: pane_tail,
                        });
                    }
                }
                let mut tab_snaps: Vec<TabSnapshotInfo> = Vec::with_capacity(self.tabs.len());
                for (tab_idx, tab) in self.tabs.iter().enumerate() {
                    // Tab-aggregate alt-screen edge: fires once when
                    // the first pane in this tab enters alt-screen,
                    // and once when the last pane leaves. Plugins
                    // that gate tab-bar styling on "is this tab
                    // running a TUI?" subscribe here instead of
                    // de-duplicating per-pane events themselves.
                    let any_alt = all_panes
                        .iter()
                        .filter(|p| p.tab == tab_idx)
                        .any(|p| p.alt_screen);
                    let prev_any_alt = tab.last_any_alt.swap(any_alt, Ordering::Relaxed);
                    if prev_any_alt != any_alt {
                        // `<tab+1>\t<focused_pane_uid>` — same shape as
                        // `tab.switch`. uid is 0 when the tab is
                        // transiently empty (between layout edits).
                        let focused_uid = tab
                            .focused_index()
                            .and_then(|i| tab.pane_at(i))
                            .map(|p| p.uid)
                            .unwrap_or(0);
                        let payload = tab_event_payload(tab_idx + 1, focused_uid);
                        let name = if any_alt { "tab.alt_enter" } else { "tab.alt_leave" };
                        self.events.emit(name, &payload);
                    }
                    let idle = all_panes
                        .iter()
                        .filter(|p| p.tab == tab_idx)
                        .map(|p| p.idle_ms)
                        .min()
                        .unwrap_or(u64::MAX);
                    let progress = all_panes
                        .iter()
                        .filter(|p| p.tab == tab_idx)
                        .filter_map(|p| p.progress)
                        .max_by_key(|(state, pct)| (progress_severity(*state), *pct));
                    // Tab-aggregate progress edge: fires when the
                    // tab's max-severity progress tuple changes,
                    // including clear→active and active→clear.
                    // Status-line plugins subscribe here instead of
                    // diffing `tabs()[].progress` themselves.
                    // Payload: `<tab+1>\t<state>\t<pct>` (cleared →
                    // state=0, pct=0).
                    if let Ok(mut last) = tab.last_progress.lock() {
                        if *last != progress {
                            let (state, pct) = progress.unwrap_or((0, 0));
                            let payload = tab_progress_payload(tab_idx + 1, state, pct);
                            self.events.emit("tab.progress", &payload);
                            *last = progress;
                        }
                    }
                    let focused_pane_uid = tab
                        .focused_index()
                        .and_then(|i| tab.pane_at(i))
                        .map(|p| p.uid)
                        .unwrap_or(0);
                    tab_snaps.push(TabSnapshotInfo {
                        idx: tab_idx,
                        focused: tab_idx == self.active_tab,
                        pane_count: tab.pane_count(),
                        focused_pane: tab.focused_index().unwrap_or(0),
                        focused_pane_uid,
                        zoomed: tab.zoomed,
                        custom_title: tab.custom_title.clone(),
                        idle_ms: idle,
                        unread: tab.unread,
                        progress,
                    });
                }
                // Edge-triggered `tab.silence` / `pane.silence`: an entity
                // that produced output earlier (silence_armed) and is now
                // idle longer than the configured threshold fires exactly
                // once. Plugins use the pair for "ping me when this command
                // is done" — at tab granularity, or at pane granularity for
                // split layouts running independent commands.
                // `tab_silence_ms == 0` disables both events entirely.
                if self.tab_silence_ms > 0 {
                    let threshold = self.tab_silence_ms;
                    for tab_snap in &tab_snaps {
                        let i = tab_snap.idx;
                        let armed = self
                            .tabs
                            .get(i)
                            .map(|t| t.silence_armed)
                            .unwrap_or(false);
                        if armed
                            && tab_snap.idle_ms >= threshold
                            && tab_snap.idle_ms != u64::MAX
                        {
                            if let Some(tab) = self.tabs.get_mut(i) {
                                tab.silence_armed = false;
                            }
                            // Trailing `\t<focused_pane_uid>` field
                            // lets uid-tracking plugins answer "is my
                            // watched pane the one that went silent?"
                            // without a follow-up `list_panes()` walk.
                            // `0` when the tab somehow has no focused
                            // pane (transient between layout edits).
                            self.events.emit(
                                "tab.silence",
                                &format!(
                                    "{}\t{}\t{}",
                                    i + 1,
                                    tab_snap.idle_ms,
                                    tab_snap.focused_pane_uid,
                                ),
                            );
                        }
                    }
                    // Per-pane variant — uses the same threshold but the
                    // `silence_armed` flag lives on each Pane so siblings
                    // don't disturb each other's gates.
                    for pane_snap in &all_panes {
                        if pane_snap.idle_ms < threshold
                            || pane_snap.idle_ms == u64::MAX
                        {
                            continue;
                        }
                        let Some(tab) = self.tabs.get(pane_snap.tab) else {
                            continue;
                        };
                        let Some(pane) = tab.pane_at(pane_snap.pane) else {
                            continue;
                        };
                        if pane.silence_armed.swap(false, Ordering::Relaxed) {
                            // Payload: `<tab>:<pane>\t<idle_ms>\t<uid>`.
                            // Additive uid lets plugins answer "did MY
                            // watched pane just go silent?" without
                            // re-resolving the index pair.
                            self.events.emit(
                                "pane.silence",
                                &format!(
                                    "{}:{}\t{}\t{}",
                                    pane_snap.tab + 1,
                                    pane_snap.pane + 1,
                                    pane_snap.idle_ms,
                                    pane.uid,
                                ),
                            );
                        }
                    }
                }
                if let Some(pane) = self.focused_pane() {
                    let snap = if let Ok(t) = pane.terminal.lock() {
                        let rows = t.size().rows;
                        let text = if want_text {
                            grid_text_snapshot(&t)
                        } else {
                            String::new()
                        };
                        // Logical points — the same unit plugins pass to
                        // `rterm.set_font_size`. The TextLayer's value is
                        // physical (scale-multiplied) and would read 2×
                        // too big on HiDPI.
                        let font_size = self.font_size;
                        let font_family = self
                            .state
                            .as_ref()
                            .map(|s| s.text.font_family().to_string())
                            .unwrap_or_else(|| self.font_family.clone());
                        let (cell_w, line_h) = self
                            .state
                            .as_ref()
                            .map(|s| (s.text.cell_width(), s.text.line_height()))
                            .unwrap_or((0.0, 0.0));
                        let prompt_mark_lines: Vec<usize> =
                            t.prompt_marks().iter().copied().collect();
                        let command_mark_lines: Vec<usize> =
                            t.command_marks().iter().copied().collect();
                        let scrollback_limit = t.scrollback_limit();
                        // Snapshot the values that need the guard *now*,
                        // then release so `selection_text` (which takes
                        // its own pane lock) can't deadlock on the same
                        // focused pane.
                        let cwd = t.cwd().map(|s| s.to_string());
                        let cols = t.size().cols;
                        // Focused-pane scrollback tail for
                        // `rterm.scrollback_text()`. Skipped on alt
                        // screen (vim/less) where the scrollback ring
                        // belongs to the suspended primary screen and
                        // exposing it would surface stale content
                        // unrelated to what the user is looking at.
                        let scrollback_text = if want_text && !t.is_on_alt_screen() {
                            scrollback_text_snapshot(&t)
                        } else {
                            String::new()
                        };
                        drop(t);
                        let selection_text = self.selection_text();
                        TerminalSnapshot {
                            cwd,
                            title: pane.dynamic_title.lock().ok().and_then(|g| g.clone()),
                            cols,
                            rows,
                            panes: all_panes,
                            tabs: tab_snaps,
                            grid_text: text,
                            font_size,
                            font_family,
                            cell_width: cell_w,
                            line_height: line_h,
                            tab_silence_ms: self.tab_silence_ms,
                            slow_command_ms: self.slow_command_ms,
                            scroll_on_output: self.scroll_on_output,
                            show_scrollbar: self.show_scrollbar,
                            bell_visual: self.bell_visual,
                            bell_urgent: self.bell_urgent,
                            cursor_blink: self.cursor_blink,
                            named_palette: palette::palette().named,
                            dragging_tab: self.tab_dragging.map(|i| (i + 1) as u32),
                            scrollback_limit,
                            selection_text,
                            opacity: self.opacity,
                            window_focused: self.window_focused,
                            last_exit_code: self.last_exit_code,
                            prompt_mark_lines,
                            command_mark_lines,
                            theme_fg: palette::default_fg(),
                            theme_bg: palette::default_bg(),
                            // Mirror xterm convention: an unset cursor
                            // colour falls back to the default fg.
                            theme_cursor: palette::cursor_color()
                                .unwrap_or_else(palette::default_fg),
                            scrollback_text,
                            search_active: self.search.is_some(),
                            search_query: self
                                .search
                                .as_ref()
                                .map(|s| s.query.clone())
                                .unwrap_or_default(),
                            // `current` is 0-based internally; surface
                            // 1-based to Lua (matches the in-overlay
                            // counter UX) but report `0` when there are
                            // no matches so plugins can spot "n=0" by
                            // checking either field.
                            search_match_index: self
                                .search
                                .as_ref()
                                .filter(|s| !s.matches.is_empty())
                                .map(|s| (s.current + 1) as u32)
                                .unwrap_or(0),
                            search_match_total: self
                                .search
                                .as_ref()
                                .map(|s| s.matches.len() as u32)
                                .unwrap_or(0),
                            search_regex_mode: self
                                .search
                                .as_ref()
                                .map(|s| s.regex_mode)
                                .unwrap_or(false),
                            active_theme: self.active_theme.clone(),
                        }
                    } else {
                        TerminalSnapshot { panes: all_panes, tabs: tab_snaps, ..Default::default() }
                    };
                    // Edge-trigger `cwd` / `title` events when the focused
                    // pane's values change between frames.
                    let new_cwd = snap.cwd.clone();
                    if self.last_focused_cwd != new_cwd {
                        self.last_focused_cwd = new_cwd.clone();
                        if let Some(c) = new_cwd {
                            self.events.emit("cwd", &c);
                        }
                    }
                    let new_title = snap.title.clone();
                    if self.last_focused_title != new_title {
                        self.last_focused_title = new_title.clone();
                        if let Some(t) = new_title {
                            self.events.emit("title", &t);
                        }
                    }
                    self.events.set_terminal_state(snap);
                }
                if let Some(_idx) = bell_pane {
                    // The `bell` event already fired up above for every
                    // ringing pane; here we only handle the focused-tab
                    // visual flash + taskbar attention. Both are
                    // independently disabled via `terminal.bell_visual` /
                    // `terminal.bell_urgent` for users who want one but
                    // not the other.
                    if self.bell_visual {
                        self.flash_until =
                            Some(Instant::now() + Duration::from_millis(BELL_FLASH_FADE_MS));
                    }
                    if self.bell_urgent && !self.window_focused {
                        if let Some(s) = self.state.as_ref() {
                            s.window.request_user_attention(Some(
                                winit::window::UserAttentionType::Informational,
                            ));
                        }
                    }
                }
                if window_title_changed {
                    self.update_title();
                }

                // OSC 52 clipboard write. Drain EVERY pane each frame so
                // a background pane can't accumulate pending writes and
                // then flush a stale clipboard overwrite the moment it
                // gains focus. Only the focused pane's write is applied
                // (matching the "focused pane controls the clipboard"
                // intent); background panes' writes are drained + dropped.
                let focused_uid = self
                    .tabs
                    .get(self.active_tab)
                    .and_then(|t| t.focused_pane())
                    .map(|p| p.uid);
                let panes_iter = self
                    .tabs
                    .iter()
                    .flat_map(|t| t.panes().into_iter().map(|p| (p.uid, &p.terminal)))
                    .collect::<Vec<_>>();
                let osc52 = drain_osc52(panes_iter.into_iter(), focused_uid);
                if let Some(b64) = osc52 {
                    // Drain the pending OSC 52 even when policy is deny —
                    // otherwise a shell that keeps trying would have the
                    // payload re-queued indefinitely and we'd never make
                    // progress past it. With `allow_osc52 = false` we
                    // simply drop the bytes and emit `osc52.blocked` so a
                    // plugin can surface a toast.
                    if !self.allow_osc52 {
                        tracing::debug!("OSC 52 clipboard write blocked by config (allow_osc52 = false)");
                        self.events.emit("osc52.blocked", &b64);
                    } else {
                        use base64::Engine;
                        match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
                            Ok(bytes) => {
                                if let Ok(text) = std::str::from_utf8(&bytes) {
                                    clipboard_set(text);
                                    self.events.emit("osc52.write", text);
                                }
                            }
                            Err(e) => tracing::debug!("OSC 52 base64 decode failed: {e}"),
                        }
                    }
                }
                {
                    static R_OSCDRAIN: std::sync::Once = std::sync::Once::new();
                    R_OSCDRAIN.call_once(|| tracing::info!("redraw: past OSC drain"));
                }
                // 1 Hz heartbeat for plugin clock/idle-watch logic.
                // Emits `frame.tick` with payload `<epoch_seconds>` so
                // plugins can do "show wall clock in status bar" type
                // work without subscribing to every redraw. We track
                // monotonic Instant for the throttle and stuff the
                // wall-clock seconds into the payload.
                let now = Instant::now();
                let should_tick = self
                    .last_frame_tick
                    .map(|t| now.duration_since(t) >= Duration::from_secs(1))
                    .unwrap_or(true);
                if should_tick {
                    self.last_frame_tick = Some(now);
                    let epoch = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    self.events.emit("frame.tick", &epoch.to_string());
                }
                // 2) Render frame.
                let rects = self.layout_active_tab();
                let header_rect = self.header_rect();
                let tab_strip_rect_for_draw = self.header_rect();
                let mut label_storage: Vec<String> = Vec::new();
                let header_spans = self.header_spans(&mut label_storage);
                let mut tabs_storage: Vec<String> = Vec::new();
                let header_tabs_spans = self.header_tabs_spans(&mut tabs_storage);
                let header_tabs_layout = self.tab_layout();
                let mut tabs_ghost_storage: Vec<String> = Vec::new();
                let header_tabs_ghost_spans =
                    self.header_tabs_ghost_spans(&mut tabs_ghost_storage);
                let mut header_right_storage: Vec<String> = Vec::new();
                let header_right = self.header_right_spans(&mut header_right_storage);
                // Title bar text is folded back into the single-row
                // header design (window controls live next to tabs,
                // there's no separate title row to render). Keep the
                // status bar at the bottom unchanged.
                let title_bar: AnchoredSpans = None;
                let mut status_bar_storage: Vec<String> = Vec::new();
                // Bottom-bar dispatch: search prompt wins over the
                // scrollback indicator (rare case: user opens search
                // while scrolled into history — search is the active
                // gesture). When neither is active, the bar's height
                // is 0 and `status_bar_spans` returns None.
                let status_bar = if self.search.is_some() {
                    self.search_bar_spans(&mut status_bar_storage)
                } else if self.bottom_bar_has_content() {
                    self.scrollback_bar_spans(&mut status_bar_storage)
                } else {
                    self.status_bar_spans(&mut status_bar_storage)
                };
                let search_sel = self.current_match_selection();
                let blink_on = self.cursor_blink_on();
                // Drop a completed tab-switch animation so subsequent
                // frames return to drawing the static accent stripe.
                if let Some(a) = self.tab_switch_anim {
                    if a.started_at.elapsed().as_millis() >= TAB_SWITCH_ANIM_MS {
                        self.tab_switch_anim = None;
                    }
                }
                // Same cleanup for the in-flight tab swap slide.
                if let Some(a) = self.tab_swap_anim {
                    if a.started_at.elapsed().as_millis() >= TAB_SWAP_ANIM_MS {
                        self.tab_swap_anim = None;
                    }
                }
                // Fire the debounced PTY resize once the user has
                // paused long enough on the resize drag — the shell
                // gets one final SIGWINCH at the actual settled size
                // instead of dozens at intermediate widths (which
                // produced one-char-per-line garbage in the
                // scrollback).
                if let Some(at) = self.pending_pty_resize_at {
                    if at.elapsed().as_millis() >= RESIZE_DEBOUNCE_MS {
                        self.pending_pty_resize_at = None;
                        self.sync_terminal_size();
                    }
                }
                // Overlay precedence: context menu > palette > settings >
                // help. One overlay at a time keeps input routing tidy.
                let mut help_storage: Vec<String> = Vec::new();
                let (overlay_spans, overlay_rect) =
                    if let Some(modal) = self.paste_confirmation.clone() {
                        // Paste-confirmation modal beats every
                        // other overlay — it's blocking input
                        // delivery, so the user must resolve it
                        // first.
                        (
                            self.paste_confirmation_spans(&modal, &mut help_storage),
                            self.paste_confirmation_rect(&modal),
                        )
                    } else if let Some(rt) = self.rename_tab.clone() {
                        (
                            self.rename_spans(&rt, &mut help_storage),
                            self.rename_rect(),
                        )
                    } else if let Some(menu) = self.context_menu.clone() {
                        (
                            self.context_menu_spans(&menu, &mut help_storage),
                            self.context_menu_rect(&menu),
                        )
                    } else if self.palette.is_some() {
                        match self.palette_overlay_spans(&mut help_storage) {
                            Some(s) => (s, self.help_rect()),
                            None => (Vec::new(), None),
                        }
                    } else if self.show_settings {
                        (self.settings_spans(&mut help_storage), self.help_rect())
                    } else if self.show_help {
                        (self.help_spans(&mut help_storage), self.help_rect())
                    } else if let Some(popup) = self.suggestion_popup.clone() {
                        // Suggestion popup lives at the bottom of
                        // the overlay precedence — modal overlays
                        // (palette / settings / help / rename /
                        // context menu) all hide it. When none of
                        // them are up, the popup renders as an
                        // inline auto-complete tray.
                        (
                            self.suggestion_popup_spans(&popup, &mut help_storage),
                            self.suggestion_popup_rect(&popup),
                        )
                    } else {
                        (Vec::new(), None)
                    };
                // Precompute tab-bar background quads + split divider
                // quads now, before the upcoming `self.state.as_mut()`
                // long-lived borrow, so we can move them into the render
                // call without overlap.
                let before_panes_quads: Vec<bg::BgQuad> = {
                    let mut v = self.tab_bar_quads();
                    v.extend(self.pane_split_quads());
                    v
                };
                // Bottom bar quads go in the AFTER-panes pass so they
                // visually mask the last terminal row when the scrollback
                // indicator is "floating" (i.e. does not reserve pane
                // space). Painting them BEFORE pane glyphs let pane text
                // bleed through the bar background and made the indicator
                // overlap whatever was on the last visible row.
                let status_bar_quads = self.status_bar_quads();
                // Resize-marker quads share the same "compute before
                // mut-borrowing state" requirement. Stored separately
                // so the after-panes assembly inside the state borrow
                // can just move-extend them.
                let resize_marker_quads = self.resize_marker_quads();
                // Paste-modal selection highlight: computed here in
                // `&self` context so it can be moved into the
                // mut-borrowed render block below without conflict.
                let paste_modal_selection_quads = self.paste_modal_selection_quads();
                // Cell width for header right-clip computation —
                // captured here so it's available inside the
                // `self.state.as_mut()` block below without re-
                // borrowing `self.state` immutably (would conflict
                // with the mutable borrow held by the outer `if let`).
                let header_clip_cell_w = self
                    .state
                    .as_ref()
                    .map(|s| s.text.cell_width())
                    .unwrap_or(8.0);
                let header_clip_os_decorations = self.os_decorations;
                {
                    static R_PRELAY: std::sync::Once = std::sync::Once::new();
                    R_PRELAY.call_once(|| tracing::debug!("redraw: about to enter state branch"));
                }
                if let Some(state) = self.state.as_mut() {
                    {
                        static IN: std::sync::Once = std::sync::Once::new();
                        IN.call_once(|| {
                            tracing::info!(
                                tabs = self.tabs.len(),
                                active = self.active_tab,
                                "entered render branch",
                            );
                        });
                    }
                    let Some(tab) = self.tabs.get(self.active_tab) else {
                        tracing::warn!(
                            active = self.active_tab,
                            tab_count = self.tabs.len(),
                            "no active tab; skipping render",
                        );
                        state.window.request_redraw();
                        return;
                    };
                    // If a pane's terminal mutex is poisoned (PTY reader
                    // or plugin host panicked with it held), skip the
                    // whole frame and request another redraw. The
                    // App-level prune path runs the next tick and reaps
                    // the dead pane via its `alive` flag; rendering
                    // half the panes is jarring, and panicking here
                    // takes the whole window down for a bug confined
                    // to one pane. Symmetric with `spawn_reader_thread`.
                    let guards: Vec<_> = match tab
                        .panes()
                        .iter()
                        .map(|p| p.terminal.lock())
                        .collect::<Result<Vec<_>, _>>()
                    {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                "render skipped: terminal mutex poisoned: {e}",
                            );
                            state.window.request_redraw();
                            return;
                        }
                    };
                    // Translate the absolute-coord selection to
                    // current viewport rows once per frame, using
                    // each guard's live `(sb_len, offset)` snapshot
                    // so the highlight follows wheel scrolls and
                    // scrollback evictions.
                    let sel_normalized = self.selection.and_then(|s| {
                        let g = guards.get(s.pane_idx)?;
                        let sb_len = if g.is_on_alt_screen() {
                            0
                        } else {
                            g.scrollback_len()
                        };
                        let rows = g.size().rows;
                        let pane = tab.pane_at(s.pane_idx)?;
                        let offset = pane.scroll_offset.load(Ordering::Relaxed);
                        s.to_visible_norm(sb_len, offset, rows)
                            .map(|n| (s.pane_idx, n))
                    });
                    // While searching, the active match takes priority over
                    // any user-drawn selection.
                    let draws: Vec<PaneDraw> = guards
                        .iter()
                        .zip(rects.iter())
                        .enumerate()
                        .map(|(i, (g, rect))| {
                            let sel = if let Some((_, n)) =
                                search_sel.filter(|(pi, _)| *pi == i)
                            {
                                Some(n)
                            } else {
                                sel_normalized
                                    .filter(|(pi, _)| *pi == i)
                                    .map(|(_, n)| n)
                            };
                            PaneDraw {
                                // `g: &MutexGuard<Terminal>` — explicit
                                // `&**g` deref is required to reach the
                                // underlying `&Terminal` for the field.
                                #[allow(clippy::explicit_auto_deref)]
                                terminal: &**g,
                                scroll_offset: tab
                                    .pane_at(i)
                                    .map(|p| p.scroll_offset.load(Ordering::Relaxed))
                                    .unwrap_or(0),
                                focused: Some(i) == tab.focused_index(),
                                rect: *rect,
                                selection: sel,
                                blink_on,
                                pane_uid: tab
                                    .pane_at(i)
                                    .map(|p| p.uid)
                                    .unwrap_or(0),
                            }
                        })
                        .collect();
                    // The main header text (tabs / hamburger / `+`)
                    // lives in the BOTTOM row of the header — the tab
                    // strip — not the title bar row. Render with
                    // `tab_strip_rect` so vertical centering inside the
                    // text buffer lands on the right strip.
                    let _ = header_rect;
                    let header_draw = tab_strip_rect_for_draw.map(|rect| {
                        // Same right boundary `tab_layout` uses
                        // (rect.right minus the window-control
                        // reserve plus the breathing gap). The
                        // TextLayer pixel-clips glyph rendering here
                        // so a long tab label can't paint over the
                        // minimize / maximize / close cluster.
                        let controls_cells = if header_clip_os_decorations {
                            0.0
                        } else {
                            (WINDOW_CONTROLS_WIDTH_CELLS + TAB_CONTROLS_GAP_CELLS)
                                as f32
                        };
                        let right_clip =
                            rect.left + rect.width - controls_cells * header_clip_cell_w;
                        HeaderDraw {
                            spans: header_spans,
                            rect,
                            right_clip: Some(right_clip),
                        }
                    });
                    // Compute the scroll-aware position of the tab-
                    // label buffer. `header_tabs_layout` was captured
                    // before the `&mut state` borrow above.
                    let header_tabs_draw = tab_strip_rect_for_draw.and_then(|rect| {
                        let layout = header_tabs_layout.as_ref()?;
                        let controls_cells = if header_clip_os_decorations {
                            0.0
                        } else {
                            (WINDOW_CONTROLS_WIDTH_CELLS + TAB_CONTROLS_GAP_CELLS)
                                as f32
                        };
                        let right_clip =
                            rect.left + rect.width - controls_cells * header_clip_cell_w;
                        // `layout.entries[0].left` is the pixel x of
                        // the first tab AFTER subtracting the scroll
                        // offset. Use it directly as the buffer's
                        // anchor so the glyphs slide in lock-step
                        // with the chip-fill quads in `tab_bar_quads`.
                        let first_left = layout
                            .entries
                            .first()
                            .map(|e| e.left as f32)
                            .unwrap_or(layout.hamburger_end as f32);
                        let left_clip = layout.hamburger_end as f32;
                        Some(HeaderTabsDraw {
                            spans: header_tabs_spans,
                            left: first_left,
                            top: rect.top,
                            width: (right_clip - first_left).max(1.0),
                            height: rect.height,
                            right_clip,
                            left_clip,
                        })
                    });
                    // Cursor-following ghost label. Built only when
                    // a drag is in flight (the spans builder returns
                    // None otherwise). Position math mirrors the
                    // ghost-chip branch in `tab_bar_quads`: the
                    // chip's left edge is `cursor.x - press_offset +
                    // gap/2`, and the label has to land exactly on
                    // top of that chip, so we reuse the same
                    // arithmetic plus the source tab's chip width.
                    let header_tabs_ghost_draw = match (
                        header_tabs_ghost_spans,
                        self.tab_dragging,
                        header_tabs_layout.as_ref(),
                        tab_strip_rect_for_draw,
                    ) {
                        (Some(spans), Some(src_idx), Some(layout), Some(rect)) => {
                            const TAB_GAP: f32 = 2.0; // matches `tab_bar_quads`
                            layout
                                .entries
                                .iter()
                                .find(|e| e.idx == src_idx)
                                .map(|e| {
                                    let chip_width =
                                        ((e.close_end - e.left) as f32 - TAB_GAP).max(2.0);
                                    let ghost_left = (self.cursor_pos.x
                                        - self.tab_drag_press_offset)
                                        as f32
                                        + TAB_GAP * 0.5;
                                    HeaderTabsGhostDraw {
                                        spans,
                                        left: ghost_left,
                                        top: rect.top,
                                        width: chip_width,
                                        height: rect.height,
                                    }
                                })
                        }
                        _ => None,
                    };
                    let header_right_draw =
                        header_right.map(|(spans, rect)| HeaderRightDraw { spans, rect });
                    let title_bar_draw =
                        title_bar.map(|(spans, rect)| TitleBarDraw { spans, rect });
                    let status_bar_draw =
                        status_bar.map(|(spans, rect)| StatusBarDraw { spans, rect });
                    // Paste-confirmation modal in Edit mode wants
                    // wrap-off so its mini-editor reads "1 buffer
                    // line == 1 visual row" — the click-to-cursor
                    // hit-test math relies on this. The settings
                    // overlay needs the same guarantee: its click
                    // hit-zones (`settings_hits`) are computed as
                    // `row × line_height`, so a word-wrapped line
                    // would shift every row below it away from its
                    // hit rect. Wrap-off keeps buttons clickable at
                    // any font size (overflow clips at the panel
                    // edge instead of re-flowing).
                    let settings_overlay_shown = self.show_settings
                        && self.paste_confirmation.is_none()
                        && self.rename_tab.is_none()
                        && self.context_menu.is_none()
                        && self.palette.is_none();
                    let overlay_nowrap = settings_overlay_shown
                        || matches!(
                            self.paste_confirmation.as_ref().map(|m| &m.mode),
                            Some(paste_confirm::PasteMode::Edit { .. }),
                        );
                    let overlay_draw = overlay_rect.map(|rect| OverlayDraw {
                        spans: overlay_spans,
                        rect,
                        nowrap: overlay_nowrap,
                    });
                    // Solid backdrop card behind any modal overlay so
                    // its text doesn't visually mix with pane content
                    // bleeding through the dim. Two layers: a darker
                    // shadow expanded by a few pixels, then the panel
                    // body on top of it.
                    let mut after_panes_quads: Vec<bg::BgQuad> = Vec::new();
                    // Bottom bar fill + separator — drawn AFTER pane
                    // glyphs so the bar masks any last-row content that
                    // would otherwise leak through. The bar text itself
                    // sits in `main_areas` and renders on top of this.
                    after_panes_quads.extend(status_bar_quads);
                    // Resize-marker strip / corner-L the cursor is over
                    // — drawn on top of pane content so the affordance
                    // is visible.
                    after_panes_quads.extend(resize_marker_quads);
                    if let Some(rect) = overlay_rect {
                        let pad = 6.0_f32;
                        let panel_radius = 8.0_f32;
                        // Shadow — same shape as the panel, blurred via
                        // the SDF anti-alias band so it fades softly.
                        after_panes_quads.push(bg::BgQuad::from_srgb_rounded(
                            [rect.left - pad, rect.top - pad],
                            [rect.width + pad * 2.0, rect.height + pad * 2.0],
                            [0, 0, 0],
                            0.55,
                            panel_radius + pad,
                        ));
                        // Panel body — slightly darker than the
                        // terminal bg so it's recognisable as a card.
                        let bg = palette::default_bg();
                        let panel = [
                            bg[0].saturating_sub(6),
                            bg[1].saturating_sub(6),
                            bg[2].saturating_sub(6),
                        ];
                        after_panes_quads.push(bg::BgQuad::from_srgb_rounded(
                            [rect.left, rect.top],
                            [rect.width, rect.height],
                            panel,
                            1.0,
                            panel_radius,
                        ));
                        // Accent border on the leading edge.
                        let accent = palette::default_fg().map(|c| c.saturating_sub(40));
                        after_panes_quads.push(bg::BgQuad::from_srgb(
                            [rect.left, rect.top + panel_radius],
                            [2.0, rect.height - panel_radius * 2.0],
                            accent,
                            0.85,
                        ));
                    }
                    // Selection highlight inside the paste-modal
                    // Edit area. Sits on top of the panel body and
                    // below the overlay text so the selected glyphs
                    // render with their normal foreground on the
                    // highlight rectangle.
                    after_panes_quads.extend(paste_modal_selection_quads);
                    // Bell-flash fade intensity: 1.0 right at the BEL,
                    // easing to 0 as `flash_until` approaches. The event
                    // loop is ControlFlow::Poll with an unconditional
                    // request_redraw per frame, so the fade gets smooth
                    // intermediate frames.
                    let flash = self
                        .flash_until
                        .map(|t| {
                            let now = Instant::now();
                            if t > now {
                                ((t - now).as_secs_f32()
                                    / Duration::from_millis(BELL_FLASH_FADE_MS).as_secs_f32())
                                .min(1.0)
                            } else {
                                0.0
                            }
                        })
                        .unwrap_or(0.0);
                    if flash <= 0.0 {
                        self.flash_until = None;
                    }
                    // DECSET ?2026 — Synchronized Output. While the
                    // focused pane is in sync mode (and we haven't hit
                    // the 200 ms safety timeout), defer the GPU present
                    // so apps painting multi-segment frames don't show
                    // half-finished state. The parser keeps consuming
                    // bytes from the PTY into the off-screen grid, so
                    // when sync flips off the next frame renders the
                    // fully composed result.
                    const SYNC_TIMEOUT: Duration = Duration::from_millis(200);
                    // CRITICAL: read `sync_output` from the already-held
                    // `guards` for the focused pane, not via a fresh
                    // `terminal.lock()`. The fresh lock would
                    // deadlock against the guard collected above —
                    // `std::sync::Mutex` is not reentrant. WSL2's
                    // llvmpipe was slow enough that the deadlock
                    // fired on every startup (manifested as a
                    // black window after the first clear frame).
                    let focus_syncing = tab
                        .focused_index()
                        .and_then(|i| guards.get(i))
                        .map(|g| g.sync_output())
                        .unwrap_or(false);
                    let defer = match (focus_syncing, self.sync_started_at) {
                        (true, None) => {
                            self.sync_started_at = Some(Instant::now());
                            true
                        }
                        (true, Some(started)) => {
                            // Honour the timeout: if the app's taking too
                            // long, render anyway so the user isn't stuck.
                            started.elapsed() < SYNC_TIMEOUT
                        }
                        (false, _) => {
                            self.sync_started_at = None;
                            false
                        }
                    };
                    if defer {
                        static FIRST_DEFER: std::sync::Once = std::sync::Once::new();
                        FIRST_DEFER.call_once(|| {
                            tracing::warn!(
                                focus_syncing,
                                elapsed_ms = self.sync_started_at
                                    .map(|t| t.elapsed().as_millis() as u64)
                                    .unwrap_or(0),
                                "DECSET ?2026 sync-output deferring frame; if this loops, an app forgot to reset the mode",
                            );
                        });
                        drop(draws);
                        drop(guards);
                        state.window.request_redraw();
                        return;
                    }

                    // Coalesce sub-frame PTY bursts only. The render loop
                    // re-requests a frame every tick, so on a fast present
                    // mode it spins at hundreds+ FPS and could present a
                    // multi-write burst mid-way. If the focused pane emitted
                    // output within the last few ms, hold the present briefly
                    // so the burst lands as one frame; cap it LOW so output
                    // is never delayed noticeably.
                    //
                    // NOTE: an earlier version also held frames much longer
                    // (up to 180 ms) while a bare-CR redraw was in flight to
                    // hide the Ctrl+C cursor flick. That fired on every zle
                    // line-redraw too, which made typing over SSH feel laggy
                    // — so it's gone. The residual Ctrl+C flick on a high-
                    // latency link is the lesser evil vs. input lag.
                    const SETTLE_MS: u64 = 3;
                    const COALESCE_MAX: Duration = Duration::from_millis(12);
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let focus_idle_ms = tab
                        .focused_index()
                        .and_then(|i| tab.panes().get(i).map(|p| p.last_output_ms.load(Ordering::Relaxed)))
                        .map(|last| now_ms.saturating_sub(last))
                        .unwrap_or(u64::MAX);
                    let coalesce = match (focus_idle_ms < SETTLE_MS, self.coalesce_started_at) {
                        (true, None) => {
                            self.coalesce_started_at = Some(Instant::now());
                            true
                        }
                        (true, Some(started)) => started.elapsed() < COALESCE_MAX,
                        (false, _) => {
                            self.coalesce_started_at = None;
                            false
                        }
                    };
                    if coalesce {
                        drop(draws);
                        drop(guards);
                        state.window.request_redraw();
                        return;
                    }

                    // Log first render (or each error) so the user can
                    // see whether the pipeline is actually presenting.
                    static FIRST: std::sync::Once = std::sync::Once::new();
                    FIRST.call_once(|| {
                        tracing::info!(
                            size = ?state.window.inner_size(),
                            tabs = self.tabs.len(),
                            active = self.active_tab,
                            panes = draws.len(),
                            "rendering first frame",
                        );
                    });
                    match state.render(
                        &draws,
                        header_draw.as_ref(),
                        header_right_draw.as_ref(),
                        header_tabs_draw.as_ref(),
                        header_tabs_ghost_draw.as_ref(),
                        title_bar_draw.as_ref(),
                        status_bar_draw.as_ref(),
                        overlay_draw.as_ref(),
                        flash,
                        self.show_scrollbar,
                        &before_panes_quads,
                        &after_panes_quads,
                    ) {
                        Ok(()) => {
                            static FIRST_OK: std::sync::Once = std::sync::Once::new();
                            FIRST_OK.call_once(|| {
                                tracing::info!("first frame presented");
                            });
                        }
                        Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                            let s = state.window.inner_size();
                            tracing::warn!(
                                width = s.width,
                                height = s.height,
                                "surface lost/outdated — reconfiguring",
                            );
                            state.resize(s.width, s.height);
                        }
                        Err(wgpu::SurfaceError::OutOfMemory) => {
                            tracing::error!("GPU out of memory; exiting");
                            event_loop.exit();
                            return;
                        }
                        Err(e) => tracing::warn!("render error: {e:?}"),
                    }
                    drop(draws);
                    drop(guards);
                }
                // Decide what wakes the loop next: continuous redraw
                // while an animation runs, a timed wake for blink /
                // resize / plugin heartbeat, or full idle Wait. This
                // replaces the old unconditional per-frame redraw that
                // spun the CPU at the present rate even when idle.
                self.schedule_after_frame(event_loop);
            }
            _ => {}
        }
        if needs_repaint {
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
        }
    }
}

pub fn run(cfg: RunConfig) -> Result<()> {
    // Build an `EventLoop<UserEvent>` so background threads (notably
    // the Windows global-hotkey worker) can wake the main loop via
    // `EventLoopProxy::send_event`. The fallback path stays unchanged
    // for builds where the worker isn't wired (Linux / macOS today):
    // the channel is just never used.
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("EventLoop::with_user_event")?;
    // Event-driven by default: the loop sleeps until a window event,
    // a `UserEvent::Wake` (PTY output), or a scheduled timer wake
    // (cursor blink / resize debounce / plugin heartbeat). Each frame's
    // `schedule_after_frame` re-arms the right wake. This replaces the
    // old `Poll` busy-loop that rendered at the present rate forever.
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let global_hotkey_spec = cfg
        .guake
        .as_ref()
        .map(|g| g.global_hotkey.clone())
        .unwrap_or_default();
    let mut app = App::new(cfg);
    // Wake handle for the PTY reader threads (installed into the
    // spawner in `resumed`).
    app.set_event_proxy(proxy.clone());
    // Spawn the global-hotkey worker AFTER `App::new` so the worker
    // never fires before the App is in a state ready to handle it.
    // The handle is owned by the App so its `Drop` unregisters
    // cleanly on shutdown.
    if !global_hotkey_spec.trim().is_empty() {
        app.install_global_hotkey(&global_hotkey_spec, proxy.clone());
    }
    // Don't propagate `EventLoopError::ExitFailure` as a fatal error
    // from `main`. winit emits it when the event loop's panic catcher
    // fires, when the display server connection dies during a rapid
    // resize storm, or when a handler returned a non-zero exit code.
    // None of those should turn a normal close into a non-zero exit
    // from rterm — we already wrote any session state in `exiting()`.
    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::warn!(error = ?e, "event loop ended with error — exiting cleanly");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise env-mutating tests across threads. `cargo test` is
    /// parallel by default; two tests racing on `HOME` (or any other
    /// process-wide variable) can observe each other's values and
    /// produce flaky failures.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn pt(row: u16, col: u16) -> SelectionPoint {
        SelectionPoint { row, col }
    }

    /// Test helper: build an `AbsPoint` directly from row/col. The
    /// selection tests assert against ActiveSelection's normalised
    /// output, which lives in absolute coords now.
    fn ap(row: i64, col: u16) -> AbsPoint {
        AbsPoint { abs_row: row, col }
    }

    #[test]
    fn toml_escape_handles_basic_string_control_chars() {
        // Unix filenames can contain backslash, quote, and even \n/\t,
        // and `write_session` embeds them inside a TOML basic string.
        // Without escaping, `toml::from_str` would refuse to parse the
        // saved session on next start. Pin the mapping for each byte
        // class plus the `\\` and `"` literals so a future refactor
        // can't drop one silently.
        assert_eq!(toml_escape_basic_string("plain"), "plain");
        assert_eq!(toml_escape_basic_string("a\\b"), "a\\\\b");
        assert_eq!(toml_escape_basic_string("a\"b"), "a\\\"b");
        assert_eq!(toml_escape_basic_string("line1\nline2"), "line1\\nline2");
        assert_eq!(toml_escape_basic_string("\r\n"), "\\r\\n");
        assert_eq!(toml_escape_basic_string("col\tname"), "col\\tname");
        // Multi-replacement compounding: `\\` must be escaped first so the
        // subsequent quote substitution doesn't double-escape.
        assert_eq!(
            toml_escape_basic_string("\\\""),
            "\\\\\\\"",
        );
    }

    #[test]
    fn abbreviate_home_replaces_prefix() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os("HOME");
        // SAFETY of `set_var`: env mutation serialised by ENV_GUARD so
        // no other test thread is reading HOME concurrently.
        unsafe { std::env::set_var("HOME", "/home/u"); }
        assert_eq!(abbreviate_home("/home/u"), "~");
        assert_eq!(abbreviate_home("/home/u/projects/x"), "~/projects/x");
        // Non-prefix paths pass through unchanged.
        assert_eq!(abbreviate_home("/etc/passwd"), "/etc/passwd");
        // Edge case: a path that *starts* with the home text but isn't a
        // proper subpath (`/home/users`) must not be abbreviated to
        // `~sers` — the trailing `/` requirement guards this.
        assert_eq!(abbreviate_home("/home/users"), "/home/users");
        // Restore.
        match saved {
            Some(v) => unsafe { std::env::set_var("HOME", v); },
            None => unsafe { std::env::remove_var("HOME"); },
        }
    }

    #[test]
    fn probe_cell_width_yields_reasonable_advance() {
        // The probe must return a sub-font-size advance for a real
        // monospace font (DejaVu Sans Mono, present on every supported CI
        // image). At 13pt the natural M-advance is ~7.8px; anywhere in
        // [3, 13*1.5] is fine. We also guard the upper bound against
        // proportional fallbacks that would have given ~15px and broken
        // cell alignment — the probe rejects those via its `<= size*1.5`
        // sanity clamp, falling back to `font_size * 0.6` instead.
        let mut fs = FontSystem::new();
        // Whatever face the platform has — if no monospace at all is
        // installed, probe falls back; the test isn't asserting which
        // font is picked, only that the probe path produces a number.
        let value = probe_cell_width(&mut fs, 13.0, 16.25, Family::Monospace);
        if let Some(v) = value {
            assert!(v >= 3.0, "cell width {v} unrealistically small");
            assert!(v <= 13.0 * 1.5, "cell width {v} exceeds sanity clamp");
            assert!(v.is_finite());
        }
        // List_monospace_families is a pure read of fontdb — it must
        // never panic and must produce sorted, deduplicated output.
        let families = list_monospace_families();
        let mut sorted = families.clone();
        sorted.sort();
        assert_eq!(sorted, families, "list_monospace_families not sorted");
    }

    #[test]
    fn canonical_names_includes_anchor_actions() {
        // Pin the names that plugins / user configs most commonly
        // bind, so an editing slip that drops a variant from
        // `from_name` / `canonical_names` fails this test
        // immediately rather than silently breaking their keymap.
        // (The exhaustive-match test catches the inverse — new
        // variants without mappings.)
        let canonical: std::collections::HashSet<String> =
            AppAction::canonical_names().into_iter().collect();
        for must_have in [
            // Core tab + pane.
            "new_tab", "close_tab", "next_tab", "prev_tab",
            "goto_first_tab", "goto_last_tab", "toggle_last_tab",
            "move_tab_left", "move_tab_right",
            "split_horizontal", "split_vertical", "split_auto", "close_pane",
            "focus_next_pane", "focus_prev_pane",
            "focus_first_pane", "focus_last_pane",
            "swap_pane_next", "swap_pane_prev",
            "zoom_pane", "balance_panes", "reset_pane",
            "resize_pane_left", "resize_pane_right",
            "resize_pane_up", "resize_pane_down",
            "toggle_bell_mute",
            // Clipboard.
            "paste", "copy", "clear_selection",
            "copy_hovered_url", "open_hovered_url",
            // Scrollback navigation family.
            "scroll_page_up", "scroll_page_down",
            "scroll_half_page_up", "scroll_half_page_down",
            "scroll_line_up", "scroll_line_down",
            "scroll_home", "scroll_end",
            "clear_scrollback", "save_scrollback",
            // Shell-integration navigation.
            "jump_prev_prompt", "jump_next_prompt",
            "jump_prev_command", "jump_next_command",
            // Adjustment family.
            "font_increase", "font_decrease", "font_reset",
            "opacity_increase", "opacity_decrease", "opacity_reset",
            // Misc.
            "search", "toggle_help", "open_palette", "quit",
            // Theme / settings family.
            "cycle_theme", "cycle_theme_prev", "open_settings",
            "rename_tab",
            // Window-management family.
            "snap_window_left", "snap_window_right",
            "snap_window_top", "snap_window_bottom",
            "maximize_toggle", "minimize_window", "restore_window",
        ] {
            assert!(
                canonical.contains(must_have),
                "canonical_names is missing anchor action {must_have:?}; \
                 likely a regression in `from_name` / `canonical_names`",
            );
        }
    }

    #[test]
    fn ambiguous_aliases_resolve_to_the_documented_owner() {
        // Two action families historically claim the short alias
        // `last_tab`: `toggle_last_tab` (alt-tab "swap with previous
        // tab") *owns* it for back-compat; the newer "jump to
        // rightmost tab" action uses `goto_last_tab` instead.
        // Pin the ownership here so a future cleanup that swaps
        // them won't silently re-route every user's keybinding.
        assert!(matches!(
            AppAction::from_name("last_tab"),
            Some(AppAction::ToggleLastTab)
        ));
        assert!(matches!(
            AppAction::from_name("toggle_last_tab"),
            Some(AppAction::ToggleLastTab)
        ));
        assert!(matches!(
            AppAction::from_name("goto_last_tab"),
            Some(AppAction::LastTab)
        ));
        assert!(matches!(
            AppAction::from_name("rightmost_tab"),
            Some(AppAction::LastTab)
        ));
        assert!(matches!(
            AppAction::from_name("goto_first_tab"),
            Some(AppAction::FirstTab)
        ));
        assert!(matches!(
            AppAction::from_name("first_tab"),
            Some(AppAction::FirstTab)
        ));
    }

    #[test]
    fn focus_pane_action_aliases_parse() {
        // tmux users reach for `focus_top_pane` / `focus_bottom_pane`
        // naturally — pin the alias mapping so it survives a future
        // `from_name` cleanup.
        assert!(matches!(
            AppAction::from_name("focus_first_pane"),
            Some(AppAction::FocusFirstPane)
        ));
        assert!(matches!(
            AppAction::from_name("focus_top_pane"),
            Some(AppAction::FocusFirstPane)
        ));
        assert!(matches!(
            AppAction::from_name("focus_last_pane"),
            Some(AppAction::FocusLastPane)
        ));
        assert!(matches!(
            AppAction::from_name("focus_bottom_pane"),
            Some(AppAction::FocusLastPane)
        ));
    }

    #[test]
    fn scroll_action_aliases_parse() {
        // Vim-style aliases for the scroll family. These were
        // chosen for parity with `page_up` / `page_down`'s existing
        // short forms; a future refactor that drops one would
        // silently break user keybindings (`half_page_up = 'C-u'`).
        assert!(matches!(
            AppAction::from_name("scroll_line_up"),
            Some(AppAction::ScrollLineUp)
        ));
        assert!(matches!(
            AppAction::from_name("line_up"),
            Some(AppAction::ScrollLineUp)
        ));
        assert!(matches!(
            AppAction::from_name("scroll_line_down"),
            Some(AppAction::ScrollLineDown)
        ));
        assert!(matches!(
            AppAction::from_name("line_down"),
            Some(AppAction::ScrollLineDown)
        ));
        assert!(matches!(
            AppAction::from_name("scroll_half_page_up"),
            Some(AppAction::ScrollHalfPageUp)
        ));
        assert!(matches!(
            AppAction::from_name("half_page_up"),
            Some(AppAction::ScrollHalfPageUp)
        ));
        assert!(matches!(
            AppAction::from_name("scroll_half_page_down"),
            Some(AppAction::ScrollHalfPageDown)
        ));
        assert!(matches!(
            AppAction::from_name("half_page_down"),
            Some(AppAction::ScrollHalfPageDown)
        ));
    }

    #[test]
    fn opacity_action_aliases_parse() {
        // The opacity actions accept a small set of memorable aliases so
        // users coming from other terminals (alacritty, kitty, wezterm)
        // don't have to look up our exact name. Pin them so a refactor
        // of `from_name` can't silently drop an alias.
        // `AppAction` doesn't derive `PartialEq` (some variants would
        // need it on opaque inner state), so match on the result.
        assert!(matches!(
            AppAction::from_name("opacity_increase"),
            Some(AppAction::OpacityIncrease)
        ));
        assert!(matches!(
            AppAction::from_name("more_opaque"),
            Some(AppAction::OpacityIncrease)
        ));
        assert!(matches!(
            AppAction::from_name("raise_opacity"),
            Some(AppAction::OpacityIncrease)
        ));
        assert!(matches!(
            AppAction::from_name("opacity_decrease"),
            Some(AppAction::OpacityDecrease)
        ));
        assert!(matches!(
            AppAction::from_name("more_transparent"),
            Some(AppAction::OpacityDecrease)
        ));
        assert!(matches!(
            AppAction::from_name("opacity_reset"),
            Some(AppAction::OpacityReset)
        ));
    }

    #[test]
    fn canonical_names_round_trip_from_name() {
        // Every canonical name must parse cleanly via `from_name`.
        for n in AppAction::canonical_names() {
            assert!(
                AppAction::from_name(&n).is_some(),
                "canonical {n:?} did not round-trip",
            );
        }
    }

    #[test]
    fn name_label_pairs_match_canonical_names_and_from_name() {
        // Each `(name, label)` pair must:
        //   1. use a name that parses via `from_name` (so a typo in
        //      `canonical_for` can't ship a unparseable label entry);
        //   2. appear exactly once in `canonical_names()` (so adding a
        //      new variant without updating either table fails here);
        //   3. carry a non-empty label (so `--list-actions --labels`
        //      doesn't dump a blank second column for any entry).
        let canonical: std::collections::HashSet<String> =
            AppAction::canonical_names().into_iter().collect();
        let pairs = AppAction::name_label_pairs();
        // Same total count → no drift in either direction.
        assert_eq!(
            pairs.len(),
            canonical.len(),
            "name_label_pairs and canonical_names disagree on action count",
        );
        for (name, label) in &pairs {
            assert!(
                AppAction::from_name(name).is_some(),
                "label-pair name {name:?} doesn't parse via from_name",
            );
            assert!(
                canonical.contains(*name),
                "label-pair name {name:?} not in canonical_names()",
            );
            assert!(!label.is_empty(), "label for {name:?} is empty");
        }
    }

    #[test]
    fn canonical_names_have_no_duplicates() {
        // Same rationale as builtin_event_names: duplicates would show
        // up in `--list-actions` / the command palette as two identical
        // entries, and would mean an edit added a new variant without
        // removing its predecessor.
        let names = AppAction::canonical_names();
        let mut sorted = names.clone();
        sorted.sort();
        let dups: Vec<_> = sorted
            .windows(2)
            .filter_map(|w| if w[0] == w[1] { Some(&w[0]) } else { None })
            .collect();
        assert!(
            dups.is_empty(),
            "duplicate action names in canonical_names: {:?}",
            dups,
        );
    }

    #[test]
    fn every_app_action_variant_has_canonical_name() {
        // Exhaustive match — if a new `AppAction` variant is added without
        // listing a name in `canonical_names`, this test fails to compile
        // (the `match` becomes non-exhaustive) instead of silently dropping
        // the new action from config parsing.
        let names = AppAction::canonical_names();
        let actions = [
            AppAction::NewTab,
            AppAction::CloseTab,
            AppAction::NextTab,
            AppAction::PrevTab,
            AppAction::FirstTab,
            AppAction::LastTab,
            AppAction::MoveTabLeft,
            AppAction::MoveTabRight,
            AppAction::SplitHorizontal,
            AppAction::SplitVertical,
            AppAction::SplitAuto,
            AppAction::ClosePane,
            AppAction::FocusNextPane,
            AppAction::FocusFirstPane,
            AppAction::FocusLastPane,
            AppAction::FocusPrevPane,
            AppAction::PasteClipboard,
            AppAction::CopySelection,
            AppAction::ClearSelection,
            AppAction::StartSearch,
            AppAction::ToggleHelp,
            AppAction::OpenPalette,
            AppAction::JumpPrevPrompt,
            AppAction::JumpNextPrompt,
            AppAction::JumpPrevCommand,
            AppAction::JumpNextCommand,
            AppAction::ScrollPageUp,
            AppAction::ScrollPageDown,
            AppAction::ScrollHalfPageUp,
            AppAction::ScrollHalfPageDown,
            AppAction::ScrollLineUp,
            AppAction::ScrollLineDown,
            AppAction::ScrollHome,
            AppAction::ScrollEnd,
            AppAction::ClearScrollback,
            AppAction::ResizePaneLeft,
            AppAction::ResizePaneRight,
            AppAction::ResizePaneUp,
            AppAction::ResizePaneDown,
            AppAction::SaveScrollback,
            AppAction::ZoomPane,
            AppAction::BalancePanes,
            AppAction::Quit,
            AppAction::FontIncrease,
            AppAction::FontDecrease,
            AppAction::FontReset,
            AppAction::OpacityIncrease,
            AppAction::OpacityDecrease,
            AppAction::OpacityReset,
            AppAction::ToggleLastTab,
            AppAction::CopyHoveredUrl,
            AppAction::ResetPane,
            AppAction::SwapPaneNext,
            AppAction::SwapPanePrev,
            AppAction::ToggleBellMute,
            AppAction::OpenHoveredUrl,
            AppAction::CycleTheme,
            AppAction::CycleThemePrev,
            AppAction::OpenSettings,
            AppAction::RenameTab,
            AppAction::SnapWindowLeft,
            AppAction::SnapWindowRight,
            AppAction::SnapWindowTop,
            AppAction::SnapWindowBottom,
            AppAction::MaximizeToggle,
            AppAction::MinimizeWindow,
            AppAction::RestoreWindow,
            AppAction::ToggleGuake,
        ];
        // Force the compiler to enforce exhaustiveness via a single match.
        for a in actions {
            let _: () = match a {
                AppAction::NewTab => (),
                AppAction::CloseTab => (),
                AppAction::NextTab => (),
                AppAction::PrevTab => (),
                AppAction::FirstTab => (),
                AppAction::LastTab => (),
                AppAction::MoveTabLeft => (),
                AppAction::MoveTabRight => (),
                AppAction::SplitHorizontal => (),
                AppAction::SplitVertical => (),
                AppAction::SplitAuto => (),
                AppAction::ClosePane => (),
                AppAction::FocusNextPane => (),
                AppAction::FocusFirstPane => (),
                AppAction::FocusLastPane => (),
                AppAction::FocusPrevPane => (),
                AppAction::PasteClipboard => (),
                AppAction::CopySelection => (),
                AppAction::ClearSelection => (),
                AppAction::StartSearch => (),
                AppAction::ToggleHelp => (),
                AppAction::OpenPalette => (),
                AppAction::JumpPrevPrompt => (),
                AppAction::JumpNextPrompt => (),
                AppAction::JumpPrevCommand => (),
                AppAction::JumpNextCommand => (),
                AppAction::ScrollPageUp => (),
                AppAction::ScrollPageDown => (),
                AppAction::ScrollHalfPageUp => (),
                AppAction::ScrollHalfPageDown => (),
                AppAction::ScrollLineUp => (),
                AppAction::ScrollLineDown => (),
                AppAction::ScrollHome => (),
                AppAction::ScrollEnd => (),
                AppAction::ClearScrollback => (),
                AppAction::ResizePaneLeft => (),
                AppAction::ResizePaneRight => (),
                AppAction::ResizePaneUp => (),
                AppAction::ResizePaneDown => (),
                AppAction::SaveScrollback => (),
                AppAction::ZoomPane => (),
                AppAction::BalancePanes => (),
                AppAction::Quit => (),
                AppAction::FontIncrease => (),
                AppAction::FontDecrease => (),
                AppAction::FontReset => (),
                AppAction::OpacityIncrease => (),
                AppAction::OpacityDecrease => (),
                AppAction::OpacityReset => (),
                AppAction::ToggleLastTab => (),
                AppAction::CopyHoveredUrl => (),
                AppAction::ResetPane => (),
                AppAction::SwapPaneNext => (),
                AppAction::SwapPanePrev => (),
                AppAction::ToggleBellMute => (),
                AppAction::OpenHoveredUrl => (),
                AppAction::CycleTheme => (),
                AppAction::CycleThemePrev => (),
                AppAction::OpenSettings => (),
                AppAction::RenameTab => (),
                AppAction::SnapWindowLeft => (),
                AppAction::SnapWindowRight => (),
                AppAction::SnapWindowTop => (),
                AppAction::SnapWindowBottom => (),
                AppAction::MaximizeToggle => (),
                AppAction::MinimizeWindow => (),
                AppAction::RestoreWindow => (),
                AppAction::ToggleGuake => (),
            };
        }
        // And the names list must cover every variant.
        assert_eq!(names.len(), actions.len(), "names list size mismatch");
    }

    #[test]
    fn word_back_delete_index_basic_ascii() {
        // `"foo bar"` + Ctrl+W → drop `bar`, keep `"foo "`.
        assert_eq!(word_back_delete_index("foo bar"), 4);
        // Trailing whitespace is trimmed FIRST, so `"foo bar  "` still
        // drops `bar` and lands at the trailing space's position.
        assert_eq!(word_back_delete_index("foo bar  "), 4);
        // No whitespace → whole query is dropped.
        assert_eq!(word_back_delete_index("foobar"), 0);
        // Empty stays empty.
        assert_eq!(word_back_delete_index(""), 0);
        // Single word with trailing whitespace → drop the word.
        assert_eq!(word_back_delete_index("hello   "), 0);
    }

    #[test]
    fn word_back_delete_index_respects_multibyte_whitespace() {
        // Regression: `char::is_whitespace` matches Unicode whitespace
        // (NBSP U+00A0 is 2 bytes, ideographic space U+3000 is 3
        // bytes). The old `i + 1` indexed one byte past the first
        // byte of the matched whitespace, which on NBSP landed
        // *inside* the char and made `String::truncate` panic at
        // runtime. The new implementation hands back a byte index
        // that is always on a UTF-8 char boundary.
        //
        // `"foo\u{00A0}bar"` → drop `bar`, keep `"foo<NBSP>"` = 5 bytes.
        let q = "foo\u{00A0}bar";
        let idx = word_back_delete_index(q);
        assert_eq!(idx, 5);
        // The returned index must be a real char boundary on every
        // path — String::truncate would panic otherwise.
        assert!(q.is_char_boundary(idx));

        // Ideographic space (3 bytes).
        let q = "foo\u{3000}bar";
        let idx = word_back_delete_index(q);
        assert_eq!(idx, 6);
        assert!(q.is_char_boundary(idx));

        // Multi-byte word chars on the *non-whitespace* side, ASCII
        // whitespace between → keep CJK prefix + the space.
        let q = "日本 語";
        let idx = word_back_delete_index(q);
        assert!(q.is_char_boundary(idx));
        // "日本" = 6 bytes, " " = 1 byte → idx = 7.
        assert_eq!(idx, 7);
    }

    #[test]
    fn parse_key_spec_ctrl_shift_t() {
        let (mods, k) = parse_key_spec("Ctrl+Shift+T").unwrap();
        assert!(mods.contains(ModifiersState::CONTROL));
        assert!(mods.contains(ModifiersState::SHIFT));
        assert!(!mods.contains(ModifiersState::ALT));
        assert_eq!(k, KeyMatch::Char("t".to_string()));
    }

    #[test]
    fn parse_key_spec_alt_arrow() {
        let (mods, k) = parse_key_spec("alt+right").unwrap();
        assert!(mods.contains(ModifiersState::ALT));
        assert_eq!(k, KeyMatch::Named(NamedKey::ArrowRight));
    }

    #[test]
    fn parse_key_spec_f1_no_mods() {
        let (mods, k) = parse_key_spec("F1").unwrap();
        assert!(mods.is_empty());
        assert_eq!(k, KeyMatch::Named(NamedKey::F1));
    }

    #[test]
    fn parse_key_spec_function_keys_cover_full_range() {
        // The function-key arm parses the numeric suffix instead of
        // listing all twelve variants by hand. Pin the full F1..F12
        // contract so a regression in `parse_function_key` (e.g.
        // off-by-one on the lookup table) can't silently drop a
        // mid-range key.
        let expected = [
            ("F1", NamedKey::F1),
            ("F2", NamedKey::F2),
            ("f3", NamedKey::F3),
            ("F4", NamedKey::F4),
            ("F5", NamedKey::F5),
            ("F6", NamedKey::F6),
            ("F7", NamedKey::F7),
            ("F8", NamedKey::F8),
            ("F9", NamedKey::F9),
            // Two- and three-digit numeric suffixes are the easiest
            // place for a hand-cranked alternation to lose an entry.
            ("F10", NamedKey::F10),
            ("F11", NamedKey::F11),
            ("F12", NamedKey::F12),
        ];
        for (spec, want) in expected {
            let (_, k) = parse_key_spec(spec)
                .unwrap_or_else(|| panic!("{spec} should parse"));
            assert_eq!(k, KeyMatch::Named(want), "wrong NamedKey for {spec}");
        }
    }

    #[test]
    fn parse_key_spec_function_key_rejects_out_of_range_and_garbage() {
        // `f0` has no `NamedKey` equivalent; `f13`+ is outside the
        // shipped range. Both must fall through to the "treat as a
        // literal char" path so a user's typo doesn't silently bind
        // an arbitrary F-key.
        for spec in ["F0", "F13", "F99", "F999"] {
            let (_, k) = parse_key_spec(spec)
                .unwrap_or_else(|| panic!("{spec} should still parse as a literal"));
            // Any `Named` result here would mean the suffix parser
            // accepted an out-of-range index.
            match k {
                KeyMatch::Char(_) => {}
                KeyMatch::Named(other) => panic!(
                    "spec {spec:?} unexpectedly resolved to NamedKey {other:?}",
                ),
            }
        }
        // Single 'f' (not a function-key — the user wants the letter)
        // must still resolve as a literal char binding.
        let (_, k) = parse_key_spec("F").unwrap();
        assert_eq!(k, KeyMatch::Char("f".to_string()));
        // Non-numeric suffix (`flag`) too.
        let (_, k) = parse_key_spec("flag").unwrap();
        assert_eq!(k, KeyMatch::Char("flag".to_string()));
    }

    #[test]
    fn parse_key_spec_accepts_modifier_aliases() {
        // The bundled default.toml template documents alias spellings for
        // every modifier (Control, Option, Cmd/Meta/Win). Pin them so a
        // refactor of `parse_key_spec` can't silently drop an alias and
        // strand users whose config used the alias spelling.
        for spec in ["Control+T", "control+t"] {
            let (mods, _) = parse_key_spec(spec).unwrap_or_else(|| panic!("{spec} should parse"));
            assert!(mods.contains(ModifiersState::CONTROL), "{spec}");
        }
        for spec in ["Option+Right", "option+right"] {
            let (mods, _) = parse_key_spec(spec).unwrap_or_else(|| panic!("{spec} should parse"));
            assert!(mods.contains(ModifiersState::ALT), "{spec}");
        }
        for spec in ["Cmd+P", "Meta+P", "Win+P", "super+p"] {
            let (mods, _) = parse_key_spec(spec).unwrap_or_else(|| panic!("{spec} should parse"));
            assert!(mods.contains(ModifiersState::SUPER), "{spec}");
        }
    }

    #[test]
    fn parse_key_spec_accepts_named_key_aliases() {
        // Short forms users commonly type (or copy from other terminals)
        // must still resolve to the same NamedKey as their canonical
        // spelling. Pin the contract for the aliases the template
        // documents.
        let pairs = [
            ("Return", NamedKey::Enter),
            ("Esc", NamedKey::Escape),
            ("Del", NamedKey::Delete),
            ("Ins", NamedKey::Insert),
            ("PgUp", NamedKey::PageUp),
            ("PgDn", NamedKey::PageDown),
            ("Up", NamedKey::ArrowUp),
            ("Down", NamedKey::ArrowDown),
            ("Left", NamedKey::ArrowLeft),
            ("Right", NamedKey::ArrowRight),
        ];
        for (spec, expected) in pairs {
            let (_, k) = parse_key_spec(spec).unwrap_or_else(|| panic!("{spec} should parse"));
            assert_eq!(k, KeyMatch::Named(expected), "alias {spec} mapped wrong");
        }
    }

    #[test]
    fn parse_key_spec_rejects_modifier_only_and_empty() {
        // Plain modifier (no key glyph) → None. Otherwise a binding like
        // just `"Ctrl"` could swallow every Ctrl press.
        assert!(parse_key_spec("Ctrl").is_none());
        assert!(parse_key_spec("Ctrl+Shift").is_none());
        // Empty / whitespace-only specs are also invalid.
        assert!(parse_key_spec("").is_none());
        assert!(parse_key_spec("   ").is_none());
        // Unknown key falls back to `Char(token)` rather than failing.
        // That's by design (lets users bind to obscure chars by name).
        let (_, k) = parse_key_spec("Ctrl+5").unwrap();
        assert_eq!(k, KeyMatch::Char("5".to_string()));
    }

    #[test]
    fn trim_label_keeps_path_tail_and_truncates() {
        // Path → last segment.
        assert_eq!(trim_label("/usr/bin/bash", 14), "bash");
        // Already short → unchanged.
        assert_eq!(trim_label("zsh", 14), "zsh");
        // Too long → truncated with ellipsis at the end.
        let long = "averylongprogramname";
        let trimmed = trim_label(long, 10);
        assert_eq!(trimmed.chars().count(), 10);
        assert!(trimmed.ends_with('…'));
        // No '/' separator → uses the whole string.
        assert_eq!(trim_label("plain", 14), "plain");
        // Single-byte-per-char unicode (Cyrillic, narrow display width).
        assert_eq!(trim_label("кошка", 14), "кошка");
        let trunc_unicode = trim_label("кошкакошкакошка", 5);
        // Cyrillic is width-1; output is 4 letters + ellipsis = 5 cells.
        assert_eq!(trunc_unicode, "кошк…");
        // Wide CJK glyphs count as 2 cells each.
        // "日本語" = 6 cells. max=6 ⇒ no trim.
        assert_eq!(trim_label("日本語", 6), "日本語");
        // max=4 ⇒ keep one CJK char (2 cells) + ellipsis (1 cell) = 3.
        // Won't fit a second CJK (would push to 4+1=5 > 4-1=3).
        let trimmed = trim_label("日本語", 4);
        assert_eq!(trimmed, "日…");
    }

    #[test]
    fn tilde_code_covers_f5_through_f20() {
        // xterm assigns specific tilde codes to F5..F20 with gaps at
        // 16, 22, 27, 30 that legacy DEC bindings still claim. Pin the
        // full table so an accidental renumbering can't silently
        // misencode keypresses for users with extended keyboards.
        assert_eq!(tilde_code(NamedKey::F5), Some(15));
        assert_eq!(tilde_code(NamedKey::F12), Some(24));
        assert_eq!(tilde_code(NamedKey::F13), Some(25));
        assert_eq!(tilde_code(NamedKey::F14), Some(26));
        assert_eq!(tilde_code(NamedKey::F15), Some(28));
        assert_eq!(tilde_code(NamedKey::F16), Some(29));
        assert_eq!(tilde_code(NamedKey::F17), Some(31));
        assert_eq!(tilde_code(NamedKey::F18), Some(32));
        assert_eq!(tilde_code(NamedKey::F19), Some(33));
        assert_eq!(tilde_code(NamedKey::F20), Some(34));
        // F1–F4 use the SS3 form, not tilde — must return None here.
        assert_eq!(tilde_code(NamedKey::F1), None);
        assert_eq!(tilde_code(NamedKey::F4), None);
    }

    #[test]
    fn named_key_bytes_shift_tab_sends_cbt() {
        // Bare Tab is plain `\t`; with Shift it must become CBT
        // (`ESC[Z`) so readline/menu apps can reverse-cycle.
        let plain = named_key_bytes(NamedKey::Tab, ModifiersState::empty(), false).unwrap();
        assert_eq!(plain, b"\t".to_vec());
        let shift_tab = named_key_bytes(NamedKey::Tab, ModifiersState::SHIFT, false).unwrap();
        assert_eq!(shift_tab, b"\x1b[Z".to_vec());
        // Alt+Tab still uses the plain-key alt-prefix path
        // (`\x1b\t`) — this is only the Shift override.
        let alt_tab = named_key_bytes(NamedKey::Tab, ModifiersState::ALT, false).unwrap();
        assert_eq!(alt_tab, b"\x1b\t".to_vec());
    }

    #[test]
    fn parse_key_spec_accepts_punctuation_aliases() {
        // Real-world bindings often want `Ctrl+Plus` (zoom in) but
        // `+` is also our separator, so `Ctrl++` confuses the splitter.
        // Named aliases like `plus`, `minus`, `equal`, `comma`, `period`,
        // `slash`, `semicolon`, `colon`, `apostrophe`, `backslash`
        // expand to the literal character so the resulting KeyMatch
        // compares against the raw text events winit emits.
        let cases = [
            ("Ctrl+plus", "+"),
            ("Ctrl+minus", "-"),
            ("Ctrl+dash", "-"),
            ("Ctrl+equal", "="),
            ("Ctrl+eq", "="),
            ("Ctrl+comma", ","),
            ("Ctrl+period", "."),
            ("Ctrl+dot", "."),
            ("Ctrl+slash", "/"),
            ("Ctrl+backslash", "\\"),
            ("Ctrl+semicolon", ";"),
            ("Ctrl+colon", ":"),
            ("Ctrl+apostrophe", "'"),
            ("Ctrl+quote", "'"),
        ];
        for (spec, expected) in cases {
            let (_mods, k) = parse_key_spec(spec).unwrap_or_else(|| {
                panic!("spec {spec:?} should parse");
            });
            assert_eq!(
                k,
                KeyMatch::Char(expected.to_string()),
                "spec {spec:?} should map to char {expected:?}",
            );
        }
        // Unknown words still fall through as a Char with their text —
        // preserves the historical "anything not named is a literal"
        // behaviour for plugins binding obscure keys.
        let (_mods, k) = parse_key_spec("Ctrl+xyz").unwrap();
        assert_eq!(k, KeyMatch::Char("xyz".to_string()));
    }

    #[test]
    fn user_binding_from_config() {
        let b = UserBinding::from_config("Ctrl+Shift+T", "new_tab").unwrap();
        assert!(b.mods.contains(ModifiersState::CONTROL | ModifiersState::SHIFT));
        assert!(matches!(b.action, AppAction::NewTab));
        // Spec + action_name are preserved verbatim from config so the
        // help overlay can show what the user actually wrote.
        assert_eq!(b.spec, "Ctrl+Shift+T");
        assert_eq!(b.action_name(), "new_tab");
        // Unknown action returns None.
        assert!(UserBinding::from_config("Ctrl+X", "this_action_does_not_exist").is_none());
    }

    #[test]
    fn norm_selection_contains_basic() {
        let s = NormSelection { start: pt(1, 2), end: pt(3, 5), block: false };
        // First row: cells from col 2 onward.
        assert!(!s.contains(1, 1));
        assert!(s.contains(1, 2));
        assert!(s.contains(1, 80)); // open-ended on first/intermediate rows
        // Middle row: all cells.
        assert!(s.contains(2, 0));
        assert!(s.contains(2, 100));
        // Last row: up to but not including end.col.
        assert!(s.contains(3, 4));
        assert!(!s.contains(3, 5));
        // Outside row range.
        assert!(!s.contains(0, 2));
        assert!(!s.contains(4, 0));
    }

    #[test]
    fn norm_selection_single_row() {
        let s = NormSelection { start: pt(2, 3), end: pt(2, 7), block: false };
        assert!(!s.contains(2, 2));
        assert!(s.contains(2, 3));
        assert!(s.contains(2, 6));
        assert!(!s.contains(2, 7));
    }

    #[test]
    fn norm_selection_block_contains_rect_only() {
        // Block selection: rect bounds, NOT linear. Cells inside the
        // rect on every row are selected; cells outside the column
        // range are NOT, even on intermediate rows (the linear mode's
        // "open-ended" rule does not apply).
        let s = NormSelection {
            start: pt(1, 3),
            end: pt(4, 7), // exclusive — selected cols are 3..7 → 3,4,5,6
            block: true,
        };
        // Top-left corner of the rect.
        assert!(s.contains(1, 3));
        // Bottom-right INSIDE the inclusive range.
        assert!(s.contains(4, 6));
        // Just past the right edge — NOT selected.
        assert!(!s.contains(4, 7));
        // Just before the left edge — NOT selected on any row.
        assert!(!s.contains(2, 2));
        // Inside the rect on the middle row.
        assert!(s.contains(2, 5));
        // Way to the right on a middle row would be selected in
        // *linear* mode but must NOT be in block mode.
        assert!(!s.contains(2, 100));
        // Outside the row range.
        assert!(!s.contains(0, 5));
        assert!(!s.contains(5, 5));
    }

    /// Render-state helper for ActiveSelection tests: rows=100,
    /// sb_len=0, offset=0 — that pins `abs_row == viewport_row`,
    /// so the existing assertions against `SelectionPoint { row, col }`
    /// stay meaningful without touching every literal.
    fn norm(s: &ActiveSelection) -> NormSelection {
        s.to_visible_norm(0, 0, 100).expect("selection in viewport")
    }

    #[test]
    fn active_selection_block_normalizes_rect_corners() {
        // Anchor + focus pinned diagonally; the resulting rect must
        // be `[min_row..=max_row] × [min_col..=max_col]` regardless
        // of which corner the user anchored on. Catches the "user
        // dragged up-and-to-the-left" case where naive endpoint
        // swap on the (row, col) lexicographic tuple would produce a
        // diagonal-shaped selection.
        let s = ActiveSelection {
            pane_idx: 0,
            anchor: ap(5, 10),
            focus: ap(2, 3),
            mode: SelectionMode::Block,
            pivot: None,
        };
        let n = norm(&s);
        assert!(n.block);
        assert_eq!(n.start, pt(2, 3));
        // end.col is exclusive (= max_col + 1).
        assert_eq!(n.end, pt(5, 11));
        // Sanity-check via contains: corners and interior cells.
        assert!(n.contains(2, 3));
        assert!(n.contains(5, 10));
        assert!(!n.contains(5, 11));
        assert!(n.contains(3, 7));
        // Outside the rect on the same row range.
        assert!(!n.contains(3, 2));
        assert!(!n.contains(3, 100));
    }

    #[test]
    fn active_selection_normalizes_swapped_endpoints() {
        let s = ActiveSelection {
            pane_idx: 0,
            anchor: ap(5, 3),
            focus: ap(2, 10),
            mode: SelectionMode::Char,
            pivot: None,
        };
        let n = norm(&s);
        assert_eq!(n.start, pt(2, 10));
        // end.col is exclusive: anchor col 3 is the inclusive max, so
        // the normalised end bumps to 4 to keep cell (5,3) selected.
        assert_eq!(n.end, pt(5, 4));
    }

    /// Both endpoints of a single-row linear selection must stay
    /// highlighted regardless of drag direction. Regression for the
    /// off-by-one where the inclusive max cell was dropped:
    ///   - drag LEFT lost the originally-clicked (anchor) cell;
    ///   - a double-click word lost its last glyph.
    #[test]
    fn active_selection_linear_includes_both_endpoints() {
        // Drag RIGHT: pressed col 2, swept to col 5.
        let right = ActiveSelection {
            pane_idx: 0,
            anchor: ap(7, 2),
            focus: ap(7, 5),
            mode: SelectionMode::Char,
            pivot: None,
        };
        let n = norm(&right);
        assert!(n.contains(7, 2), "anchor cell included");
        assert!(n.contains(7, 5), "cell under cursor included");
        assert!(!n.contains(7, 6), "one past the focus excluded");

        // Drag LEFT: pressed col 5, swept to col 2 — the clicked
        // (anchor) cell at col 5 used to fall outside the highlight.
        let left = ActiveSelection {
            pane_idx: 0,
            anchor: ap(7, 5),
            focus: ap(7, 2),
            mode: SelectionMode::Char,
            pivot: None,
        };
        let n = norm(&left);
        assert!(n.contains(7, 2), "cell under cursor included");
        assert!(n.contains(7, 5), "originally-clicked cell included");
        assert!(!n.contains(7, 6), "one past the clicked cell excluded");

        // Double-click word "hello" at cols 0..=4 lands as anchor=0,
        // focus=4 (the inclusive last glyph). The 'o' must highlight.
        let word = ActiveSelection {
            pane_idx: 0,
            anchor: ap(3, 0),
            focus: ap(3, 4),
            mode: SelectionMode::Word,
            pivot: Some((ap(3, 0), ap(3, 4))),
        };
        let n = norm(&word);
        assert!(n.contains(3, 4), "last glyph of the word included");
        assert!(!n.contains(3, 5), "delimiter after the word excluded");
    }

    /// Selection scrolled fully above the viewport returns None
    /// (renderer skips the highlight pass for this pane).
    #[test]
    fn selection_scrolled_above_viewport_yields_none() {
        let s = ActiveSelection {
            pane_idx: 0,
            anchor: ap(2, 0),
            focus: ap(4, 5),
            mode: SelectionMode::Char,
            pivot: None,
        };
        // sb_len = 10, offset = 0 → abs_row 2..4 maps to viewport
        // row -8..=-6, fully above the visible window.
        assert!(s.to_visible_norm(10, 0, 24).is_none());
    }

    /// Same selection becomes visible once the user scrolls into
    /// scrollback — proving the wheel-scroll anchoring works.
    #[test]
    fn selection_visible_after_scroll_into_scrollback() {
        let s = ActiveSelection {
            pane_idx: 0,
            anchor: ap(2, 0),
            focus: ap(4, 5),
            mode: SelectionMode::Char,
            pivot: None,
        };
        // sb_len = 10, offset = 8 → abs_row 2 == viewport row 0,
        // abs_row 4 == viewport row 2.
        let n = s.to_visible_norm(10, 8, 24).expect("visible");
        assert_eq!(n.start, pt(0, 0));
        // Focus col 5 is the inclusive max; exclusive end is 6.
        assert_eq!(n.end, pt(2, 6));
    }

    #[test]
    fn snap_word_drag_forward_uses_pivot_start_and_drag_end() {
        // Pivot word "hello" at row 5, cols 0..=4.
        // User drags right onto word "world" at cols 6..=10.
        let pivot = (ap(5, 0), ap(5, 4));
        let (anchor, focus) =
            snap_drag_to_range(pivot, ap(5, 6), ap(5, 10), false);
        assert_eq!(anchor, ap(5, 0));
        assert_eq!(focus, ap(5, 10));
        // Normalized: cells 0..10 inclusive (covers both words).
        let active = ActiveSelection {
            pane_idx: 0,
            anchor,
            focus,
            mode: SelectionMode::Word,
            pivot: Some(pivot),
        };
        let n = norm(&active);
        assert_eq!(n.start, pt(5, 0));
        // Inclusive max col 10 → exclusive end 11, so "world" keeps its
        // final glyph highlighted.
        assert_eq!(n.end, pt(5, 11));
    }

    #[test]
    fn snap_word_drag_backward_swaps_anchor_to_pivot_end() {
        // Pivot word at cols 6..=10. Drag left onto word at cols 0..=4.
        let pivot = (ap(5, 6), ap(5, 10));
        let (anchor, focus) =
            snap_drag_to_range(pivot, ap(5, 0), ap(5, 4), false);
        assert_eq!(anchor, ap(5, 10));
        assert_eq!(focus, ap(5, 0));
        // Normalized covers the union of both words.
        let active = ActiveSelection {
            pane_idx: 0,
            anchor,
            focus,
            mode: SelectionMode::Word,
            pivot: Some(pivot),
        };
        let n = norm(&active);
        assert_eq!(n.start, pt(5, 0));
        // Inclusive max col 10 → exclusive end 11.
        assert_eq!(n.end, pt(5, 11));
    }

    #[test]
    fn snap_line_drag_forward_extends_to_drag_row() {
        // Pivot is line 3 cols 0..=20. Drag down to line 7 cols 0..=15.
        let pivot = (ap(3, 0), ap(3, 20));
        let (anchor, focus) =
            snap_drag_to_range(pivot, ap(7, 0), ap(7, 15), true);
        assert_eq!(anchor, ap(3, 0));
        assert_eq!(focus, ap(7, 15));
    }

    #[test]
    fn snap_line_drag_within_same_row_does_not_flip() {
        // Drag column changes within pivot's row must NOT flip direction.
        let pivot = (ap(3, 0), ap(3, 20));
        let (anchor, focus) =
            snap_drag_to_range(pivot, ap(3, 0), ap(3, 5), true);
        // row_only forward path is taken because drag.row == pivot.row.
        assert_eq!(anchor, ap(3, 0));
        assert_eq!(focus, ap(3, 5));
    }

    #[test]
    fn flash_clear_color_is_neutral_and_preserves_alpha() {
        // From a dark, semi-transparent base the bell flash lifts all
        // three channels by the SAME amount (neutral — no warm cast)
        // and keeps the alpha so a transparent window stays transparent.
        let base = wgpu::Color { r: 0.02, g: 0.03, b: 0.04, a: 0.8 };
        let f = flash_clear_color(base, 1.0);
        let dr = f.r - base.r;
        let dg = f.g - base.g;
        let db = f.b - base.b;
        assert!(dr > 0.0, "flash brightens the screen");
        assert!((dr - dg).abs() < 1e-9 && (dg - db).abs() < 1e-9, "equal lift → no colour cast");
        assert_eq!(f.a, base.a, "configured opacity preserved during flash");
        // Channels saturate at 1.0 — no overflow past white. Inputs sit
        // within FLASH_LIFT of white so a full-intensity flash pushes
        // every channel to exactly 1.0 regardless of the tuned lift.
        let bright = wgpu::Color { r: 0.9995, g: 1.0, b: 0.9998, a: 1.0 };
        let fb = flash_clear_color(bright, 1.0);
        assert_eq!((fb.r, fb.g, fb.b), (1.0, 1.0, 1.0));
    }

    #[test]
    fn flash_clear_color_fades_with_intensity() {
        // The fade is monotonic: lower intensity → smaller lift, and the
        // tail eases (quadratic) so half-intensity is well under half the
        // peak lift. Zero intensity must be a no-op (no residual tint).
        let base = wgpu::Color { r: 0.1, g: 0.1, b: 0.1, a: 1.0 };
        let full = flash_clear_color(base, 1.0).r - base.r;
        let half = flash_clear_color(base, 0.5).r - base.r;
        let zero = flash_clear_color(base, 0.0).r - base.r;
        assert!(full > half && half > 0.0, "lift shrinks with intensity");
        assert!(half < full * 0.5, "quadratic easing softens the tail");
        assert_eq!(zero, 0.0, "intensity 0 leaves the base colour untouched");
        // Out-of-range inputs clamp instead of over-lifting.
        let over = flash_clear_color(base, 5.0).r - base.r;
        assert!((over - full).abs() < 1e-9, "intensity clamps at 1.0");
    }

    #[test]
    fn pick_alpha_mode_prefers_transparent_below_full_opacity() {
        use wgpu::CompositeAlphaMode::*;
        // opacity < 1.0 → take a pre/post-multiplied mode when offered.
        assert_eq!(pick_alpha_mode(0.9, &[Opaque, PreMultiplied]), PreMultiplied);
        assert_eq!(pick_alpha_mode(0.85, &[Opaque, PostMultiplied]), PostMultiplied);
        // opacity < 1.0 but only Opaque available → fall back to it, and
        // it reports as non-transparent so the caller's warning fires.
        let only_opaque = pick_alpha_mode(0.9, &[Opaque]);
        assert_eq!(only_opaque, Opaque);
        assert!(!alpha_mode_is_transparent(only_opaque));
        assert!(alpha_mode_is_transparent(PreMultiplied));
        // Full opacity → platform default (first), never overridden.
        assert_eq!(pick_alpha_mode(1.0, &[Opaque, PreMultiplied]), Opaque);
    }

    #[test]
    fn clamp_scroll_offset_saturates_past_u16() {
        // Scrollback can exceed 65 535 lines; a bare `as u16` wrapped
        // (100_000 → 34_464), flinging the viewport to a wrong line.
        assert_eq!(clamp_scroll_offset(0), 0);
        assert_eq!(clamp_scroll_offset(500), 500);
        assert_eq!(clamp_scroll_offset(u16::MAX as i64), u16::MAX);
        assert_eq!(clamp_scroll_offset(100_000), u16::MAX);
        // Negatives (shouldn't occur, but the math is signed) clamp to 0.
        assert_eq!(clamp_scroll_offset(-5), 0);
    }

    #[test]
    fn font_reset_matches_physical_zero_only() {
        // The reset chord keys off the PHYSICAL `0` so it's layout-proof.
        assert!(is_font_reset_key(&PhysicalKey::Code(KeyCode::Digit0)));
        // Neither other digits, the numpad zero, nor the zoom keys reset.
        assert!(!is_font_reset_key(&PhysicalKey::Code(KeyCode::Digit1)));
        assert!(!is_font_reset_key(&PhysicalKey::Code(KeyCode::Numpad0)));
        assert!(!is_font_reset_key(&PhysicalKey::Code(KeyCode::Equal)));
        assert!(!is_font_reset_key(&PhysicalKey::Code(KeyCode::Minus)));
    }

    #[test]
    fn alt_tab_is_recognised_as_window_switch_chord() {
        let tab = Key::Named(NamedKey::Tab);
        // Alt+Tab → swallow (shift is irrelevant: Alt+Shift+Tab too).
        assert!(is_window_switch_chord(&tab, true, false));
        // Plain Tab must still reach the PTY (forward completion).
        assert!(!is_window_switch_chord(&tab, false, false));
        // Ctrl+Alt+Tab is not the OS switcher — don't capture it here.
        assert!(!is_window_switch_chord(&tab, true, true));
        // Alt + a non-Tab key is not the switch chord.
        assert!(!is_window_switch_chord(&Key::Named(NamedKey::Enter), true, false));
        assert!(!is_window_switch_chord(&Key::Character("a".into()), true, false));
    }

    #[test]
    fn bare_modifier_keys_are_recognised() {
        assert!(is_bare_modifier_key(&Key::Named(NamedKey::Control)));
        assert!(is_bare_modifier_key(&Key::Named(NamedKey::Shift)));
        assert!(is_bare_modifier_key(&Key::Named(NamedKey::Alt)));
        assert!(is_bare_modifier_key(&Key::Named(NamedKey::Super)));
        assert!(is_bare_modifier_key(&Key::Named(NamedKey::AltGraph)));
        assert!(is_bare_modifier_key(&Key::Named(NamedKey::CapsLock)));
        // Regular keys are not bare modifiers.
        assert!(!is_bare_modifier_key(&Key::Named(NamedKey::Enter)));
        assert!(!is_bare_modifier_key(&Key::Named(NamedKey::Tab)));
        assert!(!is_bare_modifier_key(&Key::Character("a".into())));
    }

    #[test]
    fn shifted_digit_maps_back_to_digit() {
        assert_eq!(shifted_digit_to_digit("!"), Some('1'));
        assert_eq!(shifted_digit_to_digit("@"), Some('2'));
        assert_eq!(shifted_digit_to_digit(")"), Some('0'));
        assert_eq!(shifted_digit_to_digit("a"), None);
        assert_eq!(shifted_digit_to_digit(""), None);
    }

    #[test]
    fn fuzzy_score_matches_subsequence() {
        // "nt" matches "new tab" because 'n' and 't' appear in order.
        assert!(fuzzy_score("new tab", "nt").is_some());
        // No 'x' in "new tab".
        assert!(fuzzy_score("new tab", "nx").is_none());
        // Empty needle always scores zero.
        assert_eq!(fuzzy_score("anything", ""), Some(0));
    }

    #[test]
    fn fuzzy_score_prefix_beats_mid_match() {
        let prefix = fuzzy_score("new tab", "new").unwrap();
        let mid = fuzzy_score("renew window", "new").unwrap();
        assert!(prefix > mid, "prefix={prefix} mid={mid}");
    }

    #[test]
    fn fuzzy_score_substring_beats_fuzzy_subsequence() {
        // Substring "tab" in "next tab" beats fuzzy "t…a…b" hits in
        // unrelated labels.
        let substring = fuzzy_score("next tab", "tab").unwrap();
        let fuzzy = fuzzy_score("toggle a button", "tab").unwrap();
        assert!(substring > fuzzy, "substring={substring} fuzzy={fuzzy}");
    }

    #[test]
    fn drain_osc52_applies_focused_and_drains_all() {
        // Two panes both queue an OSC 52 write. Only the focused pane's
        // payload is returned; the background pane's is dropped — AND
        // both terminals are drained (so nothing accumulates to flush
        // stale later).
        let mk = |b64: &str| {
            let t = Arc::new(Mutex::new(Terminal::new(rterm_core::Size {
                cols: 8,
                rows: 1,
            })));
            t.lock()
                .unwrap()
                .advance(format!("\x1b]52;c;{b64}\x07").as_bytes());
            t
        };
        let focused = mk("Rm9v"); // "Foo"
        let background = mk("QmFy"); // "Bar"
        let panes = vec![(1u64, &focused), (2u64, &background)];
        let out = drain_osc52(panes.into_iter(), Some(1));
        assert_eq!(out.as_deref(), Some("Rm9v"), "focused pane's write applied");
        // Both drained: a second drain yields nothing from either.
        assert!(focused.lock().unwrap().take_pending_clipboard().is_none());
        assert!(background.lock().unwrap().take_pending_clipboard().is_none());
    }

    #[test]
    fn build_spans_highlights_only_default_fg_cells() {
        let _guard = highlight::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Install a rule that colours "ERROR" red, then render a grid
        // where the word appears once in plain (default fg) text and
        // once already coloured green by SGR. The plain one must pick
        // up the highlight; the SGR-coloured one must stay green.
        highlight::set_rules(
            true,
            false,
            vec![highlight::HighlightRuleInput {
                pattern: r"ERROR".to_string(),
                fg: [200, 40, 40],
                bold: false,
            }],
        );
        let mut term = Terminal::new(rterm_core::Size { cols: 24, rows: 2 });
        // Row 0: plain "ERROR". Row 1: SGR green "ERROR" (\x1b[32m).
        term.advance(b"ERROR\r\n\x1b[32mERROR\x1b[0m");
        let spans = build_spans(&term, 0, false, false, None);
        // Concatenate the text and find which style each "ERROR" got.
        // The plain-row span carrying "ERROR" should be the rule red;
        // the SGR row's should be green (~ [152,195,121]-ish is the
        // palette green; just assert it's NOT the rule red).
        let red = [200, 40, 40];
        let plain_red = spans
            .iter()
            .any(|(t, k)| t.contains("ERROR") && k.fg == red);
        let sgr_kept = spans
            .iter()
            .any(|(t, k)| t.contains("ERROR") && k.fg != red);
        assert!(plain_red, "plain default-fg ERROR should be highlighted red");
        assert!(sgr_kept, "SGR-coloured ERROR must NOT be overridden");
        // Clean up the global so other tests see no rules.
        highlight::set_rules(false, false, vec![]);
    }

    fn test_pane(title: &str) -> Pane {
        struct StubIo;
        impl TerminalIo for StubIo {
            fn write_input(&self, _: &[u8]) {}
            fn resize(&self, _: u16, _: u16) {}
        }
        let term = Arc::new(Mutex::new(Terminal::new(rterm_core::Size {
            cols: 10,
            rows: 4,
        })));
        Pane::new(
            term,
            Arc::new(StubIo),
            title,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
            Box::new(()),
            None, // tests don't need a history store
        )
    }

    #[test]
    fn display_title_prefers_osc_dynamic_title_over_everything() {
        // The shell sent OSC 0/2 → its value wins over any process-name
        // fallback. This is the highest-priority source of truth: it's
        // what the user (or their shell prompt) explicitly chose to show.
        let pane = test_pane("bash");
        *pane.dynamic_title.lock().unwrap() = Some("custom".to_string());
        *pane.last_foreground_process.lock().unwrap() = Some("vim".to_string());
        assert_eq!(pane.display_title(), "custom");
    }

    #[test]
    fn display_title_falls_back_to_foreground_process_when_no_osc() {
        // No OSC title set → use the cached foreground-process name. This
        // is the new behaviour: tab labels follow `vim` / `htop` without
        // requiring the shell to emit an OSC 0/1/2 on each fg swap.
        let pane = test_pane("bash");
        *pane.last_foreground_process.lock().unwrap() = Some("vim".to_string());
        assert_eq!(pane.display_title(), "vim");
    }

    #[test]
    fn display_title_skips_foreground_when_equal_to_shell() {
        // When the foreground process is just the shell itself (the
        // common case while sitting at a prompt), promoting it would
        // change nothing visible AND would suppress whatever static
        // fallback was richer. So if `last_foreground_process` matches
        // the static title, ignore it.
        let pane = test_pane("bash");
        *pane.last_foreground_process.lock().unwrap() = Some("bash".to_string());
        assert_eq!(pane.display_title(), "bash");
    }

    #[test]
    fn display_title_static_when_nothing_else_set() {
        // Initial state: no OSC, no foreground info yet. Tab label uses
        // the static fallback (the shell program name from spawn).
        let pane = test_pane("zsh");
        assert_eq!(pane.display_title(), "zsh");
    }

    fn test_tab(tree: tree::Tree<Pane>, focus_path: tree::TreePath) -> Tab {
        Tab {
            tree,
            focus_path,
            zoomed: false,
            custom_title: None,
            unread: false,
            silence_armed: false,
            last_any_alt: AtomicBool::new(false),
            last_progress: Mutex::new(None),
        }
    }

    #[test]
    fn focus_repair_survives_sibling_subtree_hoist() {
        // Regression: topology Split{ Split{p1, p2}, p3 }, focus on p2
        // (path [false,true]), p3 dies. close_leaf hoists Split{p1,p2}
        // to the root, shortening p2's true path to [true] — the old
        // prefix-based repair left focus_path as [false,true], which
        // addresses PAST a leaf, so focused_pane() returned None and
        // the keyboard went dead until a click.
        let p1 = test_pane("p1");
        let p2 = test_pane("p2");
        let p3 = test_pane("p3");
        let p2_uid = p2.uid;
        let mut tree = tree::Tree::new(p1);
        assert!(tree.split_leaf(&[], p3, SplitDir::Horizontal, 0.5));
        assert!(tree.split_leaf(&[false], p2, SplitDir::Vertical, 0.5));
        let mut tab = test_tab(tree, vec![false, true]);
        assert_eq!(tab.focused_pane().map(|p| p.uid), Some(p2_uid));

        // p3 (path [true]) dies and is pruned.
        let focused_uid = tab.focused_pane().map(|p| p.uid);
        assert!(tab.tree.close_leaf(&[true]));
        tab.repair_focus_after_close(&[true], focused_uid);

        assert_eq!(
            tab.focused_pane().map(|p| p.uid),
            Some(p2_uid),
            "focus must follow p2 through the subtree hoist"
        );
        assert_eq!(tab.focus_path, vec![true], "p2's path lost one element");
    }

    #[test]
    fn focus_repair_falls_back_when_focused_pane_was_removed() {
        // Same topology, but the FOCUSED pane (p3) is the one that
        // died — its uid is gone from the tree, so focus falls to the
        // leftmost survivor under the removed leaf's parent.
        let p1 = test_pane("p1");
        let p2 = test_pane("p2");
        let p3 = test_pane("p3");
        let p1_uid = p1.uid;
        let p3_uid = p3.uid;
        let mut tree = tree::Tree::new(p1);
        assert!(tree.split_leaf(&[], p3, SplitDir::Horizontal, 0.5));
        assert!(tree.split_leaf(&[false], p2, SplitDir::Vertical, 0.5));
        let mut tab = test_tab(tree, vec![true]);
        assert_eq!(tab.focused_pane().map(|p| p.uid), Some(p3_uid));

        let focused_uid = tab.focused_pane().map(|p| p.uid);
        assert!(tab.tree.close_leaf(&[true]));
        tab.repair_focus_after_close(&[true], focused_uid);

        assert_eq!(
            tab.focused_pane().map(|p| p.uid),
            Some(p1_uid),
            "focus falls to the leftmost survivor"
        );
    }

    #[test]
    fn tab_aggregate_progress_picks_most_severe_pane() {
        // Build a tab with three panes carrying different progress
        // states. The header badge aggregator should pick the error
        // pane (state 2 → severity 4), regardless of percent values.
        // This pins the same iter+max_by_key logic that both
        // `header_spans` and the snapshot loop use.
        let p_a = test_pane("a");
        let p_b = test_pane("b");
        let p_c = test_pane("c");
        *p_a.progress.lock().unwrap() = Some((1, 90)); // set 90%
        *p_b.progress.lock().unwrap() = Some((2, 0));  // error
        *p_c.progress.lock().unwrap() = Some((4, 0));  // warn
        let mut tree: tree::Tree<Pane> = tree::Tree::new(p_a);
        tree.split_leaf(&[], p_b, SplitDir::Horizontal, 0.5);
        tree.split_leaf(&[true], p_c, SplitDir::Vertical, 0.5);
        let aggregated = tree
            .leaves()
            .iter()
            .filter_map(|p| p.progress.lock().ok().and_then(|g| *g))
            .max_by_key(|(s, p)| (progress_severity(*s), *p));
        assert_eq!(aggregated, Some((2, 0)));
    }

    #[test]
    fn pane_command_finish_payload_carries_duration_or_empty() {
        // Format: `<tab>:<pane>\t<exit>\t<ms>\t<uid>`. Missing duration
        // becomes an empty middle field — `split('\t')` still yields
        // four columns so plugins reading by index don't shift, and
        // treating empty as "unknown" prevents misattributing a 0ms
        // duration to a command that may have taken longer.
        assert_eq!(
            pane_command_finish_payload(1, 2, 0, Some(150), 42),
            "1:2\t0\t150\t42",
        );
        assert_eq!(
            pane_command_finish_payload(3, 1, 127, None, 7),
            "3:1\t127\t\t7",
        );
        // Negative exit code (e.g. SIGKILL-ish from shells) round-trips
        // through the formatter without truncation.
        assert_eq!(
            pane_command_finish_payload(2, 2, -9, Some(0), 13),
            "2:2\t-9\t0\t13",
        );
    }

    #[test]
    fn pane_exit_payload_includes_exit_code_and_uid() {
        // Format: `<tab>:<pane>\t<code_or_empty>\t<uid>`. Always three
        // tab-separated fields after the colon-separated prefix; empty
        // middle column means "no shell-integration finish observed".
        // uid lets plugins match the event to a previously-stashed
        // identifier; the colon prefix still parses with the legacy
        // `(%d+):(%d+)` Lua matcher.
        assert_eq!(
            pane_exit_payload(2, 3, Some(0), 42),
            "2:3\t0\t42",
        );
        assert_eq!(
            pane_exit_payload(1, 1, Some(-1), 7),
            "1:1\t-1\t7",
        );
        // No exit code → empty middle column, uid still present.
        assert_eq!(
            pane_exit_payload(7, 4, None, 99),
            "7:4\t\t99",
        );
        // Legacy parse still works.
        let payload = pane_exit_payload(5, 9, Some(2), 13);
        let (tab_str, rest) = payload.split_once(':').unwrap();
        let pane_str = rest.split_once('\t').map(|(p, _)| p).unwrap_or(rest);
        assert_eq!(tab_str, "5");
        assert_eq!(pane_str, "9");
    }

    #[test]
    fn pane_split_payload_appends_direction_tag_and_uid() {
        // Format: `<tab>:<pane>\t<h|v>\t<uid>`. Both trailing fields are
        // additive so legacy `<tab>:<pane>`-only parsers keep working.
        assert_eq!(
            pane_split_payload(1, 2, SplitDir::Horizontal, 100),
            "1:2\th\t100",
        );
        assert_eq!(
            pane_split_payload(3, 5, SplitDir::Vertical, 42),
            "3:5\tv\t42",
        );
        // Legacy parse still works on the enriched payload.
        let payload = pane_split_payload(7, 9, SplitDir::Horizontal, 200);
        let (tab_str, rest) = payload.split_once(':').unwrap();
        let pane_str = rest.split_once('\t').map(|(p, _)| p).unwrap_or(rest);
        assert_eq!(tab_str, "7");
        assert_eq!(pane_str, "9");
        // Full tab-split parse yields exactly four fields.
        let fields: Vec<&str> = payload.split([':', '\t']).collect();
        assert_eq!(fields, vec!["7", "9", "h", "200"]);
    }

    #[test]
    fn pane_value_uid_payload_keeps_value_before_uid_across_types() {
        // The historical wire format for `pane.bell_mute` and
        // `pane.shell_exit` has `<value>` BEFORE `<uid>` — opposite
        // of `pane_attr_payload`. The helper is generic over
        // `Display`; pin both a bool (bell_mute) and an integer
        // (shell exit code) variant so a future "uid-suffix
        // everywhere" refactor doesn't silently flip them.
        assert_eq!(
            pane_value_uid_payload(1, 2, true, 100),
            "1:2\ttrue\t100",
        );
        assert_eq!(
            pane_value_uid_payload(3, 5, false, 42),
            "3:5\tfalse\t42",
        );
        // Negative exit code — i32 path.
        assert_eq!(
            pane_value_uid_payload(2, 4, -1i32, 7),
            "2:4\t-1\t7",
        );
        let p = pane_value_uid_payload(7, 9, 130i32, 200);
        let fields: Vec<&str> = p.split([':', '\t']).collect();
        assert_eq!(fields, vec!["7", "9", "130", "200"]);
    }

    #[test]
    fn progress_payload_matches_legacy_format_and_clamps_high_values() {
        // Shape: `<tab+1>:<pane+1>\t<state>\t<percent>`. State 0
        // means "clear". The historic format is `(%d+):(%d+)\t…`
        // so legacy parsers must still recover tab/pane via that
        // shape.
        assert_eq!(progress_payload(1, 2, 1, 42), "1:2\t1\t42");
        assert_eq!(progress_payload(3, 5, 0, 0), "3:5\t0\t0");
        // Even pathological 255/255 stay byte-for-byte stable —
        // catches a future change to e.g. `{:03}` formatting.
        assert_eq!(progress_payload(7, 9, 255, 255), "7:9\t255\t255");
        let p = progress_payload(2, 4, 2, 100);
        let fields: Vec<&str> = p.split([':', '\t']).collect();
        assert_eq!(fields, vec!["2", "4", "2", "100"]);
    }

    #[test]
    fn half_page_rows_floors_to_one_and_rounds_down_otherwise() {
        // The vim-style ⌈page/2⌉ rule must keep `scroll_half_page_*`
        // moving at least one row, even on a tiny / unknown page.
        // Pin the boundary so a future refactor can't accidentally
        // produce a `0` (which would no-op the action and look like
        // a bug to the user).
        assert_eq!(half_page_rows(0), 1, "unknown page → 1-row safety");
        assert_eq!(half_page_rows(-5), 1, "negative page → 1-row safety");
        assert_eq!(half_page_rows(1), 1, "1-row pane → 1 row");
        assert_eq!(half_page_rows(2), 1, "2-row pane → 1 row");
        assert_eq!(half_page_rows(3), 1, "3-row pane → 1 row");
        assert_eq!(half_page_rows(4), 2);
        assert_eq!(half_page_rows(10), 5);
        assert_eq!(half_page_rows(31), 15, "odd page rounds down");
        assert_eq!(half_page_rows(80), 40);
    }

    #[test]
    fn tab_drag_end_payload_encodes_outcome_bool() {
        // Shape: `<src+1>\t<true|false>`. Pin both outcomes so
        // a future "let me just emit the bool unformatted" diff
        // can't drop the field separator.
        assert_eq!(tab_drag_end_payload(1, true), "1\ttrue");
        assert_eq!(tab_drag_end_payload(3, false), "3\tfalse");
        let p = tab_drag_end_payload(7, true);
        let parts: Vec<&str> = p.split('\t').collect();
        assert_eq!(parts, vec!["7", "true"]);
    }

    #[test]
    fn tab_progress_payload_distinguishes_clear_from_zero_pct_set() {
        // Shape: `<tab+1>\t<state>\t<pct>`. State 0 / pct 0 is the
        // "cleared" sentinel that fires when the aggregate goes
        // from Some to None. State 1 / pct 0 is a legitimate "just
        // started" reading — the two must NOT collide as bytes.
        // They don't, because state 1 ≠ 0; pin it here so a future
        // helper rewrite can't accidentally drop the state byte.
        assert_eq!(tab_progress_payload(1, 0, 0), "1\t0\t0");
        assert_eq!(tab_progress_payload(1, 1, 0), "1\t1\t0");
        assert_eq!(tab_progress_payload(3, 2, 42), "3\t2\t42");
        assert_eq!(tab_progress_payload(7, 4, 100), "7\t4\t100");
        let p = tab_progress_payload(5, 3, 50);
        let fields: Vec<&str> = p.split('\t').collect();
        assert_eq!(fields, vec!["5", "3", "50"]);
    }

    #[test]
    fn tab_title_payload_passes_through_unicode_and_empty() {
        // Shape: `<tab+1>\t<title>`. The trailing field is the
        // free-form title — must preserve unicode, special chars,
        // and the empty string (which signals a cleared override).
        assert_eq!(tab_title_payload(1, "main"), "1\tmain");
        assert_eq!(
            tab_title_payload(3, "build — debug"),
            "3\tbuild — debug",
        );
        // Empty title → trailing tab with nothing after.
        assert_eq!(tab_title_payload(5, ""), "5\t");
    }

    #[test]
    fn tab_event_payload_format_is_tab_then_uid() {
        // Shape: `<tab+1>\t<uid>`. Shared by `tab.switch`,
        // `tab.alt_enter`, `tab.alt_leave`. Pin the byte order so a
        // future field-swap doesn't silently corrupt subscriber
        // payloads.
        assert_eq!(tab_event_payload(1, 100), "1\t100");
        assert_eq!(tab_event_payload(7, 0), "7\t0");
        let p = tab_event_payload(3, 42);
        let mut parts = p.split('\t');
        assert_eq!(parts.next(), Some("3"));
        assert_eq!(parts.next(), Some("42"));
        assert_eq!(parts.next(), None);
    }

    #[test]
    fn pane_text_payload_preserves_unicode_and_spaces() {
        // Shape: `<tab>:<pane>\t<text>`. Used by `pane.title` and
        // `pane.cwd`. The trailing field can legitimately contain
        // spaces, slashes, and UTF-8 — the helper must pass them
        // through unchanged; only the leading three control
        // fragments are structured.
        assert_eq!(
            pane_text_payload(1, 2, "/home/user"),
            "1:2\t/home/user",
        );
        assert_eq!(
            pane_text_payload(3, 5, "vim — main.rs"),
            "3:5\tvim — main.rs",
        );
        // Empty string is valid (a cleared title).
        assert_eq!(pane_text_payload(1, 1, ""), "1:1\t");
        // Legacy `(%d+):(%d+)` parse still works.
        let p = pane_text_payload(7, 9, "weird/path with;semi");
        let (tab_str, rest) = p.split_once(':').unwrap();
        let (pane_str, _) = rest.split_once('\t').unwrap();
        assert_eq!(tab_str, "7");
        assert_eq!(pane_str, "9");
    }

    #[test]
    fn pane_edge_payload_is_legacy_compatible() {
        // 3-field shape `<tab>:<pane>\t<uid>` used by alt_enter/leave
        // and scrollback_enter/leave. Same `<tab>:<pane>` prefix as
        // legacy parsers — splitting once on ':' and the rest on '\t'
        // must still recover all three fields.
        assert_eq!(pane_edge_payload(1, 2, 100), "1:2\t100");
        assert_eq!(pane_edge_payload(7, 9, 42), "7:9\t42");
        let p = pane_edge_payload(3, 5, 200);
        let fields: Vec<&str> = p.split([':', '\t']).collect();
        assert_eq!(fields, vec!["3", "5", "200"]);
    }

    #[test]
    fn style_key_hyperlink_accent_survives_inversion() {
        // The hyperlink accent blue must paint the glyph slot
        // regardless of REVERSE / DECSCNM, because StyleKey only
        // paints fg (bg is reserved). The override moved to
        // run AFTER the swap to fix this. Pin both forward and
        // reversed cases.
        let hyperlink_default_fg = Cell {
            ch: 'L',
            fg: TermColor::Default,
            bg: TermColor::Default,
            hyperlink: 7,
            ..Cell::default()
        };
        let key = StyleKey::from_cell(&hyperlink_default_fg, false, false, false);
        assert_eq!(key.fg, [86, 156, 214], "no reverse: glyph is blue");
        // With REVERSE attr.
        let rev = Cell {
            attrs: CellAttrs::REVERSE,
            ..hyperlink_default_fg
        };
        let key = StyleKey::from_cell(&rev, false, false, false);
        assert_eq!(key.fg, [86, 156, 214], "REVERSE attr keeps glyph blue");
        // With DECSCNM reverse_screen.
        let key = StyleKey::from_cell(&hyperlink_default_fg, false, false, true);
        assert_eq!(key.fg, [86, 156, 214], "DECSCNM keeps glyph blue");
        // Cursor inversion.
        let key = StyleKey::from_cell(&hyperlink_default_fg, true, false, false);
        assert_eq!(key.fg, [86, 156, 214], "cursor keeps glyph blue");
    }

    #[test]
    fn pane_attr_payload_is_legacy_compatible_and_appends_value() {
        // Shape: `<tab>:<pane>\t<uid>\t<value>`. Same prefix as
        // `pane.alt_enter` so legacy `(%d+):(%d+)` parsers keep
        // working; new code can split on `\t` for uid+value.
        assert_eq!(
            pane_attr_payload(1, 2, 100, "block"),
            "1:2\t100\tblock",
        );
        assert_eq!(
            pane_attr_payload(3, 5, 42, "any"),
            "3:5\t42\tany",
        );
        // Bool stringification matches Rust's `Display` for bool —
        // verified by passing the bool directly through the
        // generic `impl Display` parameter rather than a
        // pre-stringified literal. Catches a regression where a
        // future helper rewrite re-introduces an Into<String>
        // bound that would force callers back to the
        // `if b { "true" } else { "false" }` materialisation.
        assert_eq!(pane_attr_payload(1, 1, 7, true), "1:1\t7\ttrue");
        assert_eq!(pane_attr_payload(1, 1, 7, false), "1:1\t7\tfalse");
        // Full tab-split parse yields exactly four fields.
        let p = pane_attr_payload(7, 9, 200, "underline");
        let fields: Vec<&str> = p.split([':', '\t']).collect();
        assert_eq!(fields, vec!["7", "9", "200", "underline"]);
    }

    #[test]
    fn mouse_mode_code_maps_four_modes_and_falls_back_to_off() {
        // Same shape as cursor_shape_code: u8 bucket per known name,
        // unknown -> 0 so a typo in the snapshot path silently lands
        // in "off" instead of churning a fresh bucket each frame.
        assert_eq!(mouse_mode_code("off"), 0);
        assert_eq!(mouse_mode_code("x10"), 1);
        assert_eq!(mouse_mode_code("btn"), 2);
        assert_eq!(mouse_mode_code("any"), 3);
        assert_eq!(mouse_mode_code("normal"), 0);
        assert_eq!(mouse_mode_code(""), 0);
    }

    #[test]
    fn shape_and_mode_name_tables_have_expected_length() {
        // The snapshot builder indexes into `CURSOR_SHAPE_NAMES` /
        // `MOUSE_MODE_NAMES` with a manually-curated match
        // (`MouseTracking::Off => 0`, …). Adding a new variant to
        // either enum without growing the matching array would
        // out-of-bounds at runtime. Pin the expected lengths so
        // such a regression fails at `cargo test` instead.
        assert_eq!(CURSOR_SHAPE_NAMES.len(), 3, "block/underline/bar");
        assert_eq!(MOUSE_MODE_NAMES.len(), 4, "off/x10/btn/any");
    }

    #[test]
    fn cursor_shape_code_and_mouse_mode_code_form_bijective_partition() {
        // Pin the partition property: every known name maps to a
        // distinct u8, and the ordering matches what
        // `rterm.cursor_shape_names()` / `rterm.mouse_mode_names()`
        // advertise on the Lua side. A future renaming that
        // collides codes or shifts the index ordering surfaces
        // here rather than as a plugin runtime mismatch.
        let shape_pairs: [(&str, u8); 3] = [
            ("block", 0),
            ("underline", 1),
            ("bar", 2),
        ];
        let mut seen_shape: std::collections::HashSet<u8> =
            std::collections::HashSet::new();
        for (name, code) in shape_pairs {
            assert_eq!(cursor_shape_code(name), code, "{name}");
            assert!(seen_shape.insert(code), "duplicate shape code {code}");
        }
        let mouse_pairs: [(&str, u8); 4] = [
            ("off", 0),
            ("x10", 1),
            ("btn", 2),
            ("any", 3),
        ];
        let mut seen_mouse: std::collections::HashSet<u8> =
            std::collections::HashSet::new();
        for (name, code) in mouse_pairs {
            assert_eq!(mouse_mode_code(name), code, "{name}");
            assert!(seen_mouse.insert(code), "duplicate mouse code {code}");
        }
    }

    #[test]
    fn cursor_shape_code_maps_three_known_shapes_and_falls_back_to_block() {
        // The edge-trigger gate stores shape as u8. Pin the encoding
        // so a renamed string variant (or a typo) doesn't silently
        // hash to a different bucket — which would either fire a
        // false-positive event every frame, or never fire when it
        // should.
        assert_eq!(cursor_shape_code("block"), 0);
        assert_eq!(cursor_shape_code("underline"), 1);
        assert_eq!(cursor_shape_code("bar"), 2);
        // Unknown -> 0 (block) so a stale string is silently
        // absorbed instead of clobbering the gate with a fresh
        // bucket each frame.
        assert_eq!(cursor_shape_code("steady-beam"), 0);
        assert_eq!(cursor_shape_code(""), 0);
    }

    #[test]
    fn progress_severity_ranks_error_above_warn_above_indeterminate_above_set() {
        // The tab-aggregate uses this ranking to pick which pane's
        // progress dominates. Pin the ordering so a future spec change
        // (or a typo) doesn't silently demote errors below warnings.
        let s = |x| progress_severity(x);
        assert!(s(2) > s(4), "error (2) must outrank warn (4)");
        assert!(s(4) > s(3), "warn (4) must outrank indeterminate (3)");
        assert!(s(3) > s(1), "indeterminate (3) must outrank set (1)");
        assert!(s(1) > s(0), "set (1) must outrank clear (0)");
        // Unknown state byte falls back to zero (lowest).
        assert_eq!(s(99), 0);
    }

    #[test]
    fn focus_index_decision_branches() {
        // The three branches Alt+N can hit on a single keypress.
        // (a) Out-of-range index → None (silently dropped — pressing
        //     Alt+5 on a tab with 2 panes is harmless).
        assert_eq!(focus_index_decision(0, 2, 5), None);
        // (b) Already focused → AlreadyThere so we skip the redundant
        //     pane.focus event but still pulse the cursor.
        assert_eq!(
            focus_index_decision(1, 3, 1),
            Some(FocusIndexAction::AlreadyThere),
        );
        // (c) Real move → Apply(idx).
        assert_eq!(
            focus_index_decision(0, 3, 2),
            Some(FocusIndexAction::Apply(2)),
        );
        // Edge: empty tab (no panes) → every index out of range.
        assert_eq!(focus_index_decision(0, 0, 0), None);
    }

    #[test]
    fn display_title_empty_dynamic_treated_as_unset() {
        // Some shells send `OSC 2 ; ; ST` (empty title) to clear the
        // window title. Treat empty exactly like no OSC was ever set —
        // don't show a blank tab label.
        let pane = test_pane("zsh");
        *pane.dynamic_title.lock().unwrap() = Some(String::new());
        *pane.last_foreground_process.lock().unwrap() = Some("htop".to_string());
        assert_eq!(pane.display_title(), "htop");
    }
}
