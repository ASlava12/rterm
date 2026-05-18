//! Multi-line paste safety prompt — modal dialog + inline editor.
//!
//! Pasting a chunk of text that contains newlines is a real footgun
//! in a terminal: each newline either becomes `\r` (legacy mode) and
//! executes whatever line preceded it as a shell command, or stays as
//! `\n` (bracketed paste) where it's still ambiguously close to a
//! submit. Modern terminals (kitty, iTerm2) intercept multi-line
//! clipboard payloads with a confirmation dialog; this module is
//! that for rterm.
//!
//! State machine:
//! * `Confirm` — three buttons: Paste / Edit / Cancel. Tab cycles,
//!   Enter activates the focused one. Mouse click selects + activates
//!   directly.
//! * `Edit` — minimal text editor backed by `String` + a byte cursor.
//!   Cursor navigation: ←/→ per char (UTF-8 boundary aware), Home/End
//!   per line, ↑/↓ per line. Backspace / Delete edit text. Newline
//!   submits a literal `\n`. Ctrl+Enter or the Apply button finishes
//!   editing and pastes.
//!
//! Both modes share the same overlay rect — the renderer picks the
//! span list based on `PasteConfirmation::mode` and routes keys /
//! mouse through `handle_key` / `handle_press`.

use std::sync::atomic::Ordering;

/// Owned modal state. `App` holds it in an `Option`.
#[derive(Debug, Clone)]
pub(crate) struct PasteConfirmation {
    /// The bytes the user wants to paste. `Confirm` mode keeps the
    /// original verbatim; `Edit` mode mutates this in place as the
    /// user edits. Always rendered as the dialog body.
    pub(crate) text: String,
    /// Pane that requested the paste. Stored as a uid so a pane
    /// switch / close doesn't redirect the paste — when the source
    /// pane no longer exists, the modal silently drops the paste
    /// on Apply.
    #[allow(dead_code)] // reserved for the "send to original pane even after focus change" follow-up
    pub(crate) pane_uid: u64,
    /// Whether we're showing the confirm dialog or the editor.
    pub(crate) mode: PasteMode,
}

#[derive(Debug, Clone)]
pub(crate) enum PasteMode {
    Confirm {
        /// Currently highlighted button. Tab / Shift+Tab cycle.
        selected: PasteButton,
    },
    Edit {
        /// Byte offset of the cursor inside `text`. Always on a
        /// UTF-8 char boundary (the editor enforces this on every
        /// mutation).
        cursor: usize,
        /// Top of the visible viewport, as a buffer line index.
        /// Persisted between frames so a click on a visible line
        /// doesn't trigger a re-centring scroll that visually
        /// drops the cursor in the middle of a fresh viewport
        /// instead of where the user clicked. Updated explicitly
        /// by arrow / PageUp/Down / wheel navigation, and clamped
        /// every frame via [`PasteConfirmation::ensure_cursor_visible`].
        scroll_line: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteButton {
    Paste,
    Edit,
    Cancel,
}

/// Width (in monospaced cells) of one rendered button bracket pair
/// including label. Re-read by the hit-test in `App` so the layout
/// stays in lock-step with the renderer.
pub(crate) const BUTTON_LABEL_CELLS: usize = 10;
/// Gap (in cells) between adjacent buttons.
pub(crate) const BUTTON_GAP_CELLS: usize = 2;
/// Leading indent (in cells) before the first button on the row.
pub(crate) const BUTTON_LEADING_CELLS: usize = 2;

/// Render one button's fixed-width text bracket. Format:
/// `[ Paste  ]` (10 cells) — always the same width regardless of
/// selection so the row layout stays stable and the hit-test math
/// in `App::paste_button_hit_test` doesn't have to look at glyph
/// widths.
pub(crate) fn render_button_label(b: PasteButton, selected: PasteButton) -> String {
    let label = match b {
        PasteButton::Paste => "Paste",
        PasteButton::Edit => "Edit",
        PasteButton::Cancel => "Cancel",
    };
    // Inner width = BUTTON_LABEL_CELLS - 2 brackets = 8 cells.
    // Selected button gets `[> Paste <]` markers; others get a
    // plain `[ Paste  ]`. Both render to exactly 10 cells.
    if b == selected {
        // Two chars eaten by `> ` / ` <`, 8 - 4 = 4 chars of
        // label area. Truncate the longest label (`Cancel`, 6
        // chars) to 4? That'd hurt readability. So instead drop
        // the inner padding and use `[>Paste<]` / `[>Cancel<]`
        // — still 10 cells if we hard-pad.
        let padded = format!(">{label:<width$}<", width = BUTTON_LABEL_CELLS - 4);
        // Final form: `[>Cancel<]` — 10 cells exactly because
        // BUTTON_LABEL_CELLS - 4 = 6 chars between markers.
        format!("[{padded}]")
    } else {
        let padded = format!(" {label:<width$} ", width = BUTTON_LABEL_CELLS - 4);
        format!("[{padded}]")
    }
}

impl PasteButton {
    #[allow(dead_code)] // exposed for future test scaffolding
    pub(crate) fn label(self) -> &'static str {
        match self {
            PasteButton::Paste => "Paste",
            PasteButton::Edit => "Edit",
            PasteButton::Cancel => "Cancel",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            PasteButton::Paste => PasteButton::Edit,
            PasteButton::Edit => PasteButton::Cancel,
            PasteButton::Cancel => PasteButton::Paste,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            PasteButton::Paste => PasteButton::Cancel,
            PasteButton::Edit => PasteButton::Paste,
            PasteButton::Cancel => PasteButton::Edit,
        }
    }
}

impl PasteConfirmation {
    pub(crate) fn new_confirm(text: String, pane_uid: u64) -> Self {
        Self {
            text,
            pane_uid,
            mode: PasteMode::Confirm {
                selected: PasteButton::Paste,
            },
        }
    }

