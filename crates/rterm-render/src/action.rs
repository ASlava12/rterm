//! Canonical action vocabulary surfaced through `--list-actions`,
//! `[[keybindings]] action = "..."`, and `rterm.run_action(...)`.
//!
//! Each variant maps to:
//! - one snake_case canonical name (`canonical_for(...)`),
//! - zero or more aliases accepted by `from_name(...)`,
//! - a human-readable label shown in the help overlay /
//!   command palette / `--list-actions --labels` output.
//!
//! The actual side effects fire from a single match arm in
//! `App::run_action(...)` over in `lib.rs`. This module only owns the
//! name table; no rendering or window state lives here.

#[derive(Debug, Clone, Copy)]
pub enum AppAction {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    FirstTab,
    LastTab,
    MoveTabLeft,
    MoveTabRight,
    SplitHorizontal,
    SplitVertical,
    SplitAuto,
    ClosePane,
    FocusNextPane,
    FocusFirstPane,
    FocusLastPane,
    FocusPrevPane,
    PasteClipboard,
    CopySelection,
    ClearSelection,
    StartSearch,
    ToggleHelp,
    OpenPalette,
    JumpPrevPrompt,
    JumpNextPrompt,
    JumpPrevCommand,
    JumpNextCommand,
    ScrollPageUp,
    ScrollPageDown,
    ScrollHalfPageUp,
    ScrollHalfPageDown,
    ScrollLineUp,
    ScrollLineDown,
    ScrollHome,
    ScrollEnd,
    ClearScrollback,
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    SaveScrollback,
    ZoomPane,
    BalancePanes,
    Quit,
    FontIncrease,
    FontDecrease,
    FontReset,
    OpacityIncrease,
    OpacityDecrease,
    OpacityReset,
    ToggleLastTab,
    CopyHoveredUrl,
    ResetPane,
    SwapPaneNext,
    SwapPanePrev,
    ToggleBellMute,
    OpenHoveredUrl,
    /// Rotate to the next built-in theme (Default → Dracula → Solarized
    /// Dark → Solarized Light → Nord → Gruvbox Dark → Light → Default …).
    /// Reverse with `cycle_theme_prev`.
    CycleTheme,
    /// Same, in reverse order.
    CycleThemePrev,
    /// Toggle the settings overlay (terminator-style configuration
    /// window: theme picker, font size, opacity, all live-adjustable).
    OpenSettings,
    /// Open the rename overlay for the focused tab. Type a new title +
    /// Enter to apply, Esc to cancel.
    RenameTab,
    /// Snap the window to the left half of the current monitor.
    /// Cross-platform snap actions are best-effort:
    /// X11 / Win32 / macOS / FreeBSD-X11 reposition + resize via
    /// `Window::set_outer_position` + `set_inner_size`. Wayland
    /// disallows app-controlled positioning, so on Wayland these
    /// actions fall back to `set_maximized(true)` for "top" and are
    /// no-ops elsewhere — the compositor handles drag-to-edge snap
    /// itself in that case.
    SnapWindowLeft,
    /// Snap the window to the right half of the current monitor.
    SnapWindowRight,
    /// Snap the window to the top half (or maximize when the compositor
    /// owns positioning — fallback for Wayland).
    SnapWindowTop,
    /// Snap the window to the bottom half of the current monitor.
    SnapWindowBottom,
    /// Toggle the maximized state — same as the `▢` button. Exposed as
    /// a separate action so users can bind it without going through the
    /// menu.
    MaximizeToggle,
    /// Minimize the window — same as the `─` button.
    MinimizeWindow,
    /// Restore window to a sensible default size centered on the
    /// current monitor. Cancels any snap / maximize.
    RestoreWindow,
    /// Guake-style drop-down: toggle the window between a "dropped"
    /// state (sized + anchored per `[guake]` config, raised above
    /// other windows) and a minimised / hidden state. A no-op when
    /// `[guake] enabled = false`. Binding is in-app only — a true
    /// system-wide hotkey needs OS-level integration that this crate
    /// intentionally doesn't pull in.
    ToggleGuake,
}

