//! Layout helpers — pure rect computations for the App's chrome.
//!
//! Every method here is `&self` (no mutation, no side-effects beyond
//! returning a `PaneRect`). They consume window dimensions + font
//! metrics + overlay state and produce the geometry the render path
//! consumes.
//!
//! `impl App` blocks are split across multiple files via Rust's
//! submodule rules — private fields of `App` (declared in `lib.rs`)
//! are visible here because this module is a descendant of the
//! crate root.

use std::sync::atomic::Ordering;

use crate::{App, ContextMenu, MenuItem, PaneRect, PAD, SPLIT_GAP};

impl App {
    /// Lay the BSP tree of the active tab into the available pane area
    /// (everything below the header, above any bottom-bar reserve).
    ///
    /// Zoomed tabs collapse non-focused panes to a degenerate 0×0 rect
    /// so caller indices by pane index still line up — only the
    /// focused pane gets the full inner rect.
    pub(crate) fn layout_active_tab(&self) -> Vec<PaneRect> {
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

    /// Single-row tab strip + window controls at the top of the
    /// window. Spans the full width minus PAD on each side.
    pub(crate) fn header_rect(&self) -> Option<PaneRect> {
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
    pub(crate) fn status_bar_rect(&self) -> Option<PaneRect> {
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
    pub(crate) fn bottom_bar_reserves_space(&self) -> bool {
        self.search.is_some()
    }

    /// True when the bottom bar has any visible content — search
    /// prompt OR scrollback position indicator. Used by the render
    /// path to decide whether to paint the bar; layout uses the
    /// stricter `bottom_bar_reserves_space`.
    pub(crate) fn bottom_bar_has_content(&self) -> bool {
        if self.search.is_some() {
            return true;
        }
        let Some(tab) = self.active_tab() else { return false };
        let Some(p) = tab.focused_pane() else { return false };
        p.scroll_offset.load(Ordering::Relaxed) > 0
    }

    /// Help / settings overlay rect. Centered card, 520×(h-40) max,
    /// returns None on windows too small to fit.
    pub(crate) fn help_rect(&self) -> Option<PaneRect> {
        let state = self.state.as_ref()?;
        let w = state.config.width as f32;
        let h = state.config.height as f32;
        let max_w = 520.0_f32.min(w - 40.0).max(100.0);
        let max_h = (h - 40.0).max(100.0);
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

    /// Rename overlay rect. Smaller than `help_rect` (one input line).
    pub(crate) fn rename_rect(&self) -> Option<PaneRect> {
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

    /// Context-menu rect. Width = longest label + padding × cell_w.
    /// Height = item count × line_h. Anchor (top-left) is clamped to
    /// keep the menu on-screen.
    pub(crate) fn context_menu_rect(&self, menu: &ContextMenu) -> Option<PaneRect> {
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

    /// Compute the outer rect inside which panes are laid out — below
    /// the header, with PAD on every edge, shrunk by the bottom-bar
    /// height when the bar reserves space.
    pub(crate) fn outer_rect(&self) -> Option<PaneRect> {
        let state = self.state.as_ref()?;
        let w = state.config.width as f32;
        let h = state.config.height as f32;
        let header_h = state.text.header_height();
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
}
