//! Overlay text-span builders. Each method returns a list of
//! styled `(text, fg-rgb, bold)` triples, plus (for some) an
//! anchoring `PaneRect`. The render path stages these into
//! glyphon buffers per frame.
//!
//! Pure-ish — most methods are `&self` and only touch overlay
//! state (`self.search`, `self.palette`, `self.show_help`,
//! `self.context_menu`). `settings_spans` is `&mut self` because
//! it rebuilds `self.settings_hits` for click hit-testing in the
//! same pass.

use std::sync::atomic::Ordering;

use crate::{
    abbreviate_home, palette, AnchoredSpans, App, AppAction, ContextMenu, MenuItem, PaneRect,
    SettingsHit, WINDOW_CONTROLS_WIDTH_CELLS, WINDOW_CONTROL_CLOSE, WINDOW_CONTROL_GAP_CELLS,
    WINDOW_CONTROL_MAX, WINDOW_CONTROL_MIN,
};

impl App {
    pub(crate) fn palette_overlay_spans<'a>(
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

    /// text — fixes the "controls drift when cwd/url changes" bug.
    pub(crate) fn header_right_spans<'a>(
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

    /// tab strip stays visible during search.
    pub(crate) fn search_bar_spans<'a>(
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

    /// pane has `scroll_offset > 0` and search isn't holding the bar.
    pub(crate) fn scrollback_bar_spans<'a>(
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

    /// strip but condensed to one short line.
    pub(crate) fn status_bar_spans<'a>(
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