    /// Switch to edit mode. Cursor lands at the end of the buffer
    /// so the user can append immediately without re-anchoring.
    pub(crate) fn enter_edit_mode(&mut self) {
        self.mode = PasteMode::Edit {
            cursor: self.text.len(),
            scroll_line: 0,
        };
    }

    /// Read the current scroll_line (top of viewport) in edit mode.
    /// `0` for non-edit modes.
    pub(crate) fn scroll_line(&self) -> usize {
        match self.mode {
            PasteMode::Edit { scroll_line, .. } => scroll_line,
            _ => 0,
        }
    }

    /// Clamp `scroll_line` so the cursor stays inside the visible
    /// `[scroll_line, scroll_line + visible_rows)` window. Called
    /// every frame by the renderer (in `&mut self` context, before
    /// cloning) AND on every cursor-moving edit. Idempotent — if
    /// the cursor is already in view, `scroll_line` is unchanged.
    pub(crate) fn ensure_cursor_visible(&mut self, visible_rows: usize) {
        let (cursor, scroll_line) = match &mut self.mode {
            PasteMode::Edit { cursor, scroll_line } => (cursor, scroll_line),
            _ => return,
        };
        let visible = visible_rows.max(1);
        let total_lines = if self.text.is_empty() {
            1
        } else {
            self.text.matches('\n').count() + 1
        };
        let cursor_line = self.text[..(*cursor).min(self.text.len())]
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        // If cursor is above the viewport top → snap viewport to it.
        if cursor_line < *scroll_line {
            *scroll_line = cursor_line;
        }
        // If cursor is at or below the viewport bottom → snap up.
        let bottom = scroll_line.saturating_add(visible);
        if cursor_line >= bottom {
            *scroll_line = cursor_line + 1 - visible;
        }
        // Clamp against the buffer's end so we don't scroll past
        // the last line.
        let max_scroll = total_lines.saturating_sub(visible);
        if *scroll_line > max_scroll {
            *scroll_line = max_scroll;
        }
    }

    /// Move the viewport (independently of cursor) by `delta` lines.
    /// Positive = scroll DOWN (later lines). Negative = scroll UP.
    /// Used by the mouse wheel handler.
    pub(crate) fn scroll_by(&mut self, delta: i64, visible_rows: usize) {
        let scroll_line = match &mut self.mode {
            PasteMode::Edit { scroll_line, .. } => scroll_line,
            _ => return,
        };
        let total_lines = if self.text.is_empty() {
            1
        } else {
            self.text.matches('\n').count() + 1
        };
        let max_scroll = total_lines.saturating_sub(visible_rows.max(1));
        let cur = *scroll_line as i64;
        let next = (cur + delta).clamp(0, max_scroll as i64);
        *scroll_line = next as usize;
    }

