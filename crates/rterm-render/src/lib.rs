//! GPU renderer + winit application for rterm.

mod bg;
pub mod palette;
pub(crate) mod tree;
pub mod action;
pub use action::AppAction;
pub mod events;
pub use events::{EventSink, NullSink};

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    Style, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use rterm_core::{Cell, CellAttrs, Color as TermColor, MouseTracking, Size as TermSize, Terminal};

use crate::bg::BgLayer;
use crate::palette::{color_to_rgb, default_bg, default_fg};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent};
use winit::dpi::PhysicalPosition;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
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
    let reverse_screen = terminal.is_reverse_screen();
    let blink_state = blink_on || !terminal.cursor_should_blink();
    let cursor_active = focused
        && offset == 0
        && terminal.cursor_visible()
        && blink_state
        && matches!(terminal.cursor_shape(), rterm_core::CursorShape::Block);
    let cursor = terminal.cursor();
    let mut spans: Vec<(String, StyleKey)> = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let Some(row) = terminal.visible_row(offset, r) else { continue };
        let mut current_text = String::new();
        let mut current_key: Option<StyleKey> = None;
        for (c, cell_ref) in row.iter().enumerate() {
            let cell = *cell_ref;
            // Skip spacers — the preceding WIDE glyph visually covers them.
            if cell.attrs.contains(CellAttrs::WIDE_SPACER) {
                continue;
            }
            let is_cursor =
                cursor_active && cursor.row == r && cursor.col as usize == c;
            let is_selected = selection
                .map(|s| s.contains(r, c as u16))
                .unwrap_or(false);
            let key = StyleKey::from_cell(&cell, is_cursor, is_selected, reverse_screen);
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
}