    /// Build the per-row text spans for the paste-confirmation
    /// modal. Renders differently depending on `modal.mode`:
    /// * `Confirm` — header (line count) + preview of the first
    ///   ~12 lines + button row.
    /// * `Edit` — header explaining Ctrl+Enter / Esc + the full
    ///   editable buffer + a `▏` cursor mark at the cursor offset.
    pub(crate) fn paste_confirmation_spans<'a>(
        &self,
        modal: &crate::paste_confirm::PasteConfirmation,
        storage: &'a mut Vec<String>,
    ) -> Vec<(&'a str, [u8; 3], bool)> {
        use crate::paste_confirm::{PasteButton, PasteMode};
        storage.clear();
        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        let fg = palette::default_fg();
        let muted = fg.map(|c| c.saturating_sub(80));
        let accent: [u8; 3] = [121, 192, 255];
        let warn: [u8; 3] = [220, 200, 120];
        match &modal.mode {
            PasteMode::Confirm { selected } => {
                let lines = modal.line_count();
                storage.push(format!("Paste {lines} lines into terminal?\n"));
                spans.push((storage.len() - 1, warn, true));
                storage.push("\n".to_string());
                spans.push((storage.len() - 1, fg, false));
                let (preview, total) = modal.preview(12);
                for line in preview {
                    // Truncate per-line so a 500-char log entry
                    // doesn't push the modal off-screen.
                    let display = if line.chars().count() > 80 {
                        let head: String = line.chars().take(78).collect();
                        format!("  │ {head}…\n")
                    } else {
                        format!("  │ {line}\n")
                    };
                    storage.push(display);
                    spans.push((storage.len() - 1, fg, false));
                }
                if total > 12 {
                    storage.push(format!("  │ … {} more lines\n", total - 12));
                    spans.push((storage.len() - 1, muted, false));
                }
                storage.push("\n".to_string());
                spans.push((storage.len() - 1, fg, false));
                // Button row — fixed-width labels so the mouse
                // hit-test math (in `paste_button_hit_test`) stays
                // a pure column-arithmetic computation rather than
                // a font-metric query. Selected button gets accent
                // colour; the brackets stay the same width either
                // way so the rest of the row doesn't shift.
                //
                // `crate::paste_confirm::BUTTON_LABEL_CELLS` /
                // `BUTTON_GAP_CELLS` document the constants the
                // hit-test re-reads.
                let row = format!(
                    "  {}  {}  {}\n",
                    crate::paste_confirm::render_button_label(PasteButton::Paste, *selected),
                    crate::paste_confirm::render_button_label(PasteButton::Edit, *selected),
                    crate::paste_confirm::render_button_label(PasteButton::Cancel, *selected),
                );
                storage.push(row);
                spans.push((storage.len() - 1, accent, true));
                storage.push("\n".to_string());
                spans.push((storage.len() - 1, fg, false));
                storage
                    .push("  Tab / ← → cycle  ·  Enter activate  ·  Esc cancel\n".to_string());
                spans.push((storage.len() - 1, muted, false));
            }
            PasteMode::Edit { cursor, .. } => {
                storage.push("Edit paste — Ctrl+Enter apply, Esc cancel\n".to_string());
                spans.push((storage.len() - 1, warn, true));
                storage.push("\n".to_string());
                spans.push((storage.len() - 1, fg, false));
                // Scrollable viewport. The previous implementation
                // dumped the whole buffer into one TextArea — for a
                // multi-page paste the cursor + trailing lines fell
                // outside the overlay rect with no way to recover.
                //
                // New layout: split the buffer into lines, compute
                // how many fit in the rect (modal_rows - 2 header
                // rows), and slice a viewport centred on the cursor
                // line. The cursor mark `▏` lives inside the
                // visible slice at the correct byte offset.
                let mut cursor_byte = (*cursor).min(modal.text.len());
                while cursor_byte > 0 && !modal.text.is_char_boundary(cursor_byte) {
                    cursor_byte -= 1;
                }
                // Soft-wrap the buffer into display rows so a line
                // wider than the modal stays visible instead of
                // clipping at the right edge. `↪` in the left margin
                // marks a wrap continuation; `↵` at a row's end marks
                // a real newline — so the two read differently.
                let wrap_cols = self.paste_modal_wrap_cols();
                let rows = crate::paste_confirm::display_rows(&modal.text, wrap_cols);
                let cursor_row = crate::paste_confirm::cursor_row_idx(&rows, cursor_byte);
                // Viewport height in rows, computed from the
                // overlay rect. Falls back to a sensible default
                // when cell metrics aren't ready yet (first frame
                // after a wgpu resize).
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
                // Header eats 2 rows ("Edit paste …", blank); leave
                // 1 row of bottom padding so the last line doesn't
                // get clipped by the rounded panel corner.
                const HEADER_ROWS: usize = 2;
                const BOTTOM_PAD_ROWS: usize = 1;
                let total_rows = (rect_h / line_h) as usize;
                let visible_rows = total_rows
                    .saturating_sub(HEADER_ROWS + BOTTOM_PAD_ROWS)
                    .max(1);
                // Use the persisted `scroll_line` (a DISPLAY-row index)
                // — the App's pre-render hook (`clamp_paste_modal_scroll`)
                // already adjusted it for this frame so the cursor is in
                // view but otherwise the viewport is stable across frames.
                // Click-to-position relies on this: the hit-test reads the
                // SAME scroll_line + wrap, so a click maps to that row.
                let scroll = modal.scroll_line().min(rows.len().saturating_sub(1));
                let end = (scroll + visible_rows).min(rows.len());
                for (offset, dr) in rows[scroll..end].iter().enumerate() {
                    let abs = scroll + offset;
                    let seg = &modal.text[dr.start..dr.end];
                    // Left margin: `↪ ` for soft-wrap continuations,
                    // two spaces for a fresh logical line. Both are two
                    // cells wide so the text column never shifts.
                    storage.push(if dr.is_continuation { "↪ ".to_string() } else { "  ".to_string() });
                    spans.push((storage.len() - 1, muted, false));
                    // Row text, with the caret `▏` spliced in when the
                    // cursor lives on this row.
                    if abs == cursor_row {
                        let split = cursor_byte.saturating_sub(dr.start).min(seg.len());
                        let (pre, post) = seg.split_at(split);
                        storage.push(format!("{pre}▏{post}"));
                    } else {
                        storage.push(seg.to_string());
                    }
                    spans.push((storage.len() - 1, fg, false));
                    // Trailing newline glyph (only on rows that end a
                    // hard line) + the actual line break.
                    storage.push(if dr.ends_newline { "↵\n".to_string() } else { "\n".to_string() });
                    spans.push((storage.len() - 1, muted, false));
                }
                // Scroll-position hint at the bottom — same idiom
                // as the suggestion-popup's `↓ N more`.
                if scroll > 0 || end < rows.len() {
                    let total = rows.len();
                    storage.push(format!(
                        "  ─ row {}–{} / {} ─\n",
                        scroll + 1,
                        end,
                        total,
                    ));
                    spans.push((storage.len() - 1, muted, false));
                }
            }
        }
        spans
            .into_iter()
            .map(|(idx, c, b)| (storage[idx].as_str(), c, b))
            .collect()
    }

    /// Build the per-row text spans for the suggestion popup. One
    /// line per visible entry; the selected row (when present) is
    /// rendered in an accent colour with a leading `▸`. A `↓ N
    /// more` trailer appears when there are entries past the
    /// visible window.
    pub(crate) fn suggestion_popup_spans<'a>(
        &self,
        popup: &crate::suggestion_popup::SuggestionPopup,
        storage: &'a mut Vec<String>,
    ) -> Vec<(&'a str, [u8; 3], bool)> {
        storage.clear();
        let mut spans: Vec<(usize, [u8; 3], bool)> = Vec::new();
        let fg = palette::default_fg();
        let muted = fg.map(|c| c.saturating_sub(80));
        let accent: [u8; 3] = [121, 192, 255];
        let visible_rows = (self.history_popup_cfg.popup_rows as usize).max(1);
        let end = (popup.scroll + visible_rows).min(popup.entries.len());
        for i in popup.scroll..end {
            let entry = &popup.entries[i];
            let is_selected = popup.selected == Some(i);
            let prefix = if is_selected { " ▸ " } else { "   " };
            // Trailing newline so cosmic-text breaks the line for
            // us; the overlay text buffer is single-block.
            let line = format!("{prefix}{}\n", entry.text);
            storage.push(line);
            let color = if is_selected { accent } else { fg };
            spans.push((storage.len() - 1, color, is_selected));
        }
        // Scroll indicator at the bottom — matches the
        // palette-overlay convention so users learn the cue once.
        if end < popup.entries.len() {
            storage.push(format!("   ↓ {} more\n", popup.entries.len() - end));
            spans.push((storage.len() - 1, muted, false));
        }
        spans
            .into_iter()
            .map(|(idx, c, b)| (storage[idx].as_str(), c, b))
            .collect()
    }

    /// Build the per-row text spans for the context menu (or app menu).
    pub(crate) fn context_menu_spans<'a>(
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

    /// clicks land on the right action without a second layout pass.
    pub(crate) fn settings_spans<'a>(
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

        let autoimg_row = row;
        let autoimg_mark = if self.image_auto_detect { "[x]" } else { "[ ]" };
        storage.push(format!("  {} Auto-detect inline images\n", autoimg_mark));
        spans.push((storage.len() - 1, fg, false));
        push_hit(
            &mut self.settings_hits,
            autoimg_row,
            2,
            32,
            SettingsHit::ToggleAutoDetectImages,
        );
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

    /// Build the help-overlay spans into `storage` and return colored slices.
    pub(crate) fn help_spans<'a>(&self, storage: &'a mut Vec<String>) -> Vec<(&'a str, [u8; 3], bool)> {
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
}