    /// Lines for the modal preview. Used by the renderer to draw
    /// the confirm dialog body. Returns up to `max_lines` lines;
    /// extra lines collapse into a "+N more lines" trailer (which
    /// the renderer surfaces).
    pub(crate) fn preview(&self, max_lines: usize) -> (Vec<&str>, usize) {
        let mut lines: Vec<&str> = self.text.lines().collect();
        let total = lines.len();
        if lines.len() > max_lines {
            lines.truncate(max_lines);
        }
        (lines, total)
    }

    /// Number of newlines in the buffer. Surfaced in the modal
    /// header ("Paste 12 lines?").
    pub(crate) fn line_count(&self) -> usize {
        // `lines()` drops a trailing empty line. Count by '\n'
        // occurrences + 1 if non-empty.
        if self.text.is_empty() {
            0
        } else {
            self.text.matches('\n').count() + 1
        }
    }

    /// Insert a single character at the cursor in edit mode. Bumps
    /// the cursor by the char's byte length (UTF-8 safe).
    pub(crate) fn edit_insert(&mut self, ch: char) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        let pos = (*cursor).min(self.text.len());
        // Walk back to a char boundary if needed (defensive — the
        // mutation paths below all maintain the invariant, but
        // belt + suspenders against a future bug).
        let pos = floor_char_boundary(&self.text, pos);
        self.text.insert(pos, ch);
        *cursor = pos + ch.len_utf8();
    }

    /// Insert a string (e.g. bracketed paste of another payload —
    /// rare but possible). Same UTF-8-safe rules as `edit_insert`.
    pub(crate) fn edit_insert_str(&mut self, s: &str) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        let pos = floor_char_boundary(&self.text, (*cursor).min(self.text.len()));
        self.text.insert_str(pos, s);
        *cursor = pos + s.len();
    }

    /// Backspace: delete the char before the cursor.
    pub(crate) fn edit_backspace(&mut self) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        if *cursor == 0 {
            return;
        }
        let start = floor_char_boundary(&self.text, *cursor - 1);
        self.text.replace_range(start..*cursor, "");
        *cursor = start;
    }

    /// Forward Delete: drop the char AT the cursor.
    pub(crate) fn edit_delete(&mut self) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        if *cursor >= self.text.len() {
            return;
        }
        // The next char boundary AFTER cursor.
        let end = ceil_char_boundary(&self.text, *cursor + 1);
        self.text.replace_range(*cursor..end, "");
    }

    /// Move the cursor left one UTF-8 char.
    pub(crate) fn edit_left(&mut self) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        if *cursor == 0 {
            return;
        }
        *cursor = floor_char_boundary(&self.text, *cursor - 1);
    }

    /// Move the cursor right one UTF-8 char.
    pub(crate) fn edit_right(&mut self) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        if *cursor >= self.text.len() {
            return;
        }
        *cursor = ceil_char_boundary(&self.text, *cursor + 1);
    }

    /// Move cursor to the start of the current line.
    pub(crate) fn edit_home(&mut self) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        let bytes = self.text.as_bytes();
        let mut i = *cursor;
        while i > 0 && bytes[i - 1] != b'\n' {
            i -= 1;
        }
        *cursor = i;
    }

    /// Move cursor to the end of the current line.
    pub(crate) fn edit_end(&mut self) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        let bytes = self.text.as_bytes();
        let mut i = *cursor;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        *cursor = i;
    }

    /// Move cursor up one line, preserving column. Stays at line 0
    /// when already on the first line. Approximate column: byte
    /// offset from the line start; works correctly for ASCII and
    /// for cases where the target line also has the matching
    /// multi-byte char at the same position.
    pub(crate) fn edit_up(&mut self) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        let bytes = self.text.as_bytes();
        // Find current line start + column.
        let mut line_start = *cursor;
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        if line_start == 0 {
            // Already on first line — jump to col 0.
            *cursor = 0;
            return;
        }
        let col = *cursor - line_start;
        // Find previous line's start.
        let prev_line_end = line_start - 1; // position of the '\n'
        let mut prev_line_start = prev_line_end;
        while prev_line_start > 0 && bytes[prev_line_start - 1] != b'\n' {
            prev_line_start -= 1;
        }
        let prev_line_len = prev_line_end - prev_line_start;
        let new_col = col.min(prev_line_len);
        let raw = prev_line_start + new_col;
        *cursor = floor_char_boundary(&self.text, raw);
    }

    /// Move cursor up `n` lines. Used by PageUp (n = viewport-1).
    pub(crate) fn edit_up_n(&mut self, n: usize) {
        for _ in 0..n {
            let before = match self.mode {
                PasteMode::Edit { cursor, .. } => cursor,
                _ => return,
            };
            self.edit_up();
            // Stop when we hit the top — `edit_up` becomes a no-op
            // on line 0, but loops would still spin. Early-exit on
            // a fixed-point.
            match self.mode {
                PasteMode::Edit { cursor, .. } if cursor == before && before == 0 => return,
                _ => {}
            }
        }
    }

    /// Move cursor down `n` lines. Used by PageDown.
    pub(crate) fn edit_down_n(&mut self, n: usize) {
        for _ in 0..n {
            let before = match self.mode {
                PasteMode::Edit { cursor, .. } => cursor,
                _ => return,
            };
            self.edit_down();
            match self.mode {
                PasteMode::Edit { cursor, .. } if cursor == before => return,
                _ => {}
            }
        }
    }

    /// Move cursor down one line. Mirror of `edit_up`.
    pub(crate) fn edit_down(&mut self) {
        let cursor = match &mut self.mode {
            PasteMode::Edit { cursor, .. } => cursor,
            _ => return,
        };
        let bytes = self.text.as_bytes();
        let mut line_start = *cursor;
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        let mut line_end = *cursor;
        while line_end < bytes.len() && bytes[line_end] != b'\n' {
            line_end += 1;
        }
        if line_end >= bytes.len() {
            *cursor = bytes.len();
            return;
        }
        let col = *cursor - line_start;
        let next_line_start = line_end + 1;
        let mut next_line_end = next_line_start;
        while next_line_end < bytes.len() && bytes[next_line_end] != b'\n' {
            next_line_end += 1;
        }
        let next_line_len = next_line_end - next_line_start;
        let new_col = col.min(next_line_len);
        let raw = next_line_start + new_col;
        *cursor = floor_char_boundary(&self.text, raw);
    }
}

