//! Plugin-event payload builders and text snapshots — the pure
//! `String`-producing helpers that format the JSON-ish payloads carried
//! by pane / tab / progress events, plus the scrollback / grid text
//! snapshots and the cursor-shape / mouse-mode name→code maps. Extracted
//! from `lib.rs`; all callers live in `event_loop.rs`. No `self`, no
//! I/O — trivially unit-testable in isolation.

use rterm_core::{CellAttrs, Terminal};

use crate::{SplitDir, CURSOR_SHAPE_NAMES, MOUSE_MODE_NAMES, SCROLLBACK_SNAPSHOT_MAX};

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
pub(crate) fn pane_split_payload(
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
pub(crate) fn pane_attr_payload<V: std::fmt::Display>(
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
pub(crate) fn pane_edge_payload(tab_1based: usize, pane_1based: usize, uid: u64) -> String {
    format!("{}:{}\t{}", tab_1based, pane_1based, uid)
}

/// Format the payload for a pane-text event:
/// `<tab+1>:<pane+1>\t<text>`. Used by `pane.title` and
/// `pane.cwd`. Semantically distinct from `pane_edge_payload`
/// (which carries a uid) so legacy parsers that splice the
/// trailing field into a UI label don't accidentally inherit a
/// numeric uid when the schema looks identical at the byte
/// level.
pub(crate) fn pane_text_payload(tab_1based: usize, pane_1based: usize, text: &str) -> String {
    format!("{}:{}\t{}", tab_1based, pane_1based, text)
}

/// Format the payload for a tab-level event that carries the
/// tab's focused-pane uid: `<tab+1>\t<uid>`. Shared by
/// `tab.switch` and `tab.alt_enter` / `tab.alt_leave` so uid-aware
/// plugins can answer "is my watched pane in this tab?" without
/// re-resolving the tab's contents.
pub(crate) fn tab_event_payload(tab_1based: usize, uid: u64) -> String {
    format!("{}\t{}", tab_1based, uid)
}

/// Format the payload for `tab.title`: `<tab+1>\t<title>`. Same
/// byte shape as `tab_event_payload` but the trailing field is a
/// free-form string rather than a uid — kept separate so a future
/// schema change (e.g. uid suffix) only touches one site, and so
/// the reader sees the intent of the trailing payload.
pub(crate) fn tab_title_payload(tab_1based: usize, title: &str) -> String {
    format!("{}\t{}", tab_1based, title)
}

/// Format the payload for `tab.progress`:
/// `<tab+1>\t<state>\t<pct>`. State 0 with pct 0 means "cleared"
/// (the aggregate went from Some progress to None this frame).
pub(crate) fn tab_progress_payload(tab_1based: usize, state: u8, percent: u8) -> String {
    format!("{}\t{}\t{}", tab_1based, state, percent)
}

/// Format the payload for `tab.drag_end`:
/// `<src_tab+1>\t<moved>` where `moved` is "true" / "false".
/// Plugins read this to distinguish a canceled drag (release
/// on the same tab) from a successful reorder.
pub(crate) fn tab_drag_end_payload(src_1based: usize, moved: bool) -> String {
    format!("{}\t{}", src_1based, moved)
}

/// Format the payload for the OSC 9;4 `progress` event:
/// `<tab+1>:<pane+1>\t<state>\t<percent>`. State 0 means "clear"
/// (percent ignored). Kept here so a future schema extension
/// (e.g. uid suffix) lands in one place — currently the event has
/// the same prefix as `pane_edge_payload` so legacy parsers still
/// extract tab/pane.
pub(crate) fn progress_payload(tab_1based: usize, pane_1based: usize, state: u8, percent: u8) -> String {
    format!("{}:{}\t{}\t{}", tab_1based, pane_1based, state, percent)
}

/// Format the payload for pane events that carry a value BEFORE the
/// uid: `<tab+1>:<pane+1>\t<value>\t<uid>`. Used by `pane.bell_mute`
/// (`value` = muted bool) and `pane.shell_exit` (`value` = exit
/// code). Opposite trailing order from `pane_attr_payload` —
/// preserved for backwards compatibility with plugins that already
/// parse these emissions in this order.
pub(crate) fn pane_value_uid_payload<V: std::fmt::Display>(
    tab_1based: usize,
    pane_1based: usize,
    value: V,
    uid: u64,
) -> String {
    format!("{}:{}\t{}\t{}", tab_1based, pane_1based, value, uid)
}

/// Encode a cursor-shape name into a stable u8 used by the per-pane
/// `last_cursor_shape` edge-trigger gate. Unknown values fall back
/// to `0` (block) so a bad rename in the snapshot path is silent in
/// production but still pinned by the helper's unit tests.
pub(crate) fn cursor_shape_code(name: &str) -> u8 {
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
pub(crate) fn mouse_mode_code(name: &str) -> u8 {
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
pub(crate) fn pane_command_finish_payload(
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

pub(crate) fn pane_exit_payload(
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

pub(crate) fn progress_severity(state: u8) -> u8 {
    match state {
        2 => 4,
        4 => 3,
        3 => 2,
        1 => 1,
        _ => 0,
    }
}

/// Snapshot the focused pane's recent scrollback as a `\n`-joined string.
/// Returns the most-recent `SCROLLBACK_SNAPSHOT_MAX` lines so plugins can
/// screen-scrape "what just rolled off the visible grid" without
/// subscribing to `pane.output` and maintaining their own buffer. Trailing
/// spaces on each line are stripped, mirroring `grid_text_snapshot`.
/// Empty scrollback yields the empty string (no allocation past header).
pub(crate) fn scrollback_text_snapshot(t: &Terminal) -> String {
    scrollback_text_snapshot_capped(t, SCROLLBACK_SNAPSHOT_MAX)
}

/// Same as `scrollback_text_snapshot` but with an explicit cap — used
/// for the per-pane (background) variant with a tighter limit so the
/// total per-frame allocation stays bounded as the pane count grows.
pub(crate) fn scrollback_text_snapshot_capped(t: &Terminal, max_lines: usize) -> String {
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

pub(crate) fn grid_text_snapshot(t: &Terminal) -> String {
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

