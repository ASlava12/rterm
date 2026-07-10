//! Suggestion popup state machine.
//!
//! Held on `App` as `Option<SuggestionPopup>`. The renderer's
//! per-frame redraw path calls `refresh` to consider arming /
//! refreshing / dismissing the popup; the keyboard pipeline calls
//! `handle_key` first when the popup is visible, intercepting ↓ /
//! ↑ / TAB / Esc; the click path calls `hit_test` to dispatch a
//! mouse click on a row.
//!
//! The popup ONLY tracks state and ranking. Rendering lives in
//! `lib.rs`'s redraw branch (reuses the bg+text overlay layers from
//! the context_menu / palette path); injection lives at the call
//! site (`pane.send_input(b"\x15")` then `pane.send_input(text)`).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use rterm_history::{History, Suggestion};

use crate::HistoryPopupConfig;

/// Current popup state, owned by `App`.
///
/// `Clone` is intentional: the renderer's overlay-build path snapshots
/// the popup once at the top of a redraw frame so the `(spans, rect)`
/// pair can survive the long-lived `state.as_mut()` borrow inside the
/// render call. The clone is cheap — entries are `Suggestion` value
/// types (`String + u32 + i64`).
#[derive(Debug, Clone)]
pub(crate) struct SuggestionPopup {
    /// Which pane is the source of the prefix. Closing the popup on
    /// a pane switch / tab switch reads this.
    pub(crate) pane_uid: u64,
    /// Currently-displayed prefix. Compared to the live pane prefix
    /// on each `refresh` call to detect "user kept typing."
    pub(crate) prefix: String,
    /// Ranked suggestions for `prefix`.
    pub(crate) entries: Vec<Suggestion>,
    /// Highlighted row in `entries`. `None` means no selection —
    /// TAB at this state falls through to the shell. First ↓ moves
    /// to `Some(0)`.
    pub(crate) selected: Option<usize>,
    /// Top of the visible window in `entries`. Adjusted so
    /// `selected` stays in view.
    pub(crate) scroll: usize,
    /// `CommandCapture::generation` value at last refresh. Lets us
    /// short-circuit the debouncer if the user hasn't typed since.
    pub(crate) last_seen_generation: u64,
}

impl SuggestionPopup {
    /// Move the selection down. First press from `None` selects
    /// row 0. Subsequent presses advance and clamp at `len - 1`.
    /// Adjusts `scroll` so the new selection stays inside the
    /// visible window (`visible` rows tall).
    pub(crate) fn nav_down(&mut self, visible: usize) {
        if self.entries.is_empty() {
            return;
        }
        let next = match self.selected {
            None => 0,
            Some(i) => (i + 1).min(self.entries.len() - 1),
        };
        self.selected = Some(next);
        self.fixup_scroll(visible);
    }

    /// Move the selection up. `↑` from `Some(0)` returns to
    /// `None` (no selection, popup still visible — TAB once
    /// becomes a shell-passthrough again). From higher rows it
    /// just decrements and clamps `scroll`.
    pub(crate) fn nav_up(&mut self, visible: usize) {
        match self.selected {
            None => {
                // Wrap to last row — user pressed ↑ first which is
                // a common pattern (open popup, want the most-
                // recent / most-relevant match at the top still
                // BUT the bottom one if "wrap up" was meant).
                // Actually the spec said ↑ from None → ???. Going
                // with: ↑ from None opens the LAST visible row,
                // matching VSCode where ↑ in a fresh dropdown
                // selects the bottom entry. Users who want the
                // top use `↓`.
                self.selected = Some(self.entries.len().saturating_sub(1));
            }
            Some(0) => {
                self.selected = None;
                self.scroll = 0;
                return;
            }
            Some(i) => {
                self.selected = Some(i - 1);
            }
        }
        self.fixup_scroll(visible);
    }

    /// Take the selected entry as an injection payload and close
    /// the popup. Returns `None` if nothing was selected — the
    /// caller should treat that as TAB-passthrough.
    pub(crate) fn take_selected(&mut self) -> Option<String> {
        let idx = self.selected?;
        let entry = self.entries.get(idx)?;
        Some(entry.text.clone())
    }

    /// Whether the popup is in a state where TAB should inject.
    pub(crate) fn has_selection(&self) -> bool {
        self.selected.is_some()
    }

    /// Slide `scroll` so `selected` is inside `[scroll, scroll +
    /// visible)`. Called after every nav. Visible rows of 0 is
    /// degenerate but possible if config says `popup_rows = 0` —
    /// guard against div / mod chaos.
    fn fixup_scroll(&mut self, visible: usize) {
        let Some(i) = self.selected else {
            self.scroll = 0;
            return;
        };
        let visible = visible.max(1);
        if i < self.scroll {
            self.scroll = i;
        } else if i >= self.scroll + visible {
            self.scroll = i + 1 - visible;
        }
        // Clamp at the back so we never scroll past the end.
        let max_scroll = self.entries.len().saturating_sub(visible);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }
}