impl NormSelection {
    pub fn contains(&self, row: u16, col: u16) -> bool {
        if row < self.start.row || row > self.end.row {
            return false;
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
}

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
        let mut header_right_buffer = Buffer::new(&mut font_system, Metrics::new(font_size, line_height));
        header_right_buffer.set_monospace_width(&mut font_system, Some(cell_width));
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

    /// Reserved height of the status / search bar painted at the
    /// bottom of the window. Zero when nothing is showing there;
    /// `line_height + 6` when the search overlay is open so the
    /// prompt has breathing room. The App passes the "is search
    /// active" hint via `bottom_bar_active`.
    pub fn status_bar_height(&self) -> f32 {
        0.0
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
        // current size rather than crashing.
        let font_size = if font_size.is_finite() {
            font_size.clamp(6.0, 96.0)
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

    fn ensure_buffers(&mut self, n: usize) {
        while self.buffers.len() < n {
            let mut b = Buffer::new(
                &mut self.font_system,
                Metrics::new(self.font_size, self.line_height),
            );
            b.set_monospace_width(&mut self.font_system, Some(self.cell_width));
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
            self.header_buffer.set_size(
                &mut self.font_system,
                Some(h.rect.width),
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
            main_areas.push(TextArea {
                buffer: &self.header_buffer,
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
        tracing::info!(
            backend = ?info.backend,
            adapter = %info.name,
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
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Pick an alpha-supporting composite mode when the user wants
        // transparency; otherwise stick with the platform default.
        let alpha_mode = if opacity < 1.0 {
            caps.alpha_modes
                .iter()
                .copied()
                .find(|m| {
                    matches!(
                        m,
                        wgpu::CompositeAlphaMode::PreMultiplied
                            | wgpu::CompositeAlphaMode::PostMultiplied
                    )
                })
                .unwrap_or(caps.alpha_modes[0])
        } else {
            caps.alpha_modes[0]
        };

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
        tracing::info!("building bg layer");
        let mut bg = BgLayer::new(&device, format);
        bg.resize(config.width, config.height);
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
        title_bar: Option<&TitleBarDraw<'_>>,
        status_bar: Option<&StatusBarDraw<'_>>,
        overlay: Option<&OverlayDraw<'_>>,
        flash: bool,
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
        static R2: std::sync::Once = std::sync::Once::new();
        R2.call_once(|| tracing::debug!("render: entering text.prepare"));
        if let Err(e) = self.text.prepare(
            &self.device,
            &self.queue,
            panes,
            header,
            header_right,
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
            // For a brief bell flash, lift the clear toward a warm tint so
            // the bell is visible even with a fully-painted screen.
            let clear = if flash {
                wgpu::Color {
                    r: (self.clear_color.r + 0.25).min(1.0),
                    g: (self.clear_color.g + 0.15).min(1.0),
                    b: self.clear_color.b,
                    a: 1.0,
                }
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
            //   1. bg.draw_main  — pane backgrounds, cursor, dim
            //   2. text.render_main — pane + header glyphs
            //   3. bg.draw_overlay — modal backdrop panels
            //   4. text.render_overlay — modal text
            // Without (3) sandwiched between (2) and (4) the overlay
            // panel ends up under both text passes and pane glyphs
            // bleed through the menu — the original visual bug.
            self.bg.draw_main(&mut pass);
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
}

/// Monotonic source for `Pane::uid`. Starts at 1 so `0` can serve as a
/// "no pane" sentinel for plugins that pass a default. Wraps at u64::MAX
/// — practically unreachable (1 billion splits per second for 580 years
/// to overflow), but we still skip 0 on wrap to keep the sentinel intact.
static PANE_UID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Pane {
    pub fn new(
        terminal: SharedTerminal,
        io: Arc<dyn TerminalIo>,
        title: impl Into<String>,
        alive: Arc<AtomicBool>,
        activity: Arc<AtomicBool>,
        last_output_ms: Arc<AtomicU64>,
        keepalive: Box<dyn std::any::Any + Send>,
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
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyMatch {
    /// Lowercase character — compared against `Key::Character` case-insensitively.
    Char(String),
    Named(NamedKey),
}

#[derive(Debug, Clone)]
pub struct UserBinding {
    mods: ModifiersState,
    key: KeyMatch,
    action: AppAction,
    /// Original key spec from config (`"Ctrl+Shift+T"`). Preserved
    /// verbatim for the help-overlay listing so the user sees the
    /// exact text they wrote rather than a normalised form.
    spec: String,
    /// Canonical action name (`"new_tab"`, `"split_horizontal"`, ...).
    /// Stored alongside the typed `AppAction` so the help overlay can
    /// print it without re-deriving from the enum variant.
    action_name: String,
}

/// Parse a key spec like `"Ctrl+Shift+T"`, `"Alt+Right"`, `"F1"`.
/// Returns None if the spec is malformed or names an unknown key.
fn parse_key_spec(s: &str) -> Option<(ModifiersState, KeyMatch)> {
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
const WINDOW_CONTROL_MIN: &str = " ─ ";
const WINDOW_CONTROL_MAX: &str = " ▢ ";
const WINDOW_CONTROL_CLOSE: &str = " ✕ ";
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
}

#[derive(Debug, Clone, Copy)]
struct ActiveSelection {
    pane_idx: usize,
    anchor: SelectionPoint,
    focus: SelectionPoint,
    mode: SelectionMode,
    /// For Word/Line modes: the inclusive cell range of the original
    /// pivot word/line captured on the initial multi-click. Subsequent
    /// drag-extends snap to word/line bounds at the drag point and
    /// anchor against this pivot range, so the selection always covers
    /// whole words or whole lines.
    pivot: Option<(SelectionPoint, SelectionPoint)>,
}

impl ActiveSelection {
    fn normalized(&self) -> NormSelection {
        let a = (self.anchor.row, self.anchor.col);
        let f = (self.focus.row, self.focus.col);
        let (s, e) = if a <= f { (self.anchor, self.focus) } else { (self.focus, self.anchor) };
        NormSelection { start: s, end: e }
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

/// Guake-style drop-down snapshot. Mirrors `rterm_config::GuakeConfig`
/// but lives in the render crate so the renderer doesn't depend on the
/// config crate.
#[derive(Debug, Clone)]
pub struct GuakeRunConfig {
    /// Master toggle. When `false`, `toggle_guake` is a no-op.
    pub enabled: bool,
    /// `"top"` | `"bottom"` | `"full"`. Other values fall back to
    /// `"top"`.
    pub position: String,
    /// 10..=100, height fraction of the current monitor for `top` /
    /// `bottom`. Ignored for `"full"`.
    pub height_pct: u8,
    /// 20..=100, width fraction of the current monitor.
    pub width_pct: u8,
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
}

pub struct App {
    state: Option<GpuState>,
    title: String,
    initial_size: (u32, u32),
    font_size: f32,
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
    modifiers: ModifiersState,
    cursor_pos: PhysicalPosition<f64>,
    selection: Option<ActiveSelection>,
    mouse_dragging: bool,
    last_click: Option<(Instant, SelectionPoint, usize)>,
    /// Timestamp of the most recent click on header-bar empty space (not
    /// on a tab). A second click within `MAX_DBL_CLICK_MS` opens a new
    /// tab — browser-style "double-click empty tab strip → new tab".
    last_header_empty_click: Option<Instant>,
    /// Pane index currently receiving PTY mouse events. Set when a press
    /// happens while mouse reporting is on; cleared on release.
    mouse_pty_pane: Option<usize>,
    /// Until-instant for a bell-induced visual flash of the surface clear.
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
}

const CURSOR_BLINK_PERIOD_MS: u128 = 1000;

const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);

impl App {
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
            os_decorations,
            allow_osc52,
            guake,
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
            user_bindings,
            modifiers: ModifiersState::empty(),
            cursor_pos: PhysicalPosition::new(0.0, 0.0),
            selection: None,
            mouse_dragging: false,
            last_click: None,
            last_header_empty_click: None,
            mouse_pty_pane: None,
            flash_until: None,
            search: None,
            gap_dragging: None,
            tab_dragging: None,
            tab_drag_press_offset: 0.0,
            tab_swap_anim: None,
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
            os_decorations,
            palette: None,
            window_focused: true,
            save_scrollback_on_exit,
            scroll_on_output,
            cursor_blink,
            show_scrollbar,
            bell_visual,
            bell_urgent,
            allow_osc52,
            guake,
            guake_dropped: false,
            first_frame_done: false,
            render_test_only,
            last_frame_tick: None,
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
    fn layout_active_tab(&self) -> Vec<PaneRect> {
        let _ = self.state.as_ref().is_some(); // bail-early guard via outer_rect below
        let Some(tab) = self.active_tab() else { return vec![] };
        if tab.pane_count() == 0 {
            return vec![];
        }
        // Single source of truth for "what space is left after the
        // header + any bottom bar". Without this the layout used its
        // own copy of the formula and missed the search bar reserve,
        // so panes painted over the bar and the PTY grid never
        // shrank by the bar's height. Result: shell output stopped at
        // ~half the window after the bar appeared.
        let Some(outer) = self.outer_rect() else { return vec![] };
        let full = tab
            .tree
            .layout(outer, SPLIT_GAP)
            .into_iter()
            .map(|(_, rect, _)| rect)
            .collect::<Vec<_>>();
        if tab.zoomed && tab.pane_count() > 1 {
            // Replace every pane's rect with a degenerate (0×0) one *except*
            // the focused pane, which gets the full inner rect. We keep the
            // list length aligned with DFS order so callers indexing by pane
            // index still work.
            let focus_idx = tab.focused_index().unwrap_or(0);
            full.iter()
                .enumerate()
                .map(|(i, _)| {
                    if i == focus_idx {
                        outer
                    } else {
                        PaneRect { left: outer.left, top: outer.top, width: 0.0, height: 0.0 }
                    }
                })
                .collect()
        } else {
            full
        }
    }

    fn header_rect(&self) -> Option<PaneRect> {
        let state = self.state.as_ref()?;
        let w = state.config.width as f32;
        let header_h = state.text.header_height();
        Some(PaneRect {
            left: PAD,
            top: PAD,
            width: (w - 2.0 * PAD).max(0.0),
            height: header_h,
        })
    }

    /// Bottom-of-window status / search bar rect. Returns `None`
    /// when the bar is disabled (no search active and no status
    /// content) or the window is too short to host it.
    fn status_bar_rect(&self) -> Option<PaneRect> {
        let state = self.state.as_ref()?;
        let w = state.config.width as f32;
        let h = state.config.height as f32;
        // Use `has_content` here so the scrollback indicator still
        // gets a rect to paint into even though it doesn't reserve
        // pane space. Layout (outer_rect) uses the stricter
        // `reserves_space`.
        let active = self.bottom_bar_has_content();
        let sh = state.text.bottom_bar_height(active);
        if sh <= 0.0 || h <= sh + PAD * 2.0 {
            return None;
        }
        Some(PaneRect {
            left: 0.0,
            top: h - sh,
            width: w,
            height: sh,
        })
    }

    /// True when the bottom bar should reserve pane space (i.e.
    /// `outer_rect` shrinks by the bar height). Only the search prompt
    /// qualifies — it captures keyboard input, so the user expects
    /// the layout to acknowledge it. The scrollback indicator floats
    /// on top of pane content without resizing, because grabbing
    /// pane rows mid-scroll would force a `sync_terminal_size` and
    /// SIGWINCH on every scroll-step.
    fn bottom_bar_reserves_space(&self) -> bool {
        self.search.is_some()
    }

    /// True when the bottom bar has any visible content — search
    /// prompt OR scrollback position indicator. Used by the render
    /// path to decide whether to paint the bar; layout uses the
    /// stricter `bottom_bar_reserves_space`.
    fn bottom_bar_has_content(&self) -> bool {
        if self.search.is_some() {
            return true;
        }
        let Some(tab) = self.active_tab() else { return false };
        let Some(p) = tab.focused_pane() else { return false };
        p.scroll_offset.load(Ordering::Relaxed) > 0
    }

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
        // (min / max / close), which share the row with the tabs.
        let controls_cells = if self.os_decorations {
            0.0
        } else {
            WINDOW_CONTROLS_WIDTH_CELLS as f64
        };
        let tabs_right =
            (rect.left as f64 + rect.width as f64) - controls_cells * cell_w;
        let mut cursor = hb_end;
        let mut entries = Vec::with_capacity(self.tabs.len());
        for (i, tab) in self.tabs.iter().enumerate() {
            let info = self.tab_label(i, tab);
            let close_cells = TAB_CLOSE_WIDTH_CELLS as f64;
            let body_end = cursor + (info.body_cells + info.badge_cells) as f64 * cell_w;
            let close_end = body_end + close_cells * cell_w;
            // Stop laying out tabs once we run into the window-controls
            // strip on the right. The remaining tabs simply aren't
            // clickable from the tab bar this frame (resize the window
            // or rebind a focus shortcut) — better than overlapping
            // chips that look broken.
            if cursor >= tabs_right {
                break;
            }
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
            if x >= e.left && x < e.close_end {
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

    /// Build the help-overlay spans into `storage` and return colored slices.
    fn help_spans<'a>(&self, storage: &'a mut Vec<String>) -> Vec<(&'a str, [u8; 3], bool)> {
        storage.clear();
        const HEADINGS: &[(&str, &[(&str, &str)])] = &[
            ("rterm — keybindings", &[]),
            ("", &[]),
            ("Tabs", &[
                ("Ctrl+Shift+T", "new tab"),
                ("Ctrl+Shift+W", "close tab"),
                ("Ctrl+Shift+←/→", "switch tab"),
                ("Ctrl+Shift+1..9", "jump to tab N"),
                ("Ctrl+Shift+Tab", "switch to last tab"),
                ("Ctrl+Shift+,/.", "move tab left/right"),
            ]),
            ("Font", &[
                ("Ctrl+Shift+=", "bigger"),
                ("Ctrl+Shift+-", "smaller"),
                ("Ctrl+Shift+0", "reset"),
            ]),
            ("Panes", &[
                ("Ctrl+Shift+D", "split horizontal"),
                ("Ctrl+Shift+E", "split vertical"),
                ("Ctrl+Shift+X", "close pane"),
                ("Ctrl+Shift+Z", "zoom / unzoom"),
                ("Ctrl+Shift+{/}", "swap pane with prev / next"),
                ("Alt+←/↑/→/↓", "focus pane in direction"),
                ("Alt+1..9", "focus pane N (DFS order)"),
                ("Alt+Shift+←/↑/→/↓", "resize focused pane"),
                ("Drag gap", "resize panes"),
            ]),
            ("Clipboard", &[
                ("Ctrl+Shift+V", "paste"),
                ("Shift+Insert", "paste (xterm)"),
                ("Ctrl+Shift+C", "copy selection"),
                ("Ctrl+Insert", "copy selection (xterm)"),
                ("Ctrl+Shift+Y", "copy hovered URL"),
                ("Select", "highlight only (use Ctrl+Shift+C to copy)"),
            ]),
            ("Search & scroll", &[
                ("Ctrl+Shift+F", "search scrollback"),
                ("  Enter / ↓", "next match"),
                ("  Shift+Enter / ↑", "previous match"),
                ("  Ctrl+R", "toggle regex mode"),
                ("  Ctrl+W", "delete word"),
                ("  Ctrl+U", "clear query"),
                ("  Esc", "exit search"),
                ("Shift+PgUp/Dn", "scroll page"),
                ("Shift+Home/End", "scroll top/bottom"),
                ("Ctrl+Alt+↑/↓", "jump to prev/next prompt (OSC 133)"),
                ("Ctrl+Shift+K", "clear scrollback (saved lines)"),
                ("Wheel", "scroll"),
                ("Ctrl+Wheel", "font size: bigger / smaller"),
            ]),
            ("Mouse selection", &[
                ("Drag", "select"),
                ("Double-click", "select word"),
                ("Triple-click", "select line"),
                ("Shift+Click", "extend selection"),
                ("Click", "focus pane (or switch tab when on tab bar)"),
                ("Ctrl+Click / Cmd+Click", "open URL / hyperlink"),
                ("Middle-click tab", "close tab"),
                ("Middle-click pane", "paste PRIMARY (Linux) / clipboard"),
                ("Right-click", "paste from clipboard"),
                ("Wheel on tab bar", "switch tab"),
                ("Double-click tab bar", "new tab"),
                ("Drag tab", "reorder"),
            ]),
            ("Command palette", &[
                ("Ctrl+Shift+P", "open palette"),
                ("  ↑/↓", "select action"),
                ("  Enter", "run action"),
                ("  Ctrl+W", "delete word"),
                ("  Ctrl+U", "clear query"),
                ("  Esc", "close"),
            ]),
            ("Themes & settings", &[
                ("Palette → Theme: …", "switch theme via Ctrl+Shift+P"),
                ("Palette → cycle_theme", "next built-in theme"),
                ("Palette → open_settings", "live settings overlay"),
                ("(in settings)  T",  "next theme"),
                ("(in settings)  F",  "font size +/−"),
                ("(in settings)  O",  "opacity +/−"),
                ("(in settings)  B",  "toggle cursor blink"),
                ("(in settings)  S",  "toggle scrollbar"),
            ]),
            ("Tab bar", &[
                ("Click ≡ (hamburger)", "open app menu"),
                ("Click tab body", "switch to tab"),
                ("Click × on tab", "close tab"),
                ("Middle-click tab", "close tab"),
                ("Right-click tab", "tab context menu"),
                ("Drag tab", "reorder"),
                ("Double-click empty", "new tab"),
            ]),
            ("Right-click menu", &[
                ("Right-click pane", "Copy / Paste / Split / Close / Settings"),
                ("Right-click tab", "Close / Move / Zoom / New"),
                ("Right-click header", "New tab / Palette / Themes / Settings / Quit"),
                ("Up/Down + Enter", "navigate the menu"),
                ("Esc", "close menu"),
            ]),
            ("This help", &[
                ("Ctrl+Shift+H", "toggle"),
                ("Esc", "close"),
            ]),
        ];
        let muted = palette::default_fg().map(|c| c.saturating_sub(60));
        let accent: [u8; 3] = [220, 200, 120];
        let key_color: [u8; 3] = [121, 192, 255];
        // Build the help content as a list of LINES first — each
        // element is a vec of spans that share one printed row. After
        // building, slice into the visible viewport and stitch the
        // displayed spans into `storage`.
        type Line = Vec<(String, [u8; 3], bool)>;
        let mut lines: Vec<Line> = Vec::new();
        for (heading, rows) in HEADINGS {
            if !rows.is_empty() {
                // Blank spacer before each new section, then the heading.
                lines.push(vec![(String::new(), muted, false)]);
                lines.push(vec![(format!("  {}", heading), accent, true)]);
            } else if !heading.is_empty() {
                lines.push(vec![(format!("  {}", heading), accent, true)]);
            } else {
                lines.push(vec![(String::new(), muted, false)]);
            }
            for (k, v) in *rows {
                lines.push(vec![
                    (format!("    {:<20}", k), key_color, false),
                    (v.to_string(), muted, false),
                ]);
            }
        }
        if !self.user_bindings.is_empty() {
            lines.push(vec![(String::new(), muted, false)]);
            lines.push(vec![("  User keybindings".to_string(), accent, true)]);
            let labels: std::collections::HashMap<&'static str, &'static str> =
                AppAction::name_label_pairs().into_iter().collect();
            for b in &self.user_bindings {
                let value = match labels.get(b.action_name()) {
                    Some(label) => format!("{} ({})", b.action_name(), label),
                    None => b.action_name().to_string(),
                };
                lines.push(vec![
                    (format!("    {:<20}", b.spec), key_color, false),
                    (value, muted, false),
                ]);
            }
        }
        // Decide visible window. Capacity is derived from the actual
        // overlay rect height (see `help_visible_lines`) so a
        // maximized window shows the full help in one viewport
        // instead of clipping at a hardcoded line count.
        let visible = self.help_visible_lines();
        let total = lines.len();
        let max_off = total.saturating_sub(visible);
        let scroll = self.help_scroll.min(max_off);
        let start = scroll;
        let end = (start + visible).min(total);
        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        // Top "↑ N more" hint when content extends above the viewport.
        if start > 0 {
            storage.push(format!("    ↑ {} more (PgUp / Home)\n", start));
            spans.push((storage.len() - 1, muted, false));
        }
        for line in &lines[start..end] {
            // Concatenate every span on the row, then add a trailing
            // \n so the next row starts on a fresh line.
            for (text, color, bold) in line.iter().take(line.len().saturating_sub(1)) {
                storage.push(text.clone());
                spans.push((storage.len() - 1, *color, *bold));
            }
            if let Some((last_text, last_color, last_bold)) = line.last() {
                storage.push(format!("{}\n", last_text));
                spans.push((storage.len() - 1, *last_color, *last_bold));
            } else {
                storage.push("\n".to_string());
                spans.push((storage.len() - 1, muted, false));
            }
        }
        // Bottom "↓ N more" hint.
        if end < total {
            storage.push(format!("    ↓ {} more (PgDn / End)\n", total - end));
            spans.push((storage.len() - 1, muted, false));
        }
        spans
            .into_iter()
            .map(|(idx, color, bold)| (storage[idx].as_str(), color, bold))
            .collect()
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

    fn help_rect(&self) -> Option<PaneRect> {
        let state = self.state.as_ref()?;
        let w = state.config.width as f32;
        let h = state.config.height as f32;
        // Centered, fits the cheat-sheet text comfortably. Guard against
        // a window smaller than the desired margin — `w - 40` could go
        // negative and produce a rect with a giant negative `left`.
        let max_w = 520.0_f32.min(w - 40.0).max(100.0);
        let max_h = (h - 40.0).max(100.0);
        // If the window is still too small to render the overlay, skip
        // it entirely rather than draw at clipped coordinates.
        if w < max_w + 1.0 || h < max_h + 1.0 {
            return None;
        }
        Some(PaneRect {
            left: (w - max_w) * 0.5,
            top: (h - max_h) * 0.5,
            width: max_w,
            height: max_h,
        })
    }

    /// Build the live-settings overlay text. Shows the current theme,
    /// font size, opacity, and the keys that adjust them.
    ///
    /// Also rebuilds `self.settings_hits` with the pixel rects of each
    /// clickable element (theme rows, +/− buttons, toggles) so mouse
    /// clicks land on the right action without a second layout pass.
    fn settings_spans<'a>(
        &mut self,
        storage: &'a mut Vec<String>,
    ) -> Vec<(&'a str, [u8; 3], bool)> {
        storage.clear();
        self.settings_hits.clear();
        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        let fg = palette::default_fg();
        let muted = fg.map(|c| c.saturating_sub(80));
        let accent: [u8; 3] = [120, 180, 240];
        let warm: [u8; 3] = [255, 204, 102];

        // Pixel metrics for the upcoming hit-zone math. Done up front so
        // every "what row are we on now?" calculation uses the same
        // origin as the rendered text.
        let (rect, cell_w, line_h) = match (self.help_rect(), self.state.as_ref()) {
            (Some(r), Some(s)) => (r, s.text.cell_width(), s.text.line_height()),
            _ => (PaneRect { left: 0.0, top: 0.0, width: 0.0, height: 0.0 }, 0.0, 0.0),
        };
        let row_y = |row: usize| rect.top + row as f32 * line_h + 2.0;
        let col_x = |col: usize| rect.left + col as f32 * cell_w + 4.0;
        let mut row: usize = 0;
        let push_hit = |hits: &mut Vec<(PaneRect, SettingsHit)>, row: usize, col: usize, cells: usize, hit: SettingsHit| {
            if cell_w <= 0.0 || line_h <= 0.0 {
                return;
            }
            hits.push((
                PaneRect {
                    left: col_x(col),
                    top: row_y(row),
                    width: cell_w * cells as f32,
                    height: line_h,
                },
                hit,
            ));
        };

        storage.push("rterm — settings".to_string());
        spans.push((storage.len() - 1, accent, true));
        storage.push("\n\n".to_string());
        spans.push((storage.len() - 1, fg, false));
        row += 2;

        // Theme picker — one row per built-in theme, with a radio-like
        // (•) / ( ) marker. Click anywhere on the row to apply.
        storage.push("  Theme\n".to_string());
        spans.push((storage.len() - 1, accent, true));
        row += 1;
        for (name, _) in palette::builtin_themes() {
            let selected = name.eq_ignore_ascii_case(&self.active_theme);
            let marker = if selected { "(●)" } else { "( )" };
            let pretty = name.replace('-', " ");
            storage.push(format!("    {} {}\n", marker, pretty));
            let color = if selected { warm } else { fg };
            spans.push((storage.len() - 1, color, selected));
            // 4 spaces + "(•) " is 8 cells, then up to ~20 for the name.
            push_hit(&mut self.settings_hits, row, 4, 28, SettingsHit::Theme(name));
            row += 1;
        }

        storage.push("\n".to_string());
        spans.push((storage.len() - 1, fg, false));
        row += 1;

        // Font size with [−] [+] [reset] buttons.
        let font_row = row;
        storage.push(format!(
            "  Font size: {:>5.1} pt   [ − ] [ + ] [ reset ]\n",
            self.font_size
        ));
        spans.push((storage.len() - 1, fg, false));
        // "  Font size:  13.0 pt   " — pre-button text is 24 cells wide.
        push_hit(&mut self.settings_hits, font_row, 24, 5, SettingsHit::FontDelta(-1.0));
        push_hit(&mut self.settings_hits, font_row, 30, 5, SettingsHit::FontDelta(1.0));
        push_hit(&mut self.settings_hits, font_row, 36, 9, SettingsHit::FontDelta(0.0));
        row += 1;

        let opacity_row = row;
        storage.push(format!(
            "  Opacity  :  {:>3} %     [ − ] [ + ] [ reset ]\n",
            (self.opacity * 100.0).round() as i32
        ));
        spans.push((storage.len() - 1, fg, false));
        push_hit(&mut self.settings_hits, opacity_row, 24, 5, SettingsHit::OpacityDelta(-0.05));
        push_hit(&mut self.settings_hits, opacity_row, 30, 5, SettingsHit::OpacityDelta(0.05));
        push_hit(&mut self.settings_hits, opacity_row, 36, 9, SettingsHit::OpacityDelta(0.0));
        row += 1;

        storage.push("\n".to_string());
        spans.push((storage.len() - 1, fg, false));
        row += 1;

        // Checkboxes — click anywhere on the row toggles.
        let blink_row = row;
        let blink_mark = if self.cursor_blink { "[x]" } else { "[ ]" };
        storage.push(format!("  {} Cursor blink\n", blink_mark));
        spans.push((storage.len() - 1, fg, false));
        push_hit(&mut self.settings_hits, blink_row, 2, 22, SettingsHit::ToggleBlink);
        row += 1;

        let scroll_row = row;
        let scroll_mark = if self.show_scrollbar { "[x]" } else { "[ ]" };
        storage.push(format!("  {} Scrollbar\n", scroll_mark));
        spans.push((storage.len() - 1, fg, false));
        push_hit(&mut self.settings_hits, scroll_row, 2, 22, SettingsHit::ToggleScrollbar);
        row += 1;

        storage.push("\n".to_string());
        spans.push((storage.len() - 1, fg, false));
        row += 1;

        // Footer buttons.
        let footer_row = row;
        storage.push("  [ Help ]   [ Close ]\n".to_string());
        spans.push((storage.len() - 1, accent, false));
        push_hit(&mut self.settings_hits, footer_row, 2, 8, SettingsHit::OpenHelp);
        push_hit(&mut self.settings_hits, footer_row, 13, 9, SettingsHit::Close);

        storage.push("\n".to_string());
        spans.push((storage.len() - 1, fg, false));
        // Hints — keep the keyboard cheats discoverable.
        storage.push(
            "  Keys: T/Shift+T theme · F/Shift+F font · O/Shift+O opacity\n  \
             0 reset font · 9 reset opacity · B blink · S scrollbar · ? help · Esc close\n"
                .to_string(),
        );
        spans.push((storage.len() - 1, muted, false));

        spans
            .into_iter()
            .map(|(i, c, b)| (storage[i].as_str(), c, b))
            .collect()
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
                    let initial = self.font_size;
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
                    let initial = self.font_size;
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
            // Hover highlight on the × close marker — Chrome-style:
            // small darker pill behind the cross when the cursor is
            // specifically over it.
            if hover_close_tab == Some(e.idx) {
                if let Some(state) = self.state.as_ref() {
                    let cell_w = state.text.cell_width();
                    if cell_w > 0.0 {
                        let close_w = TAB_CLOSE_WIDTH_CELLS as f32 * cell_w;
                        let close_left_px =
                            (e.close_end as f32) - close_w + 1.0;
                        let highlight = [
                            bg[0].saturating_add(40).min(180),
                            bg[1].saturating_sub(10),
                            bg[2].saturating_sub(10),
                        ];
                        out.push(bg::BgQuad::from_srgb_rounded(
                            [close_left_px, chip_top + 2.0],
                            [close_w - 2.0, chip_h - 4.0],
                            highlight,
                            0.85,
                            chip_radius * 0.8,
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

    /// Compute the on-screen rect for a context menu, clamped so it
    /// stays inside the window. Returns `None` when the window is too
    /// small to fit any menu.
    fn context_menu_rect(&self, menu: &ContextMenu) -> Option<PaneRect> {
        let state = self.state.as_ref()?;
        let w = state.config.width as f32;
        let h = state.config.height as f32;
        let cell_w = state.text.cell_width();
        let line_h = state.text.line_height();
        if cell_w <= 0.0 || line_h <= 0.0 {
            return None;
        }
        let max_label = menu
            .items
            .iter()
            .map(|i| match i {
                MenuItem::Action { label, .. } => label.chars().count(),
                MenuItem::Separator => 4,
            })
            .max()
            .unwrap_or(0);
        // "  ▶ " prefix on the focused row + padding.
        let menu_w = (max_label as f32 + 6.0) * cell_w;
        let menu_h = menu.items.len() as f32 * line_h + 4.0;
        let mut left = menu.anchor.0;
        let mut top = menu.anchor.1;
        if left + menu_w > w {
            left = (w - menu_w).max(0.0);
        }
        if top + menu_h > h {
            top = (h - menu_h).max(0.0);
        }
        Some(PaneRect { left, top, width: menu_w.min(w), height: menu_h.min(h) })
    }

    /// Build the per-row text spans for the context menu (or app menu).
    fn context_menu_spans<'a>(
        &self,
        menu: &ContextMenu,
        storage: &'a mut Vec<String>,
    ) -> Vec<(&'a str, [u8; 3], bool)> {
        storage.clear();
        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        let fg = palette::default_fg();
        let muted = fg.map(|c| c.saturating_sub(80));
        let accent: [u8; 3] = [120, 180, 240];
        for (i, item) in menu.items.iter().enumerate() {
            let is_hovered = menu.hovered == Some(i);
            match item {
                MenuItem::Separator => {
                    storage.push("  ─────\n".to_string());
                    spans.push((storage.len() - 1, muted, false));
                }
                MenuItem::Action { label, enabled, .. } => {
                    let prefix = if is_hovered { "▶ " } else { "  " };
                    let line = format!("  {}{}\n", prefix, label);
                    storage.push(line);
                    let color = if !enabled {
                        muted
                    } else if is_hovered {
                        accent
                    } else {
                        fg
                    };
                    spans.push((storage.len() - 1, color, is_hovered));
                }
            }
        }
        spans
            .into_iter()
            .map(|(idx, c, b)| (storage[idx].as_str(), c, b))
            .collect()
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

    /// Build the tab-bar text + per-span colour. Active tab is rendered bold
    /// in a brighter colour; others muted. The search prompt has its own
    /// bottom bar — the tab strip stays visible during search.
    fn header_spans<'a>(&self, storage: &'a mut Vec<String>) -> Vec<(&'a str, [u8; 3], bool)> {
        storage.clear();
        let muted = palette::default_fg().map(|c| c.saturating_sub(80));
        let activity_accent: [u8; 3] = [255, 204, 102]; // warm yellow
        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        // Hamburger `≡` button at the very start of the header bar.
        // Clicking it opens the app menu (a context-menu populated with
        // every common action). Width pinned to `HAMBURGER_WIDTH_CELLS`
        // so `tab_hit_at` knows where the tabs actually begin.
        storage.push(HAMBURGER_GLYPH.to_string());
        let hb_color = if self.show_app_menu {
            palette::default_fg()
        } else {
            palette::default_fg().map(|c| c.saturating_sub(40))
        };
        spans.push((storage.len() - 1, hb_color, self.show_app_menu));
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
        // Reserve the trailing N cells for the `+` new-tab button so
        // cosmic-text's monospace layout leaves the glyph slot empty —
        // the `+` itself is drawn as two centered bg quads in
        // `tab_bar_quads`, sidestepping font-specific glyph centering
        // drift that the old text-flow approach suffered from.
        storage.push(" ".repeat(NEW_TAB_WIDTH_CELLS));
        spans.push((storage.len() - 1, muted, false));
        // While hovering over a link, surface the URL; otherwise fall back to
        // the focused pane's cwd as a soft status hint.
        if let Some(url) = self.hover_url.as_ref() {
            let trimmed = trim_label(url, 80);
            storage.push(format!("  🔗 {}", trimmed));
            let accent: [u8; 3] = [120, 180, 240];
            spans.push((storage.len() - 1, accent, false));
        } else if let Some(tab) = self.active_tab() {
            if let Some(p) = tab.focused_pane() {
                let cwd = p
                    .terminal
                    .lock()
                    .ok()
                    .and_then(|t| t.cwd().map(|s| s.to_string()));
                if let Some(c) = cwd {
                    let abbreviated = abbreviate_home(&c);
                    storage.push(format!("  ▸ {}", abbreviated));
                    spans.push((storage.len() - 1, muted, false));
                }
            }
        }
        // The scrollback position chip used to render here as
        // `  ↑ off/total`. Moved to the bottom status bar via
        // `scroll_status_spans` so it doesn't compete with tab labels
        // and window controls for the limited top-row real estate.
        spans
            .into_iter()
            .map(|(idx, color, bold)| (storage[idx].as_str(), color, bold))
            .collect()
    }

    /// Bottom status-bar spans: shell program, focused pane cwd, pane
    /// count, exit code. Mirrors VSCode's "shell + cwd + indicators"
    /// strip but condensed to one short line.
    fn status_bar_spans<'a>(
        &self,
        storage: &'a mut Vec<String>,
    ) -> AnchoredSpans<'a> {
        storage.clear();
        let rect = self.status_bar_rect()?;
        let tab = self.active_tab()?;
        let pane = tab.focused_pane()?;
        let cwd = pane
            .terminal
            .lock()
            .ok()
            .and_then(|t| t.cwd().map(String::from));
        let cwd_str = cwd
            .as_deref()
            .map(abbreviate_home)
            .unwrap_or_else(|| "~".to_string());
        let pane_count = tab.pane_count();
        let tab_count = self.tabs.len();
        let shell = pane.display_title();
        // Layout: " ▸ <shell>   ◉ <cwd>   ⌗ <pane>/<panes>   ▤ <tab>/<tabs> "
        storage.push(format!(" ▸ {}", shell));
        storage.push(format!("   ◉ {}", cwd_str));
        storage.push(format!("   ⌗ {}/{}", 1, pane_count));
        storage.push(format!("   ▤ {}/{}", self.active_tab + 1, tab_count));
        let fg = palette::default_fg();
        let dim = fg.map(|c| c.saturating_sub(60));
        let accent: [u8; 3] = [0, 122, 204];
        let spans = vec![
            (storage[0].as_str(), accent, true),
            (storage[1].as_str(), dim, false),
            (storage[2].as_str(), dim, false),
            (storage[3].as_str(), dim, false),
        ];
        Some((spans, rect))
    }

    /// Bottom-bar spans for the scrollback position indicator —
    /// `↑ off / total  (Shift+End to live)`. Rendered when the focused
    /// pane has `scroll_offset > 0` and search isn't holding the bar.
    fn scrollback_bar_spans<'a>(
        &self,
        storage: &'a mut Vec<String>,
    ) -> AnchoredSpans<'a> {
        storage.clear();
        let rect = self.status_bar_rect()?;
        let tab = self.active_tab()?;
        let pane = tab.focused_pane()?;
        let off = pane.scroll_offset.load(Ordering::Relaxed) as usize;
        if off == 0 {
            return None;
        }
        let total = pane
            .terminal
            .lock()
            .ok()
            .map(|t| t.scrollback_len())
            .unwrap_or(0);
        let warm: [u8; 3] = [220, 200, 120];
        let muted = palette::default_fg().map(|c| c.saturating_sub(80));
        storage.push(format!(" ↑ {} / {}", off, total));
        storage.push("   Shift+PgUp/PgDn · Shift+Home top · Shift+End live".to_string());
        let spans = vec![
            (storage[0].as_str(), warm, true),
            (storage[1].as_str(), muted, false),
        ];
        Some((spans, rect))
    }

    /// Search prompt spans: same content the old in-header search
    /// status line had, but anchored to the bottom-bar rect so the
    /// tab strip stays visible during search.
    fn search_bar_spans<'a>(
        &self,
        storage: &'a mut Vec<String>,
    ) -> AnchoredSpans<'a> {
        storage.clear();
        let rect = self.status_bar_rect()?;
        let state = self.search.as_ref()?;
        let prefix = if state.regex_mode { "/re/" } else { "/" };
        let counter = if state.regex_error {
            "(invalid regex) ".to_string()
        } else if state.matches.is_empty() {
            if state.query.is_empty() {
                "(no query) ".to_string()
            } else {
                "(no matches) ".to_string()
            }
        } else {
            format!("{}/{} ", state.current + 1, state.matches.len())
        };
        storage.push(format!(" {}{}", prefix, state.query));
        storage.push(format!(" {}", counter));
        storage.push("[Esc] [Enter:next] [Ctrl+R:regex] [Ctrl+W:word] [Ctrl+U:clear]".to_string());
        let accent: [u8; 3] = [0, 122, 204];
        let red: [u8; 3] = [241, 76, 76];
        let muted = palette::default_fg().map(|c| c.saturating_sub(80));
        let dim = palette::default_fg().map(|c| c.saturating_sub(120));
        let counter_color = if state.regex_error { red } else { muted };
        let spans = vec![
            (storage[0].as_str(), accent, true),
            (storage[1].as_str(), counter_color, false),
            (storage[2].as_str(), dim, false),
        ];
        Some((spans, rect))
    }

    /// Build the right-anchored window-control glyphs in their own
    /// span list + pixel rect. Rendered via `HeaderRightDraw` so the
    /// glyphs don't depend on the (variable) length of the main header
    /// text — fixes the "controls drift when cwd/url changes" bug.
    fn header_right_spans<'a>(
        &self,
        storage: &'a mut Vec<String>,
    ) -> AnchoredSpans<'a> {
        storage.clear();
        let rect = self.header_rect()?;
        let state = self.state.as_ref()?;
        let cell_w = state.text.cell_width();
        if cell_w <= 0.0 {
            return None;
        }
        // Show controls only when we own the OS chrome — otherwise the
        // native title bar provides the same buttons and ours would be
        // visually redundant.
        if self.os_decorations {
            return None;
        }
        let total_w = WINDOW_CONTROLS_WIDTH_CELLS as f32 * cell_w;
        let right_rect = PaneRect {
            left: rect.left + rect.width - total_w,
            top: rect.top,
            width: total_w,
            height: rect.height,
        };
        let muted_strong = palette::default_fg().map(|c| c.saturating_sub(40));
        let red: [u8; 3] = [241, 76, 76];
        let gap = " ".repeat(WINDOW_CONTROL_GAP_CELLS);
        storage.push(WINDOW_CONTROL_MIN.to_string());
        storage.push(gap.clone());
        storage.push(WINDOW_CONTROL_MAX.to_string());
        storage.push(gap);
        storage.push(WINDOW_CONTROL_CLOSE.to_string());
        let spans = vec![
            (storage[0].as_str(), muted_strong, true),
            (storage[1].as_str(), muted_strong, false),
            (storage[2].as_str(), muted_strong, true),
            (storage[3].as_str(), muted_strong, false),
            (storage[4].as_str(), red, true),
        ];
        Some((spans, right_rect))
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
            const MIN_COLS: u16 = 80;
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

    fn adjust_font_size(&mut self, delta: f32) {
        let cur = self
            .state
            .as_ref()
            .map(|s| s.text.font_size())
            .unwrap_or(self.font_size);
        self.set_font_size_absolute(cur + delta);
    }

    /// Bump opacity by `delta`, clamped to `0.0..=1.0`. Drives the
    /// `opacity_increase` / `opacity_decrease` built-in actions.
    fn adjust_opacity(&mut self, delta: f32) {
        if !delta.is_finite() {
            return;
        }
        let next = (self.opacity + delta).clamp(0.0, 1.0);
        if (next - self.opacity).abs() < 0.001 {
            return;
        }
        self.opacity = next;
        if let Some(state) = self.state.as_mut() {
            state.set_opacity(next);
        }
    }

    fn set_font_size_absolute(&mut self, size: f32) {
        if let Some(state) = self.state.as_mut() {
            state.text.set_font_size(size);
            // Force a redraw and reflow every pane to the new cell metrics.
            state.window.request_redraw();
        }
        self.sync_terminal_size();
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
                let _ = tab.tree.close_leaf(&path);
                // Reset focus if it pointed into the removed subtree.
                if tab.focus_path.len() >= path.len() && tab.focus_path[..path.len()] == path[..] {
                    let parent = if path.is_empty() {
                        Vec::new()
                    } else {
                        path[..path.len() - 1].to_vec()
                    };
                    tab.focus_path = descend_leftmost(&tab.tree, &parent);
                }
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
                    let initial = self.font_size;
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

    /// Compute the outer rect inside which panes are laid out (below the
    /// header, with padding).
    fn outer_rect(&self) -> Option<PaneRect> {
        let state = self.state.as_ref()?;
        let w = state.config.width as f32;
        let h = state.config.height as f32;
        let header_h = state.text.header_height();
        // Reserve room for the bottom bar only when something is
        // actively rendering there (search prompt OR scrollback
        // position indicator). Otherwise the pane area extends fully
        // to the bottom edge.
        let bar_h = state.text.bottom_bar_height(self.bottom_bar_reserves_space());
        let top = PAD + header_h;
        let bottom_reserved = if bar_h > 0.0 { PAD + bar_h } else { PAD };
        Some(PaneRect {
            left: PAD,
            top,
            width: (w - 2.0 * PAD).max(0.0),
            height: (h - top - bottom_reserved).max(0.0),
        })
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
        let new_ratio = match dir {
            SplitDir::Horizontal => {
                let usable = (containing.width - SPLIT_GAP).max(1.0);
                let new_a = ((x - containing.left as f64) as f32 - SPLIT_GAP * 0.5)
                    .clamp(20.0, usable - 20.0);
                (new_a / usable).clamp(0.05, 0.95)
            }
            SplitDir::Vertical => {
                let usable = (containing.height - SPLIT_GAP).max(1.0);
                let new_a = ((y - containing.top as f64) as f32 - SPLIT_GAP * 0.5)
                    .clamp(20.0, usable - 20.0);
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
                    if let Some(sel) = self.selection.as_mut() {
                        sel.focus = p;
                    }
                    self.mouse_dragging = true;
                    return false;
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
                pane.io.write_input(&bytes);
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
                self.selection = Some(ActiveSelection {
                    pane_idx: i,
                    anchor: p,
                    focus: p,
                    mode: SelectionMode::Char,
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
                    self.selection = Some(ActiveSelection {
                        pane_idx: i,
                        anchor: sel.start,
                        focus: end_inclusive,
                        mode: SelectionMode::Word,
                        pivot: Some((sel.start, end_inclusive)),
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
                    self.selection = Some(ActiveSelection {
                        pane_idx: i,
                        anchor: sel.start,
                        focus: end_inclusive,
                        mode: SelectionMode::Line,
                        pivot: Some((sel.start, end_inclusive)),
                    });
                    self.mouse_dragging = true;
                }
            }
        }
        false
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
                            pane.io.write_input(&bytes);
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
        let snapped = match mode {
            SelectionMode::Char => None,
            SelectionMode::Word => pivot.map(|piv| {
                let (drag_start, drag_end_incl) = self
                    .word_selection_at(pane_idx, p)
                    .map(|w| (w.start, SelectionPoint { row: w.end.row, col: w.end.col.saturating_sub(1) }))
                    .unwrap_or((p, p));
                snap_drag_to_range(piv, drag_start, drag_end_incl, false)
            }),
            SelectionMode::Line => pivot.map(|piv| {
                let (drag_start, drag_end_incl) = self
                    .line_selection_at(pane_idx, p)
                    .map(|l| (l.start, SelectionPoint { row: l.end.row, col: l.end.col.saturating_sub(1) }))
                    .unwrap_or((
                        SelectionPoint { row: p.row, col: 0 },
                        SelectionPoint { row: p.row, col: 0 },
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
                None => sel.focus = p,
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
                        pane.io.write_input(&bytes);
                    }
                }
            }
            return;
        }
        self.mouse_dragging = false;
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
    fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let tab = self.active_tab()?;
        let pane = tab.pane_at(sel.pane_idx)?;
        let offset = pane.scroll_offset.load(Ordering::Relaxed);
        let norm = sel.normalized();
        let mut text = String::new();
        if let Ok(term) = pane.terminal.lock() {
            for r in norm.start.row..=norm.end.row {
                let Some(row) = term.visible_row(offset, r) else { continue };
                let lo = if r == norm.start.row { norm.start.col as usize } else { 0 };
                let hi = if r == norm.end.row {
                    (norm.end.col as usize).min(row.len())
                } else {
                    row.len()
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
                if r < norm.end.row {
                    text.push('\n');
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
        pane.scroll_offset.store(next as u16, Ordering::Relaxed);
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
                let initial = self.font_size;
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
                    if let Ok(mut t) = pane.terminal.lock() {
                        t.advance(b"\x1bc");
                    }
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

    /// Snap the window to one half of the current monitor. On
    /// platforms with positionable windows (X11, Win32, macOS, BSD-X11)
    /// this calls `set_outer_position` + `set_inner_size`. On Wayland
    /// `set_outer_position` is a no-op, so we fall back to
    /// `set_maximized(true)` for `Top` and skip the rest with a warning
    /// (compositor-level snap is the supported path there).
    fn snap_window(&mut self, dir: SnapDir) {
        let Some(state) = self.state.as_ref() else { return };
        let Some(monitor) = state.window.current_monitor() else {
            tracing::warn!("window snap: no current monitor — skipping");
            return;
        };
        let mon_size = monitor.size();
        let mon_pos = monitor.position();
        let mw = mon_size.width as i32;
        let mh = mon_size.height as i32;
        // Half rounded to even pixels so the two halves tile cleanly.
        let half_w = (mw / 2).max(320);
        let half_h = (mh / 2).max(200);
        let (target_pos, target_size) = match dir {
            SnapDir::Left => ((mon_pos.x, mon_pos.y), (half_w, mh)),
            SnapDir::Right => ((mon_pos.x + mw - half_w, mon_pos.y), (half_w, mh)),
            SnapDir::Top => ((mon_pos.x, mon_pos.y), (mw, half_h)),
            SnapDir::Bottom => (
                (mon_pos.x, mon_pos.y + mh - half_h),
                (mw, half_h),
            ),
        };
        // Unmaximize first so the position/size calls actually
        // apply — set_outer_position is silently dropped while
        // maximized on some platforms.
        if state.window.is_maximized() {
            state.window.set_maximized(false);
        }
        let pos = winit::dpi::PhysicalPosition::new(target_pos.0, target_pos.1);
        let size = winit::dpi::PhysicalSize::new(target_size.0 as u32, target_size.1 as u32);
        state.window.set_outer_position(pos);
        let _ = state.window.request_inner_size(size);
        tracing::info!(?dir, ?target_pos, ?target_size, "window snap requested");
    }

    /// Guake-style drop-down: toggle between a "dropped" state (sized
    /// per `[guake]` config, anchored to the configured edge, raised
    /// above other windows) and a minimised state. Disabled when
    /// `[guake].enabled = false` (no-op).
    ///
    /// Platform notes: Wayland disallows app-controlled positioning;
    /// the function falls back to `set_maximized(true)` for `top` /
    /// `full` and warns once for `bottom`. X11 / Win32 / macOS honour
    /// `set_outer_position` and the window lands on the requested edge.
    fn toggle_guake(&mut self) {
        let cfg = match &self.guake {
            Some(c) if c.enabled => c.clone(),
            _ => {
                tracing::debug!("toggle_guake: [guake] disabled in config — no-op");
                return;
            }
        };
        let Some(state) = self.state.as_ref() else { return };
        // Drive the toggle off the explicit `guake_dropped` flag rather
        // than `is_minimized()`. The first press on a freshly-launched
        // (visible, not minimised) window must DROP DOWN, not minimise
        // — and `is_minimized() == false` would otherwise route that
        // first press to the "hide" branch.
        if !self.guake_dropped {
            // Drop down.
            state.window.set_minimized(false);
            if state.window.is_maximized() {
                state.window.set_maximized(false);
            }
            if let Some(monitor) = state.window.current_monitor() {
                let mon_size = monitor.size();
                let mon_pos = monitor.position();
                let mw = mon_size.width as i32;
                let mh = mon_size.height as i32;
                let w_pct = (cfg.width_pct.clamp(20, 100)) as i32;
                let h_pct = (cfg.height_pct.clamp(10, 100)) as i32;
                let target_w = (mw * w_pct / 100).max(320);
                let target_h = (mh * h_pct / 100).max(200);
                let centre_x = mon_pos.x + (mw - target_w) / 2;
                match cfg.position.as_str() {
                    "full" => {
                        state.window.set_maximized(true);
                    }
                    "bottom" => {
                        let pos = winit::dpi::PhysicalPosition::new(
                            centre_x,
                            mon_pos.y + mh - target_h,
                        );
                        state.window.set_outer_position(pos);
                        let _ = state.window.request_inner_size(
                            winit::dpi::PhysicalSize::new(target_w as u32, target_h as u32),
                        );
                    }
                    _ => {
                        // Default + "top".
                        let pos = winit::dpi::PhysicalPosition::new(centre_x, mon_pos.y);
                        state.window.set_outer_position(pos);
                        let _ = state.window.request_inner_size(
                            winit::dpi::PhysicalSize::new(target_w as u32, target_h as u32),
                        );
                    }
                }
            }
            // Raise above other windows + take focus so the user can
            // start typing immediately.
            state
                .window
                .set_window_level(winit::window::WindowLevel::AlwaysOnTop);
            state.window.focus_window();
            self.guake_dropped = true;
            tracing::info!(position = %cfg.position, "guake: dropped");
        } else {
            // Hide. Stick with `set_minimized(true)` rather than
            // `set_visible(false)` — minimised windows show in the
            // taskbar so the user has a path back even without a
            // global hotkey.
            state.window.set_minimized(true);
            // Drop the always-on-top so the next non-guake show
            // doesn't surprise the user by sticking above everything.
            state
                .window
                .set_window_level(winit::window::WindowLevel::Normal);
            self.guake_dropped = false;
            tracing::info!("guake: hidden");
        }
    }

    /// Restore the window to a centered default size on the current
    /// monitor. Cancels maximize / snap.
    fn restore_window(&mut self) {
        let Some(state) = self.state.as_ref() else { return };
        if state.window.is_maximized() {
            state.window.set_maximized(false);
        }
        let (w, h) = self.initial_size;
        let size = winit::dpi::PhysicalSize::new(w, h);
        if let Some(monitor) = state.window.current_monitor() {
            let mon_size = monitor.size();
            let mon_pos = monitor.position();
            let cx = mon_pos.x + ((mon_size.width as i32 - w as i32) / 2).max(0);
            let cy = mon_pos.y + ((mon_size.height as i32 - h as i32) / 2).max(0);
            state
                .window
                .set_outer_position(winit::dpi::PhysicalPosition::new(cx, cy));
        }
        let _ = state.window.request_inner_size(size);
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

    /// Pixel rect for the rename overlay. Centered, narrower than the
    /// help/settings overlay since it only holds one input field.
    fn rename_rect(&self) -> Option<PaneRect> {
        let state = self.state.as_ref()?;
        let w = state.config.width as f32;
        let h = state.config.height as f32;
        let max_w = 480.0_f32.min(w - 40.0).max(100.0);
        let max_h = 160.0_f32.min(h - 40.0).max(60.0);
        if w < max_w + 1.0 || h < max_h + 1.0 {
            return None;
        }
        Some(PaneRect {
            left: (w - max_w) * 0.5,
            top: (h - max_h) * 0.5,
            width: max_w,
            height: max_h,
        })
    }

    /// Keyboard handler for the rename overlay. Supports basic line
    /// editing: arrows + Home/End move the caret, Ctrl+W deletes the
    /// previous word, Ctrl+U clears, Backspace pops one grapheme. The
    /// "pristine" flag mimics browser select-all-on-focus: the first
    /// printable character replaces the prefilled title in one shot.
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
        pane.scroll_offset.store(offset as u16, Ordering::Relaxed);
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
        // Wheel over the tab bar switches tabs (Firefox/Chrome
        // convention). Wheel up = previous tab, down = next tab.
        // Falls through to pane-scroll when outside the header.
        if let Some(rect) = self.header_rect() {
            let y = self.cursor_pos.y as f32;
            if y >= rect.top && y < rect.top + rect.height {
                let dir = -step.signum() as isize;
                self.switch_tab(dir);
                return;
            }
        }
        let Some(pane) = self.focused_pane() else { return };
        // If the shell wants mouse events, forward wheel as button 64 / 65.
        if let Some((_mode, sgr)) = mouse_mode_for(pane) {
            let focused_idx = self
                .active_tab()
                .and_then(|t| t.focused_index())
                .unwrap_or(0);
            let p = self
                .pixel_to_cell(focused_idx, self.cursor_pos.x, self.cursor_pos.y)
                .unwrap_or(SelectionPoint { row: 0, col: 0 });
            let button = if step > 0 { 64 } else { 65 };
            for _ in 0..step.unsigned_abs() {
                let bytes = encode_mouse(sgr, button, p.col, p.row, true);
                pane.io.write_input(&bytes);
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
        pane.scroll_offset.store(next as u16, Ordering::Relaxed);
        self.events.emit("scroll", &next.to_string());
    }

    fn paste_clipboard(&self) {
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
    fn paste_primary(&self) {
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
    fn write_paste(&self, text: &str) {
        let Some(pane) = self.focused_pane() else { return };
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
            pane.io.write_input(&out);
        } else {
            pane.io.write_input(to_send.as_bytes());
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
                pane.io.write_input(&bytes);
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
                        pane.io.write_input(&[0x1b, m]);
                    } else {
                        pane.io.write_input(&[m]);
                    }
                    return;
                }
            }
            if alt {
                let mut out = Vec::with_capacity(text.len() + 1);
                out.push(0x1b);
                out.extend_from_slice(text.as_bytes());
                pane.io.write_input(&out);
            } else {
                pane.io.write_input(text.as_bytes());
            }
        }
    }

    /// Top-level keyboard entry. Returns `true` if the window should exit.
    fn handle_key(&mut self, event: &KeyEvent) -> bool {
        if event.state != ElementState::Pressed {
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
                            let trimmed = p.query.trim_end();
                            let drop_from = trimmed
                                .rfind(char::is_whitespace)
                                .map(|i| i + 1)
                                .unwrap_or(0);
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

    fn palette_overlay_spans<'a>(
        &self,
        storage: &'a mut Vec<String>,
    ) -> Option<Vec<(&'a str, [u8; 3], bool)>> {
        let p = self.palette.as_ref()?;
        storage.clear();
        let muted = palette::default_fg().map(|c| c.saturating_sub(60));
        let accent: [u8; 3] = [220, 200, 120];
        let key_color: [u8; 3] = [121, 192, 255];

        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        storage.push(format!("  ▸ {}", p.query));
        spans.push((storage.len() - 1, accent, true));
        // Count tag on the same line so users see the result total
        // without scrolling. Hidden when the query is empty (every
        // action matches by default; the count is just noise then).
        if !p.query.is_empty() {
            let n = p.filtered.len();
            storage.push(format!(
                "   ({} {})\n",
                n,
                if n == 1 { "match" } else { "matches" },
            ));
            spans.push((storage.len() - 1, muted, false));
            storage.push("\n".to_string());
            spans.push((storage.len() - 1, muted, false));
        } else {
            storage.push("\n\n".to_string());
            spans.push((storage.len() - 1, muted, false));
        }

        if p.filtered.is_empty() {
            storage.push("    (no matches)".to_string());
            spans.push((storage.len() - 1, muted, false));
        } else {
            let total = p.filtered.len();
            let visible = self.palette_visible_rows();
            let start = p.scroll_offset.min(total);
            let end = (start + visible).min(total);
            // "↑ N more" hint when there are items above the viewport.
            if start > 0 {
                storage.push(format!("    ↑ {} more\n", start));
                spans.push((storage.len() - 1, muted, false));
            }
            for cmd_idx_pos in start..end {
                let cmd_idx = p.filtered[cmd_idx_pos];
                let (label, is_custom) = if cmd_idx < AppAction::ALL.len() {
                    (AppAction::ALL[cmd_idx].1.to_string(), false)
                } else {
                    (
                        format!("⚙ {}", p.custom[cmd_idx - AppAction::ALL.len()]),
                        true,
                    )
                };
                let selected = cmd_idx_pos == p.selected;
                let line = if selected {
                    format!("  ► {}\n", label)
                } else {
                    format!("    {}\n", label)
                };
                storage.push(line);
                let color = if selected {
                    key_color
                } else if is_custom {
                    accent
                } else {
                    muted
                };
                spans.push((storage.len() - 1, color, selected));
            }
            // "↓ N more" hint when there are items below the viewport.
            if end < total {
                storage.push(format!("    ↓ {} more  (PgDn / End)\n", total - end));
                spans.push((storage.len() - 1, muted, false));
            }
        }
        Some(
            spans
                .into_iter()
                .map(|(idx, color, bold)| (storage[idx].as_str(), color, bold))
                .collect(),
        )
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
                            // Trim trailing whitespace, then trim a run of
                            // non-whitespace word chars.
                            let trimmed = s.query.trim_end();
                            let drop_from = trimmed
                                .rfind(char::is_whitespace)
                                .map(|i| i + 1)
                                .unwrap_or(0);
                            s.query.truncate(drop_from);
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
    pivot: (SelectionPoint, SelectionPoint),
    drag_start: SelectionPoint,
    drag_end_incl: SelectionPoint,
    row_only: bool,
) -> (SelectionPoint, SelectionPoint) {
    let (piv_start, piv_end_incl) = pivot;
    let backward = if row_only {
        drag_start.row < piv_start.row
    } else {
        (drag_start.row, drag_start.col) < (piv_start.row, piv_start.col)
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

/// Write `text` to the system clipboard.
///
/// On macOS / Windows the OS stores the bytes directly, so a one-shot
/// `set_text` is enough.
///
/// On Linux (X11 / Wayland) the protocol ties selection ownership to a
/// live client connection: the process that called `set()` has to
/// remain reachable until another client requests the data. arboard's
/// `set().wait()` blocks the calling thread until that handover
/// happens. We need to call `wait()`, but spawning a *new* thread on
/// every Ctrl+Shift+C accumulates idle threads indefinitely if the
/// user copies a lot without any other application pasting (a real
/// failure mode reported during the audit: each thread sits idle
/// holding ~8 MB of virtual address space).
///
/// Hand the actual `wait()` to a single, lazily-started owner thread.
/// Callers write the latest text into a slot guarded by a `Mutex` +
/// `Condvar`; the worker takes the slot, runs `set().wait()`, and
/// loops back. Subsequent `clipboard_set` calls during a long-running
/// `wait()` overwrite the slot in place — the worker picks up the
/// freshest text once `wait()` returns. The slot is bounded to one
/// entry, so memory cannot grow.
#[cfg(target_os = "linux")]
fn clipboard_set(text: &str) {
    use arboard::SetExtLinux;
    use std::sync::{Condvar, Mutex, OnceLock};
    struct Slot {
        pending: Mutex<Option<String>>,
        cv: Condvar,
    }
    static SLOT: OnceLock<&'static Slot> = OnceLock::new();
    let slot = SLOT.get_or_init(|| {
        let s: &'static Slot = Box::leak(Box::new(Slot {
            pending: Mutex::new(None),
            cv: Condvar::new(),
        }));
        std::thread::spawn(move || loop {
            let text = {
                let mut g = s.pending.lock().unwrap_or_else(|e| e.into_inner());
                while g.is_none() {
                    g = s.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                g.take().unwrap_or_default()
            };
            let mut cb = match arboard::Clipboard::new() {
                Ok(cb) => cb,
                Err(e) => {
                    // We already took `text` out of the slot. Surface
                    // the failure — silently dropping the user's copy
                    // because the display server connection refused
                    // would otherwise look like the keystroke had no
                    // effect. Truncate the dropped payload to avoid
                    // logging a megabyte selection at warn level.
                    let preview_len = text.chars().take(80).count();
                    let preview: String = text.chars().take(preview_len).collect();
                    let elided = if text.chars().nth(80).is_some() { "…" } else { "" };
                    tracing::warn!(
                        error = %e,
                        dropped_bytes = text.len(),
                        preview = %format!("{preview}{elided}"),
                        "clipboard owner: Clipboard::new() failed; dropping queued copy",
                    );
                    continue;
                }
            };
            // wait() blocks until another client takes the selection.
            // While we block here, fresh `clipboard_set` calls keep
            // overwriting `pending` — the next loop iteration will
            // pick up the latest, not the queue of historical copies.
            if let Err(e) = cb.set().wait().text(text) {
                tracing::warn!("clipboard set failed: {e}");
            }
        });
        s
    });
    {
        let mut g = slot.pending.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(text.to_string());
    }
    slot.cv.notify_one();
}

#[cfg(not(target_os = "linux"))]
fn clipboard_set(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        if let Err(e) = cb.set_text(text.to_string()) {
            tracing::warn!("clipboard set failed: {e}");
        }
    }
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

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = WindowAttributes::default()
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
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("window create failed: {e}");
                event_loop.exit();
                return;
            }
        };
        match pollster::block_on(GpuState::new(
            window.clone(),
            self.font_size,
            self.font_family.clone(),
            self.opacity,
        )) {
            Ok(state) => {
                self.state = Some(state);
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
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(state) = self.state.as_mut() {
                    state.resize(size.width, size.height);
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
            WindowEvent::ModifiersChanged(Modifiers { .. }) => {
                if let WindowEvent::ModifiersChanged(m) = &event {
                    self.modifiers = m.state();
                }
            }
            WindowEvent::Focused(focused) => {
                self.window_focused = focused;
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
                            pane.io.write_input(seq);
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
                            pane.scroll_offset.store(next as u16, Ordering::Relaxed);
                            if let Some(sel) = self.selection.as_mut() {
                                sel.focus = if dir < 0 {
                                    SelectionPoint { row: 0, col: 0 }
                                } else {
                                    SelectionPoint { row: last_row, col: last_col }
                                };
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
                            pane.io.write_input(resp.as_bytes());
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

                // Forward any plugin-queued input to the focused pane's PTY.
                let queued = self.events.drain_pending_input();
                if !queued.is_empty() {
                    if let Some(pane) = self.focused_pane() {
                        for bytes in queued {
                            pane.io.write_input(&bytes);
                        }
                    }
                }

                // Plugin-addressed input by stable uid. Walk live panes
                // to find the matching uid and forward bytes. Unknown
                // uid silently dropped — plugin may have queued a write
                // after the target pane exited.
                for (uid, bytes) in self.events.drain_pending_routed_input_by_uid() {
                    match self.find_pane_by_uid(uid) {
                        Some(p) => p.io.write_input(&bytes),
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
                        pane.io.write_input(&bytes);
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

                // Plugin-emitted notifications (`rterm.notify(msg)`). Same
                // path OSC 9 takes: fire the `notification` event and ping
                // the taskbar when the window is unfocused.
                for msg in self.events.drain_pending_notify() {
                    self.events.emit("notification", &msg);
                    if !self.window_focused {
                        if let Some(s) = self.state.as_ref() {
                            s.window.request_user_attention(Some(
                                winit::window::UserAttentionType::Informational,
                            ));
                        }
                    }
                }

                // Built-in actions invoked from Lua via `rterm.run_action`.
                // We resolve each name; unknown names get a debug log and
                // are dropped. The exit signal still bubbles up.
                let mut plugin_exit = false;
                for name in self.events.drain_pending_actions() {
                    if let Some(act) = AppAction::from_name(&name) {
                        if self.dispatch_action(act) {
                            plugin_exit = true;
                        }
                    } else {
                        tracing::debug!(action = %name, "run_action: unknown");
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

                // Custom events relayed back through the App so registered
                // handlers fire normally.
                for (name, body) in self.events.drain_pending_custom_events() {
                    self.events.emit(&name, &body);
                }

                // Plugin-requested new tabs with optional cwd override.
                let new_tabs = self.events.drain_pending_new_tabs();
                for cwd in new_tabs {
                    self.new_tab_in(cwd.as_deref());
                }

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

                // Plugin-requested splits with optional cwd override.
                for (dir_str, cwd) in self.events.drain_pending_splits() {
                    let dir = match dir_str.as_str() {
                        "h" | "horizontal" => SplitDir::Horizontal,
                        "v" | "vertical" => SplitDir::Vertical,
                        "auto" | "smart" => self.split_auto_direction(),
                        other => {
                            tracing::debug!(dir = %other, "rterm.split: unknown direction");
                            continue;
                        }
                    };
                    self.split_active_pane_in(dir, cwd.as_deref());
                }

                // Plugin-requested URL opens. Same scheme whitelist as
                // mouse / keyboard hover-open: plugins are user-authored
                // and trusted, but the URL inside `rterm.open_url(...)`
                // might be assembled from shell output — applying the
                // same filter keeps the trust boundary at the system
                // handler, not at the plugin author.
                for url in self.events.drain_pending_open_urls() {
                    if !rterm_core::is_safe_url(&url) {
                        tracing::warn!(url = %url, "blocked plugin open_url with disallowed scheme");
                        self.events.emit("link.blocked", &url);
                        continue;
                    }
                    match open::that_detached(&url) {
                        Ok(_) => self.events.emit("link.open", &url),
                        Err(e) => tracing::warn!(url = %url, "open_url failed: {e}"),
                    }
                }

                // Plugin-requested pane kills by stable uid. Walk live
                // panes for each uid, flip `alive` on the match — same
                // prune path as the index-based kills below. Unknown
                // uid silently dropped (plugin may have queued kill on
                // a pane that already exited).
                for uid in self.events.drain_pending_kills_by_uid() {
                    'outer: for tab in &self.tabs {
                        for pane in tab.panes() {
                            if pane.uid == uid {
                                pane.alive.store(false, Ordering::Release);
                                break 'outer;
                            }
                        }
                    }
                }

                // Plugin-requested pane kills. Flip the `alive` flag so the
                // standard prune path closes the leaf next frame.
                for (tab_idx, pane_idx) in self.events.drain_pending_kills() {
                    if let Some(pane) = self
                        .tabs
                        .get(tab_idx)
                        .and_then(|t| t.pane_at(pane_idx))
                    {
                        pane.alive.store(false, Ordering::Release);
                    }
                }

                // Plugin-requested tab kills: mark every pane in the tab
                // dead so the same prune path collapses it. Doing it via
                // `alive` keeps a single tear-down code path and preserves
                // any "save scrollback on exit" hooks per pane.
                for tab_idx in self.events.drain_pending_tab_kills() {
                    if let Some(tab) = self.tabs.get(tab_idx) {
                        for pane in tab.panes() {
                            pane.alive.store(false, Ordering::Release);
                        }
                    }
                }

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
                        self.flash_until = Some(Instant::now() + Duration::from_millis(100));
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
                // line-ending normalisation and bracketed-paste-marker
                // stripping applies as for clipboard / right-click pastes.
                // Lossy UTF-8 conversion is fine: Lua strings are usually
                // valid UTF-8, and non-text bytes have no defensible meaning
                // when fed to a shell anyway.
                for bytes in self.events.drain_pending_paste() {
                    let text = String::from_utf8_lossy(&bytes);
                    self.write_paste(&text);
                }

                // Plugin-driven scroll deltas — applied to the focused pane,
                // clamped to its scrollback length. Sums via saturating_add
                // so `i32::MIN` (used by `scroll_to_live`) doesn't overflow.
                let scrolls = self.events.drain_pending_scroll();
                if !scrolls.is_empty() {
                    if let Some(pane) = self.focused_pane() {
                        let max = pane
                            .terminal
                            .lock()
                            .ok()
                            .map(|t| t.scrollback_len() as i32)
                            .unwrap_or(0);
                        let cur = pane.scroll_offset.load(Ordering::Relaxed) as i32;
                        let sum = scrolls
                            .iter()
                            .fold(0i32, |acc, d| acc.saturating_add(*d));
                        let next = cur.saturating_add(sum).clamp(0, max).max(0) as u16;
                        pane.scroll_offset.store(next, Ordering::Relaxed);
                        self.events.emit("scroll", &next.to_string());
                    }
                }

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
                        let pane_text = pane
                            .terminal
                            .lock()
                            .ok()
                            .map(|t| grid_text_snapshot(&t))
                            .unwrap_or_default();
                        // Per-pane scrollback tail (capped). Skipped on
                        // alt screen — the ring belongs to the suspended
                        // primary screen and surfacing it would mix in
                        // stale content the user isn't looking at.
                        let pane_tail = if alt {
                            String::new()
                        } else {
                            pane.terminal
                                .lock()
                                .ok()
                                .map(|t| {
                                    scrollback_text_snapshot_capped(&t, SCROLLBACK_TAIL_MAX)
                                })
                                .unwrap_or_default()
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
                        let text = grid_text_snapshot(&t);
                        let font_size = self
                            .state
                            .as_ref()
                            .map(|s| s.text.font_size())
                            .unwrap_or(self.font_size);
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
                        let scrollback_text = if t.is_on_alt_screen() {
                            String::new()
                        } else {
                            scrollback_text_snapshot(&t)
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
                        self.flash_until = Some(Instant::now() + Duration::from_millis(100));
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

                // OSC 52 clipboard write request: take from focused pane only.
                let osc52 = self
                    .tabs
                    .get(self.active_tab)
                    .and_then(|t| t.focused_pane())
                    .and_then(|p| p.terminal.lock().ok().and_then(|mut t| t.take_pending_clipboard()));
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
                    if let Some(rt) = self.rename_tab.clone() {
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
                    let sel_normalized = self.selection.map(|s| (s.pane_idx, s.normalized()));
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
                            }
                        })
                        .collect();
                    // The main header text (tabs / hamburger / `+`)
                    // lives in the BOTTOM row of the header — the tab
                    // strip — not the title bar row. Render with
                    // `tab_strip_rect` so vertical centering inside the
                    // text buffer lands on the right strip.
                    let _ = header_rect;
                    let header_draw = tab_strip_rect_for_draw.map(|rect| HeaderDraw {
                        spans: header_spans,
                        rect,
                    });
                    let header_right_draw =
                        header_right.map(|(spans, rect)| HeaderRightDraw { spans, rect });
                    let title_bar_draw =
                        title_bar.map(|(spans, rect)| TitleBarDraw { spans, rect });
                    let status_bar_draw =
                        status_bar.map(|(spans, rect)| StatusBarDraw { spans, rect });
                    let overlay_draw = overlay_rect.map(|rect| OverlayDraw {
                        spans: overlay_spans,
                        rect,
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
                    let flash = self
                        .flash_until
                        .map(|t| t > Instant::now())
                        .unwrap_or(false);
                    if !flash {
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
                    state.window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

pub fn run(cfg: RunConfig) -> Result<()> {
    let event_loop = EventLoop::new().context("EventLoop::new")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(cfg);
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
        let s = NormSelection { start: pt(1, 2), end: pt(3, 5) };
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
        let s = NormSelection { start: pt(2, 3), end: pt(2, 7) };
        assert!(!s.contains(2, 2));
        assert!(s.contains(2, 3));
        assert!(s.contains(2, 6));
        assert!(!s.contains(2, 7));
    }

    #[test]
    fn active_selection_normalizes_swapped_endpoints() {
        let s = ActiveSelection {
            pane_idx: 0,
            anchor: pt(5, 3),
            focus: pt(2, 10),
            mode: SelectionMode::Char,
            pivot: None,
        };
        let n = s.normalized();
        assert_eq!(n.start, pt(2, 10));
        assert_eq!(n.end, pt(5, 3));
    }

    #[test]
    fn snap_word_drag_forward_uses_pivot_start_and_drag_end() {
        // Pivot word "hello" at row 5, cols 0..=4.
        // User drags right onto word "world" at cols 6..=10.
        let pivot = (pt(5, 0), pt(5, 4));
        let (anchor, focus) =
            snap_drag_to_range(pivot, pt(5, 6), pt(5, 10), false);
        assert_eq!(anchor, pt(5, 0));
        assert_eq!(focus, pt(5, 10));
        // Normalized: cells 0..10 inclusive (covers both words).
        let active = ActiveSelection {
            pane_idx: 0,
            anchor,
            focus,
            mode: SelectionMode::Word,
            pivot: Some(pivot),
        };
        let n = active.normalized();
        assert_eq!(n.start, pt(5, 0));
        assert_eq!(n.end, pt(5, 10));
    }

    #[test]
    fn snap_word_drag_backward_swaps_anchor_to_pivot_end() {
        // Pivot word at cols 6..=10. Drag left onto word at cols 0..=4.
        let pivot = (pt(5, 6), pt(5, 10));
        let (anchor, focus) =
            snap_drag_to_range(pivot, pt(5, 0), pt(5, 4), false);
        assert_eq!(anchor, pt(5, 10));
        assert_eq!(focus, pt(5, 0));
        // Normalized covers the union of both words.
        let active = ActiveSelection {
            pane_idx: 0,
            anchor,
            focus,
            mode: SelectionMode::Word,
            pivot: Some(pivot),
        };
        let n = active.normalized();
        assert_eq!(n.start, pt(5, 0));
        assert_eq!(n.end, pt(5, 10));
    }

    #[test]
    fn snap_line_drag_forward_extends_to_drag_row() {
        // Pivot is line 3 cols 0..=20. Drag down to line 7 cols 0..=15.
        let pivot = (pt(3, 0), pt(3, 20));
        let (anchor, focus) =
            snap_drag_to_range(pivot, pt(7, 0), pt(7, 15), true);
        assert_eq!(anchor, pt(3, 0));
        assert_eq!(focus, pt(7, 15));
    }

    #[test]
    fn snap_line_drag_within_same_row_does_not_flip() {
        // Drag column changes within pivot's row must NOT flip direction.
        let pivot = (pt(3, 0), pt(3, 20));
        let (anchor, focus) =
            snap_drag_to_range(pivot, pt(3, 0), pt(3, 5), true);
        // row_only forward path is taken because drag.row == pivot.row.
        assert_eq!(anchor, pt(3, 0));
        assert_eq!(focus, pt(3, 5));
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