/// Round `pos` DOWN to the nearest UTF-8 char boundary in `s`. Used
/// by every cursor-moving / mutating method so a buggy caller can't
/// land us mid-codepoint. `pos == s.len()` is a valid boundary.
fn floor_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Round `pos` UP to the nearest UTF-8 char boundary.
fn ceil_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// True when a paste should trigger the confirmation modal.
/// Multi-line + above the `min_bytes` floor. Pure helper so the
/// renderer's `write_paste` can branch off one boolean.
pub(crate) fn should_confirm(text: &str, cfg: &crate::PasteConfirmConfig) -> bool {
    if !cfg.confirm_multiline {
        return false;
    }
    if !text.contains('\n') && !text.contains('\r') {
        return false;
    }
    text.len() >= cfg.min_bytes as usize
}

// Touch a never-used Ordering import in tests below so the linter
// is happy on builds where the tests are excluded.
#[allow(dead_code)]
fn _dont_warn_about_ordering() {
    let _ = Ordering::Relaxed;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc(text: &str) -> PasteConfirmation {
        PasteConfirmation::new_confirm(text.to_string(), 1)
    }

    #[test]
    fn line_count_handles_empty_and_trailing_newline() {
        assert_eq!(pc("").line_count(), 0);
        assert_eq!(pc("one").line_count(), 1);
        assert_eq!(pc("one\ntwo").line_count(), 2);
        assert_eq!(pc("one\ntwo\n").line_count(), 3, "trailing \\n counts");
    }

    #[test]
    fn preview_truncates_at_max_lines() {
        let p = pc("a\nb\nc\nd\ne\nf");
        let (lines, total) = p.preview(3);
        assert_eq!(lines, vec!["a", "b", "c"]);
        assert_eq!(total, 6);
    }

    #[test]
    fn should_confirm_only_triggers_on_multiline_above_floor() {
        let cfg = crate::PasteConfirmConfig {
            confirm_multiline: true,
            min_bytes: 10,
        };
        // Single-line: never confirms regardless of length.
        assert!(!should_confirm("ls -la --color", &cfg));
        // Multi-line but below floor.
        assert!(!should_confirm("a\nb", &cfg));
        // Multi-line and above floor.
        assert!(should_confirm("echo aaa\necho bbb\necho ccc", &cfg));
        // Master toggle off — never confirms.
        let off = crate::PasteConfirmConfig {
            confirm_multiline: false,
            min_bytes: 10,
        };
        assert!(!should_confirm("echo aaa\necho bbb\necho ccc", &off));
    }

    #[test]
    fn button_next_prev_round_trip() {
        // Tab cycles forward; Shift+Tab cycles back. Every step is
        // the inverse of the previous.
        assert_eq!(PasteButton::Paste.next(), PasteButton::Edit);
        assert_eq!(PasteButton::Edit.next(), PasteButton::Cancel);
        assert_eq!(PasteButton::Cancel.next(), PasteButton::Paste);
        for b in [PasteButton::Paste, PasteButton::Edit, PasteButton::Cancel] {
            assert_eq!(b.next().prev(), b);
            assert_eq!(b.prev().next(), b);
        }
    }

    #[test]
    fn edit_mode_starts_with_cursor_at_end() {
        let mut p = pc("hello\nworld");
        p.enter_edit_mode();
        match p.mode {
            PasteMode::Edit { cursor, .. } => assert_eq!(cursor, p.text.len()),
            _ => panic!("expected Edit mode"),
        }
    }

    #[test]
    fn edit_insert_at_cursor_then_advance() {
        let mut p = pc("ab");
        p.enter_edit_mode();
        // Move to start.
        p.edit_left();
        p.edit_left();
        p.edit_insert('X');
        assert_eq!(p.text, "Xab");
        match p.mode {
            PasteMode::Edit { cursor, .. } => assert_eq!(cursor, 1),
            _ => panic!(),
        }
    }

    #[test]
    fn edit_backspace_removes_preceding_char() {
        let mut p = pc("abc");
        p.enter_edit_mode(); // cursor at 3
        p.edit_backspace();
        assert_eq!(p.text, "ab");
        match p.mode {
            PasteMode::Edit { cursor, .. } => assert_eq!(cursor, 2),
            _ => panic!(),
        }
    }

    #[test]
    fn edit_backspace_handles_multibyte_utf8() {
        // "лa" = 0xd0 0xbb 0x61. Backspace from end → drops `a`.
        // Next backspace → drops `л` (2 bytes).
        let mut p = pc("\u{43b}a");
        p.enter_edit_mode();
        p.edit_backspace();
        assert_eq!(p.text, "\u{43b}");
        p.edit_backspace();
        assert_eq!(p.text, "");
    }

    #[test]
    fn edit_arrows_navigate_lines_preserving_column() {
        let mut p = pc("first line\nsecond line\nthird line");
        p.enter_edit_mode(); // cursor at end
        // Walk up — should land on column 10 of line 2, etc.
        p.edit_up();
        match p.mode {
            PasteMode::Edit { cursor, .. } => {
                // line 2 = "second line", len 11. col 10 of line 3
                // exceeds line 2 length? No — both lines are 11
                // chars, col 10 fits.
                let line_start = p.text[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
                assert_eq!(cursor - line_start, 10);
            }
            _ => panic!(),
        }
        p.edit_up();
        match p.mode {
            PasteMode::Edit { cursor, .. } => {
                // Line 1 starts at 0 — col 10.
                assert_eq!(cursor, 10);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn edit_home_end_per_line() {
        let mut p = pc("alpha\nbeta\ngamma");
        p.enter_edit_mode(); // cursor at end (15)
        p.edit_home();
        match p.mode {
            PasteMode::Edit { cursor, .. } => assert_eq!(cursor, 11, "start of 'gamma'"),
            _ => panic!(),
        }
        p.edit_end();
        match p.mode {
            PasteMode::Edit { cursor, .. } => assert_eq!(cursor, 16, "end of 'gamma'"),
            _ => panic!(),
        }
    }

    #[test]
    fn edit_insert_str_round_trips_through_utf8_safe_cursor() {
        let mut p = pc("ab");
        p.enter_edit_mode(); // cursor 2
        p.edit_left(); // cursor 1
        p.edit_insert_str("XYZ");
        assert_eq!(p.text, "aXYZb");
        match p.mode {
            PasteMode::Edit { cursor, .. } => assert_eq!(cursor, 4),
            _ => panic!(),
        }
    }
}