impl AppAction {
    /// Canonical snake_case names that `from_name` accepts (one canonical
    /// alias per variant). Exposed to plugins through
    /// `rterm.builtin_actions()`.
    ///
    /// Derived from [`AppAction::ALL`] via [`AppAction::canonical_for`]
    /// so the two stay in lock-step automatically — adding a new
    /// variant only needs an `ALL` entry plus an arm in
    /// `canonical_for` / `from_name`, not a third copy of the name
    /// list. Order matches `ALL`'s display order; CLI consumers
    /// (`--list-actions`) sort the result themselves.
    pub fn canonical_names() -> Vec<String> {
        Self::ALL
            .iter()
            .map(|(action, _)| Self::canonical_for(*action).to_string())
            .collect()
    }

    /// Parse a snake_case action name from config — keep in sync with `ALL`.
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "new_tab" => Self::NewTab,
            "close_tab" => Self::CloseTab,
            "next_tab" => Self::NextTab,
            "prev_tab" | "previous_tab" => Self::PrevTab,
            // `goto_first_tab` / `goto_last_tab` are the canonical
            // names — `first_tab` is also accepted, but `last_tab`
            // is *already* the alias for `toggle_last_tab` (alt-tab
            // style "swap with previous tab"), so for the
            // jump-to-end variant we deliberately use the
            // unambiguous form.
            "goto_first_tab" | "first_tab" => Self::FirstTab,
            "goto_last_tab" | "rightmost_tab" => Self::LastTab,
            "move_tab_left" => Self::MoveTabLeft,
            "move_tab_right" => Self::MoveTabRight,
            "split_horizontal" | "split_h" => Self::SplitHorizontal,
            "split_vertical" | "split_v" => Self::SplitVertical,
            "split_auto" | "smart_split" | "split" => Self::SplitAuto,
            "close_pane" => Self::ClosePane,
            "focus_next_pane" => Self::FocusNextPane,
            "focus_first_pane" | "focus_top_pane" => Self::FocusFirstPane,
            "focus_last_pane" | "focus_bottom_pane" => Self::FocusLastPane,
            "focus_prev_pane" | "focus_previous_pane" => Self::FocusPrevPane,
            "paste" | "paste_clipboard" => Self::PasteClipboard,
            "copy" | "copy_selection" => Self::CopySelection,
            "clear_selection" | "deselect" => Self::ClearSelection,
            "search" | "start_search" => Self::StartSearch,
            "jump_prev_prompt" | "prev_prompt" => Self::JumpPrevPrompt,
            "jump_next_prompt" | "next_prompt" => Self::JumpNextPrompt,
            "jump_prev_command" | "prev_command" => Self::JumpPrevCommand,
            "jump_next_command" | "next_command" => Self::JumpNextCommand,
            "scroll_page_up" | "page_up" => Self::ScrollPageUp,
            "scroll_page_down" | "page_down" => Self::ScrollPageDown,
            "scroll_half_page_up" | "half_page_up" => Self::ScrollHalfPageUp,
            "scroll_half_page_down" | "half_page_down" => Self::ScrollHalfPageDown,
            "scroll_line_up" | "line_up" => Self::ScrollLineUp,
            "scroll_line_down" | "line_down" => Self::ScrollLineDown,
            "scroll_home" | "scroll_top" => Self::ScrollHome,
            "scroll_end" | "scroll_bottom" | "scroll_to_live" => Self::ScrollEnd,
            "clear_scrollback" | "clear_saved_lines" => Self::ClearScrollback,
            "resize_pane_left" | "shrink_pane_h" => Self::ResizePaneLeft,
            "resize_pane_right" | "grow_pane_h" => Self::ResizePaneRight,
            "resize_pane_up" | "shrink_pane_v" => Self::ResizePaneUp,
            "resize_pane_down" | "grow_pane_v" => Self::ResizePaneDown,
            "toggle_help" | "help" => Self::ToggleHelp,
            "open_palette" | "palette" | "command_palette" => Self::OpenPalette,
            "save_scrollback" | "save_history" => Self::SaveScrollback,
            "zoom_pane" | "toggle_zoom" | "fullscreen_pane" => Self::ZoomPane,
            "balance_panes" | "balance" | "even_layout" => Self::BalancePanes,
            "quit" | "exit" => Self::Quit,
            "font_increase" | "zoom_in" => Self::FontIncrease,
            "font_decrease" | "zoom_out" => Self::FontDecrease,
            "font_reset" | "zoom_reset" => Self::FontReset,
            "opacity_increase" | "more_opaque" | "raise_opacity" => Self::OpacityIncrease,
            "opacity_decrease" | "more_transparent" | "lower_opacity" => Self::OpacityDecrease,
            "opacity_reset" => Self::OpacityReset,
            "toggle_last_tab" | "last_tab" | "alternate_tab" => Self::ToggleLastTab,
            "copy_hovered_url" | "yank_url" => Self::CopyHoveredUrl,
            "reset_pane" | "soft_reset" => Self::ResetPane,
            "swap_pane_next" | "move_pane_next" => Self::SwapPaneNext,
            "swap_pane_prev" | "swap_pane_previous" | "move_pane_prev" => Self::SwapPanePrev,
            "toggle_bell_mute" | "mute_bell" | "unmute_bell" => Self::ToggleBellMute,
            "open_hovered_url" | "open_url" => Self::OpenHoveredUrl,
            "cycle_theme" | "next_theme" => Self::CycleTheme,
            "cycle_theme_prev" | "prev_theme" | "previous_theme" => Self::CycleThemePrev,
            "open_settings" | "settings" | "preferences" => Self::OpenSettings,
            "rename_tab" | "rename" => Self::RenameTab,
            "snap_window_left" | "snap_left" => Self::SnapWindowLeft,
            "snap_window_right" | "snap_right" => Self::SnapWindowRight,
            "snap_window_top" | "snap_top" | "snap_maximize" => Self::SnapWindowTop,
            "snap_window_bottom" | "snap_bottom" => Self::SnapWindowBottom,
            "maximize_toggle" | "toggle_maximize" => Self::MaximizeToggle,
            "minimize_window" | "minimize" => Self::MinimizeWindow,
            "restore_window" | "restore" => Self::RestoreWindow,
            "toggle_guake" | "guake" | "drop_down" | "toggle_dropdown" => Self::ToggleGuake,
            _ => return None,
        })
    }

    /// Master list. Order here is the default sort in the palette.
    /// `(canonical_name, label)` pairs for every built-in action, in the
    /// palette display order. Exposed so the `--list-actions --labels`
    /// CLI variant can render a human-readable column without duplicating
    /// the label table.
    pub fn name_label_pairs() -> Vec<(&'static str, &'static str)> {
        Self::ALL
            .iter()
            .map(|(action, label)| (Self::canonical_for(*action), *label))
            .collect()
    }

    /// First / preferred canonical name for a given variant. Matches the
    /// snake_case form in `canonical_names()`.
    fn canonical_for(action: AppAction) -> &'static str {
        match action {
            AppAction::NewTab => "new_tab",
            AppAction::CloseTab => "close_tab",
            AppAction::NextTab => "next_tab",
            AppAction::PrevTab => "prev_tab",
            AppAction::FirstTab => "goto_first_tab",
            AppAction::LastTab => "goto_last_tab",
            AppAction::MoveTabLeft => "move_tab_left",
            AppAction::MoveTabRight => "move_tab_right",
            AppAction::SplitHorizontal => "split_horizontal",
            AppAction::SplitVertical => "split_vertical",
            AppAction::SplitAuto => "split_auto",
            AppAction::ClosePane => "close_pane",
            AppAction::FocusNextPane => "focus_next_pane",
            AppAction::FocusFirstPane => "focus_first_pane",
            AppAction::FocusLastPane => "focus_last_pane",
            AppAction::FocusPrevPane => "focus_prev_pane",
            AppAction::PasteClipboard => "paste",
            AppAction::CopySelection => "copy",
            AppAction::ClearSelection => "clear_selection",
            AppAction::StartSearch => "search",
            AppAction::JumpPrevPrompt => "jump_prev_prompt",
            AppAction::JumpNextPrompt => "jump_next_prompt",
            AppAction::JumpPrevCommand => "jump_prev_command",
            AppAction::JumpNextCommand => "jump_next_command",
            AppAction::ScrollPageUp => "scroll_page_up",
            AppAction::ScrollPageDown => "scroll_page_down",
            AppAction::ScrollHalfPageUp => "scroll_half_page_up",
            AppAction::ScrollHalfPageDown => "scroll_half_page_down",
            AppAction::ScrollLineUp => "scroll_line_up",
            AppAction::ScrollLineDown => "scroll_line_down",
            AppAction::ScrollHome => "scroll_home",
            AppAction::ScrollEnd => "scroll_end",
            AppAction::ClearScrollback => "clear_scrollback",
            AppAction::ResizePaneLeft => "resize_pane_left",
            AppAction::ResizePaneRight => "resize_pane_right",
            AppAction::ResizePaneUp => "resize_pane_up",
            AppAction::ResizePaneDown => "resize_pane_down",
            AppAction::ToggleHelp => "toggle_help",
            AppAction::OpenPalette => "open_palette",
            AppAction::SaveScrollback => "save_scrollback",
            AppAction::ZoomPane => "zoom_pane",
            AppAction::BalancePanes => "balance_panes",
            AppAction::Quit => "quit",
            AppAction::FontIncrease => "font_increase",
            AppAction::FontDecrease => "font_decrease",
            AppAction::FontReset => "font_reset",
            AppAction::OpacityIncrease => "opacity_increase",
            AppAction::OpacityDecrease => "opacity_decrease",
            AppAction::OpacityReset => "opacity_reset",
            AppAction::ToggleLastTab => "toggle_last_tab",
            AppAction::CopyHoveredUrl => "copy_hovered_url",
            AppAction::ResetPane => "reset_pane",
            AppAction::SwapPaneNext => "swap_pane_next",
            AppAction::SwapPanePrev => "swap_pane_prev",
            AppAction::ToggleBellMute => "toggle_bell_mute",
            AppAction::OpenHoveredUrl => "open_hovered_url",
            AppAction::CycleTheme => "cycle_theme",
            AppAction::CycleThemePrev => "cycle_theme_prev",
            AppAction::OpenSettings => "open_settings",
            AppAction::RenameTab => "rename_tab",
            AppAction::SnapWindowLeft => "snap_window_left",
            AppAction::SnapWindowRight => "snap_window_right",
            AppAction::SnapWindowTop => "snap_window_top",
            AppAction::SnapWindowBottom => "snap_window_bottom",
            AppAction::MaximizeToggle => "maximize_toggle",
            AppAction::MinimizeWindow => "minimize_window",
            AppAction::RestoreWindow => "restore_window",
            AppAction::ToggleGuake => "toggle_guake",
        }
    }

    pub const ALL: &'static [(AppAction, &'static str)] = &[
        (AppAction::NewTab, "New tab"),
        (AppAction::CloseTab, "Close tab"),
        (AppAction::NextTab, "Next tab"),
        (AppAction::PrevTab, "Previous tab"),
        (AppAction::FirstTab, "Jump to first tab"),
        (AppAction::LastTab, "Jump to last tab"),
        (AppAction::MoveTabLeft, "Move tab left"),
        (AppAction::MoveTabRight, "Move tab right"),
        (AppAction::SplitHorizontal, "Split pane horizontally (─ divider)"),
        (AppAction::SplitVertical, "Split pane vertically (│ divider)"),
        (AppAction::SplitAuto, "Split pane (auto direction)"),
        (AppAction::ClosePane, "Close pane"),
        (AppAction::FocusNextPane, "Focus next pane"),
        (AppAction::FocusFirstPane, "Focus first pane"),
        (AppAction::FocusLastPane, "Focus last pane"),
        (AppAction::FocusPrevPane, "Focus previous pane"),
        (AppAction::PasteClipboard, "Paste clipboard"),
        (AppAction::CopySelection, "Copy selection"),
        (AppAction::ClearSelection, "Clear selection"),
        (AppAction::StartSearch, "Search scrollback"),
        (AppAction::JumpPrevPrompt, "Jump to previous prompt"),
        (AppAction::JumpNextPrompt, "Jump to next prompt"),
        (AppAction::JumpPrevCommand, "Jump to previous command"),
        (AppAction::JumpNextCommand, "Jump to next command"),
        (AppAction::ScrollPageUp, "Scrollback: page up"),
        (AppAction::ScrollPageDown, "Scrollback: page down"),
        (AppAction::ScrollHalfPageUp, "Scrollback: half-page up"),
        (AppAction::ScrollHalfPageDown, "Scrollback: half-page down"),
        (AppAction::ScrollLineUp, "Scrollback: line up"),
        (AppAction::ScrollLineDown, "Scrollback: line down"),
        (AppAction::ScrollHome, "Scrollback: oldest"),
        (AppAction::ScrollEnd, "Scrollback: live (newest)"),
        (AppAction::ClearScrollback, "Clear scrollback (saved lines)"),
        (AppAction::ResizePaneLeft, "Resize pane: left"),
        (AppAction::ResizePaneRight, "Resize pane: right"),
        (AppAction::ResizePaneUp, "Resize pane: up"),
        (AppAction::ResizePaneDown, "Resize pane: down"),
        (AppAction::OpenPalette, "Open command palette"),
        (AppAction::SaveScrollback, "Save scrollback to file"),
        (AppAction::ZoomPane, "Zoom / unzoom focused pane"),
        (AppAction::BalancePanes, "Balance pane sizes"),
        (AppAction::Quit, "Quit rterm"),
        (AppAction::FontIncrease, "Font size: bigger"),
        (AppAction::FontDecrease, "Font size: smaller"),
        (AppAction::FontReset, "Font size: reset"),
        (AppAction::OpacityIncrease, "Opacity: more opaque"),
        (AppAction::OpacityDecrease, "Opacity: more transparent"),
        (AppAction::OpacityReset, "Opacity: reset"),
        (AppAction::ToggleLastTab, "Switch to last tab"),
        (AppAction::CopyHoveredUrl, "Copy URL under cursor"),
        (AppAction::ResetPane, "Reset focused pane (RIS)"),
        (AppAction::SwapPaneNext, "Swap focused pane with next"),
        (AppAction::SwapPanePrev, "Swap focused pane with previous"),
        (AppAction::ToggleBellMute, "Toggle bell mute (focused pane)"),
        (AppAction::OpenHoveredUrl, "Open URL under cursor in browser"),
        (AppAction::ToggleHelp, "Toggle help overlay"),
        (AppAction::CycleTheme, "Theme: cycle to next built-in"),
        (AppAction::CycleThemePrev, "Theme: cycle to previous built-in"),
        (AppAction::OpenSettings, "Open settings overlay"),
        (AppAction::RenameTab, "Rename focused tab…"),
        (AppAction::SnapWindowLeft, "Window: snap to left half"),
        (AppAction::SnapWindowRight, "Window: snap to right half"),
        (AppAction::SnapWindowTop, "Window: snap to top / maximize"),
        (AppAction::SnapWindowBottom, "Window: snap to bottom half"),
        (AppAction::MaximizeToggle, "Window: toggle maximize"),
        (AppAction::MinimizeWindow, "Window: minimize"),
        (AppAction::RestoreWindow, "Window: restore to default size"),
        (AppAction::ToggleGuake, "Window: Guake drop-down toggle"),
    ];
}