/// Per-frame state machine for the popup. Returns:
/// * `Some(popup)` to install or replace `App.suggestion_popup`,
/// * `None` (when the caller's existing state was `Some`) to close,
/// * unchanged (`None` returned + existing was `None`) when nothing
///   should happen yet.
///
/// Single function so the App side doesn't have to reason about
/// the state-transition matrix; the App just calls `compute` and
/// assigns the result.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute(
    cfg: &HistoryPopupConfig,
    history: &Arc<Mutex<History>>,
    existing: Option<&SuggestionPopup>,
    focused_pane_uid: u64,
    capture_generation: u64,
    current_input: &str,
    // History bucket to query — the focused pane's profile context.
    context: &str,
    last_input_at: Option<Instant>,
    now: Instant,
) -> StateTransition {
    if !cfg.enabled {
        return StateTransition::Close;
    }
    // Pane switched while popup was open → close (let the new pane
    // arm its own popup when it's ready).
    if let Some(p) = existing {
        if p.pane_uid != focused_pane_uid {
            return StateTransition::Close;
        }
    }
    // Empty prefix or below threshold: close if open, otherwise
    // stay closed.
    let trimmed = current_input.trim_start();
    if trimmed.len() < cfg.min_prefix_len as usize {
        return if existing.is_some() {
            StateTransition::Close
        } else {
            StateTransition::Keep
        };
    }
    // Debounce: wait until the user has paused long enough.
    let debounce = std::time::Duration::from_millis(cfg.popup_debounce_ms as u64);
    if let Some(last) = last_input_at {
        if now.duration_since(last) < debounce {
            return StateTransition::Keep;
        }
    }
    // Same prefix as the existing popup AND no new input since
    // last refresh → keep state untouched.
    if let Some(p) = existing {
        if p.prefix == current_input && p.last_seen_generation == capture_generation {
            return StateTransition::Keep;
        }
    }
    // Query the store. Use popup_rows + a buffer (×3) so the
    // popup has scroll headroom without re-querying per nav.
    let query_limit = (cfg.popup_rows as usize * 3).max(15);
    let entries = match history.lock() {
        Ok(h) => h.suggest(current_input, query_limit, context).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if entries.is_empty() {
        return if existing.is_some() {
            StateTransition::Close
        } else {
            StateTransition::Keep
        };
    }
    StateTransition::Open(SuggestionPopup {
        pane_uid: focused_pane_uid,
        prefix: current_input.to_string(),
        entries,
        selected: None,
        scroll: 0,
        last_seen_generation: capture_generation,
    })
}

#[derive(Debug)]
pub(crate) enum StateTransition {
    /// New / replaced popup state.
    Open(SuggestionPopup),
    /// Close the popup if open.
    Close,
    /// Leave the caller's state as-is.
    Keep,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str, count: u32) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            count,
            last_used: 0,
        }
    }

    fn make_popup() -> SuggestionPopup {
        SuggestionPopup {
            pane_uid: 1,
            prefix: "g".to_string(),
            entries: (0..10).map(|i| entry(&format!("g{i}"), 10 - i)).collect(),
            selected: None,
            scroll: 0,
            last_seen_generation: 0,
        }
    }

    #[test]
    fn nav_down_selects_first_row_from_none() {
        let mut p = make_popup();
        assert_eq!(p.selected, None);
        p.nav_down(5);
        assert_eq!(p.selected, Some(0));
        assert_eq!(p.scroll, 0);
    }

    #[test]
    fn nav_down_clamps_at_last_row() {
        let mut p = make_popup();
        // Walk to the bottom: 10 rows, expect selected = 9.
        for _ in 0..30 {
            p.nav_down(5);
        }
        assert_eq!(p.selected, Some(9));
    }

    #[test]
    fn nav_down_scrolls_visible_window() {
        let mut p = make_popup();
        let visible = 5;
        for _ in 0..visible {
            p.nav_down(visible);
        }
        // After 5 ↓: selected = 4 (still in first window 0..5),
        // scroll = 0.
        assert_eq!(p.selected, Some(4));
        assert_eq!(p.scroll, 0);
        // Sixth ↓ pushes selected to 5, scroll to 1.
        p.nav_down(visible);
        assert_eq!(p.selected, Some(5));
        assert_eq!(p.scroll, 1);
    }

    #[test]
    fn nav_up_from_none_jumps_to_last_row() {
        let mut p = make_popup();
        p.nav_up(5);
        assert_eq!(p.selected, Some(9));
        // Scroll should put 9 in view.
        assert!(p.scroll <= 9 && p.scroll + 5 > 9);
    }

    #[test]
    fn nav_up_from_first_returns_to_none() {
        let mut p = make_popup();
        p.nav_down(5); // selected = 0
        p.nav_up(5);
        assert_eq!(p.selected, None);
        assert_eq!(p.scroll, 0);
    }

    #[test]
    fn take_selected_returns_entry_text() {
        let mut p = make_popup();
        p.nav_down(5);
        p.nav_down(5); // selected = 1 → entries[1].text = "g1"
        let text = p.take_selected();
        assert_eq!(text.as_deref(), Some("g1"));
    }

    #[test]
    fn take_selected_returns_none_without_selection() {
        let mut p = make_popup();
        assert_eq!(p.take_selected(), None);
    }

    #[test]
    fn compute_keeps_state_below_debounce() {
        let cfg = HistoryPopupConfig::default();
        let h = Arc::new(Mutex::new(History::open(":memory:").unwrap()));
        let now = Instant::now();
        let recent = now - std::time::Duration::from_millis(50); // below 150ms
        match compute(&cfg, &h, None, 1, 0, "gi", "*", Some(recent), now) {
            StateTransition::Keep => {}
            other => panic!("expected Keep, got {other:?}"),
        }
    }

    #[test]
    fn compute_opens_popup_after_debounce() {
        let cfg = HistoryPopupConfig::default();
        let h = Arc::new(Mutex::new(History::open(":memory:").unwrap()));
        h.lock().unwrap().record("git status", "*").unwrap();
        h.lock().unwrap().record("git commit", "*").unwrap();
        let now = Instant::now();
        let old = now - std::time::Duration::from_millis(300);
        let res = compute(&cfg, &h, None, 1, 0, "git", "*", Some(old), now);
        match res {
            StateTransition::Open(p) => {
                assert_eq!(p.prefix, "git");
                assert!(p.entries.iter().any(|e| e.text == "git status"));
                assert!(p.entries.iter().any(|e| e.text == "git commit"));
                assert!(p.selected.is_none(), "fresh popup starts with no selection");
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn compute_closes_below_min_prefix() {
        let cfg = HistoryPopupConfig {
            min_prefix_len: 3,
            ..HistoryPopupConfig::default()
        };
        let h = Arc::new(Mutex::new(History::open(":memory:").unwrap()));
        let now = Instant::now();
        let old = now - std::time::Duration::from_millis(200);
        // Existing popup, but the user backspaced down to "gi".
        let existing = SuggestionPopup {
            pane_uid: 1,
            prefix: "git".to_string(),
            entries: vec![],
            selected: None,
            scroll: 0,
            last_seen_generation: 0,
        };
        match compute(&cfg, &h, Some(&existing), 1, 1, "gi", "*", Some(old), now) {
            StateTransition::Close => {}
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn compute_closes_on_pane_switch() {
        let cfg = HistoryPopupConfig::default();
        let h = Arc::new(Mutex::new(History::open(":memory:").unwrap()));
        let now = Instant::now();
        let existing = SuggestionPopup {
            pane_uid: 1,
            prefix: "git".to_string(),
            entries: vec![entry("git status", 1)],
            selected: None,
            scroll: 0,
            last_seen_generation: 0,
        };
        match compute(&cfg, &h, Some(&existing), 2, 0, "git", "*", Some(now), now) {
            StateTransition::Close => {}
            other => panic!("expected Close on pane switch, got {other:?}"),
        }
    }

    #[test]
    fn compute_closes_when_history_returns_empty() {
        let cfg = HistoryPopupConfig::default();
        let h = Arc::new(Mutex::new(History::open(":memory:").unwrap()));
        let now = Instant::now();
        let old = now - std::time::Duration::from_millis(200);
        let existing = SuggestionPopup {
            pane_uid: 1,
            prefix: "gi".to_string(),
            entries: vec![entry("git status", 1)],
            selected: None,
            scroll: 0,
            last_seen_generation: 0,
        };
        // Prefix changed; no matches in empty DB → close.
        match compute(&cfg, &h, Some(&existing), 1, 1, "qweasdzxc", "*", Some(old), now) {
            StateTransition::Close => {}
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn compute_keep_when_prefix_unchanged() {
        let cfg = HistoryPopupConfig::default();
        let h = Arc::new(Mutex::new(History::open(":memory:").unwrap()));
        let now = Instant::now();
        let old = now - std::time::Duration::from_millis(200);
        let existing = SuggestionPopup {
            pane_uid: 1,
            prefix: "git".to_string(),
            entries: vec![entry("git status", 1)],
            selected: None,
            scroll: 0,
            last_seen_generation: 0,
        };
        // Same prefix and capture_generation → Keep.
        match compute(&cfg, &h, Some(&existing), 1, 0, "git", "*", Some(old), now) {
            StateTransition::Keep => {}
            other => panic!("expected Keep, got {other:?}"),
        }
    }

    #[test]
    fn compute_disabled_always_closes() {
        let cfg = HistoryPopupConfig {
            enabled: false,
            ..HistoryPopupConfig::default()
        };
        let h = Arc::new(Mutex::new(History::open(":memory:").unwrap()));
        let now = Instant::now();
        match compute(&cfg, &h, None, 1, 0, "git", "*", Some(now), now) {
            StateTransition::Close => {}
            other => panic!("expected Close, got {other:?}"),
        }
    }
}
