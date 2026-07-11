//! The winit event-loop driver: `impl ApplicationHandler<UserEvent> for
//! App`. Extracted verbatim from `lib.rs` — this is the ~2.8k-line
//! `resumed` / `new_events` / `user_event` / `exiting` / `window_event`
//! block, including the `RedrawRequested` frame pipeline (state
//! snapshot for plugins + GPU prepare/render). Behaviour is unchanged;
//! it just no longer sits inside the renderer core file.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rterm_core::MouseTracking;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, Modifiers, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::ModifiersState;
use winit::window::{CursorIcon, WindowAttributes, WindowId};

use crate::bg;
use crate::clipboard::clipboard_set;
use crate::palette;
use crate::{
    App, AppAction, BELL_FLASH_FADE_MS, CURSOR_SHAPE_NAMES, GpuState, HeaderDraw, HeaderRightDraw, HeaderTabsDraw, HeaderTabsGhostDraw, MOUSE_MODE_NAMES, OverlayDraw, PaneDraw, PaneRect, PaneSnapshotInfo, PreeditDraw, RESIZE_DEBOUNCE_MS, SCROLLBACK_TAIL_MAX, SelectionPoint, SplitDir, StatusBarDraw, TAB_CONTROLS_GAP_CELLS, TAB_SWAP_ANIM_MS, TAB_SWITCH_ANIM_MS, TabSnapshotInfo, TerminalSnapshot, UserEvent, WINDOW_CONTROLS_WIDTH_CELLS, clamp_scroll_offset, cursor_shape_code, drain_osc52, grid_text_snapshot, ime_cursor_rect, mouse_mode_code, pane_attr_payload, pane_command_finish_payload, pane_edge_payload, pane_text_payload, pane_value_uid_payload, pid_cwd_fallback, progress_payload, progress_severity, read_proc_comm_or_none, scrollback_text_snapshot, scrollback_text_snapshot_capped, tab_event_payload, tab_progress_payload, tab_title_payload,
};

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
                        // Only apply the per-tab title when a tab was
                        // actually pushed — a failed spawn would otherwise
                        // graft this entry's title onto the PREVIOUS tab
                        // (and emit a wrong tab index).
                        if !self.new_tab_in(entry.cwd.as_deref()) {
                            continue;
                        }
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
                            // one until the next title change. `tabs.len()`
                            // is the just-pushed tab's 1-based index.
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
                    // Enable IME so composed input (CJK, dead keys,
                    // macOS long-press accents) reaches the terminal via
                    // `WindowEvent::Ime`. Without this winit never emits
                    // Ime events and those keystrokes are lost.
                    state.window.set_ime_allowed(true);
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
            // `handle_key` does the real work (PTY writes, clipboard,
            // tab mutation) and returns `true` only for the Quit path.
            // Deliberately NOT a match guard: guards are expected to be
            // pure, and hiding these side effects in a condition means a
            // future arm inserted before this one could change dispatch
            // subtly. `collapsible_match` wants the guard form; we keep
            // the explicit body.
            #[allow(clippy::collapsible_match)]
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if self.handle_key(&key_event) {
                    event_loop.exit();
                }
            }
            WindowEvent::Ime(ime) => {
                use winit::event::Ime;
                match ime {
                    // Final composed text (CJK, dead keys, macOS
                    // long-press accents) — send it to the focused
                    // pane's shell exactly like typed input, and clear
                    // any composition preview.
                    Ime::Commit(text) => {
                        self.ime_preedit.clear();
                        self.ime_anchor = None;
                        if !text.is_empty() {
                            // Route through the broadcast-aware dispatch so
                            // committed IME text reaches every pane when
                            // broadcast is on (and resets the scroll offset),
                            // exactly like ordinary typed keystrokes —
                            // previously it went only to the focused pane.
                            self.dispatch_input_bytes(text.as_bytes());
                        }
                    }
                    // Composition in progress — store the preedit for
                    // inline rendering at the cursor, and anchor it to the
                    // pane it belongs to so a later focus change can drop it.
                    Ime::Preedit(text, _range) => {
                        self.ime_preedit = text;
                        self.ime_anchor = if self.ime_preedit.is_empty() {
                            None
                        } else {
                            self.focused_pane().map(|p| p.uid)
                        };
                    }
                    // Composition ended / IME turned off — nothing is
                    // being composed anymore.
                    Ime::Disabled => {
                        self.ime_preedit.clear();
                        self.ime_anchor = None;
                    }
                    Ime::Enabled => {}
                }
                // Re-anchor the IME candidate window to the terminal
                // cursor so composition renders where the text lands.
                self.update_ime_cursor_area();
                if let Some(state) = self.state.as_ref() {
                    state.window.request_redraw();
                }
            }
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
                // Drag flavours that fire `handle_drag`: text selection
                // inside a pane (`mouse_dragging`), split-divider drag
                // (`gap_dragging`), tab-strip live-reorder (`tab_dragging`),
                // and — crucially — a button held in a pane whose shell
                // enabled motion reporting (`mouse_pty_pane`, ?1002/?1003).
                // Without that last gate `handle_drag`'s motion-report block
                // was dead code, so drag-select in vim (`mouse=a`) / tmux
                // copy-mode sent only press+release, never the motion
                // reports in between.
                if self.mouse_dragging
                    || self.gap_dragging.is_some()
                    || self.tab_dragging.is_some()
                    || self.tab_drag_pending.is_some()
                    || self.mouse_pty_pane.is_some()
                {
                    self.handle_drag(position.x, position.y);
                } else {
                    // No drag in progress: report bare-hover motion to a
                    // pane running any-event tracking (?1003). Inert for
                    // every other mode.
                    self.report_hover_motion(position.x, position.y);
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
                    // `close_tab_at` already handles the "restore the
                    // user's previous focus, shifted for the removed
                    // slot" dance — no inline copy to drift.
                    if self.close_tab_at(t) {
                        event_loop.exit();
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
            WindowEvent::RedrawRequested => self.on_redraw(event_loop),
            _ => {}
        }
        if needs_repaint {
            if let Some(state) = self.state.as_ref() {
                state.window.request_redraw();
            }
        }
    }
}

impl App {
    /// The `RedrawRequested` handler: build the per-frame plugin state
    /// snapshot, drain queued plugin commands, then run the GPU frame
    /// (prepare + present) and schedule the next wake. Split out of
    /// `window_event` so that ~2.4k-line arm reads as a single call;
    /// pure code motion — the body is verbatim (dedented one level).
    fn on_redraw(&mut self, event_loop: &ActiveEventLoop) {
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
                            let next = (cur_off as usize + grew).min(cur_sb).min(u32::MAX as usize);
                            pane.scroll_offset.store(next as u32, Ordering::Relaxed);
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
                            .store(new_len.min(u32::MAX as usize) as u32, Ordering::Relaxed);
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

        // Plugin-triggered attention (`rterm.attention()`): fire the
        // documented `attention` event so other plugins can react
        // (`rterm.on("attention", ...)`), and ping the taskbar when the
        // window is unfocused. The event fires regardless of focus — the
        // taskbar ping is the focus-gated part.
        if self.events.take_pending_attention() {
            self.events.emit("attention", "");
            if !self.window_focused {
                if let Some(s) = self.state.as_ref() {
                    s.window.request_user_attention(Some(
                        winit::window::UserAttentionType::Informational,
                    ));
                }
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
                        // Not a built-in — route to the plugin host's custom
                        // action registry, matching the command palette's
                        // dispatch (lib.rs `execute_palette_selection`). Lets
                        // `rterm.run_action("my_custom")` fire actions declared
                        // via `rterm.register_action`.
                        self.events.run_action(&name);
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
                    sb_len.saturating_sub(line).min(u32::MAX as usize) as u32;
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
        // No separate title-bar row — the window title is folded
        // into the single-row header (controls live next to
        // tabs). The status bar at the bottom is unchanged.
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
        // IME preedit: composition text drawn inline at the
        // cursor. Computed here (before the `self.state.as_mut()`
        // borrow, which `focused_pane_rect` / cursor reads would
        // otherwise conflict with) as `(text, rect)`. `None`
        // when nothing is composing. The rect starts at the
        // cursor pixel and spans the composition's display width.
        // Drop a stale composition if focus moved to a different pane while
        // composing — otherwise the preedit would render on (and a later
        // commit could leak into) the wrong terminal. Pane UIDs are stable
        // for a pane's lifetime, so this only fires on a real focus change,
        // never mid-composition on the same pane.
        if !self.ime_preedit.is_empty()
            && self.focused_pane().map(|p| p.uid) != self.ime_anchor
        {
            self.ime_preedit.clear();
            self.ime_anchor = None;
        }
        let preedit_info: Option<(String, PaneRect)> =
            if self.ime_preedit.is_empty() {
                None
            } else {
                self.state.as_ref().and_then(|s| {
                    let cell_w = s.text.cell_width();
                    let line_h = s.text.line_height();
                    let rect = self.focused_pane_rect()?;
                    let (col, row) = self
                        .focused_pane()
                        .and_then(|p| p.terminal.lock().ok().map(|t| t.cursor()))
                        .map(|c| (c.col, c.row))?;
                    let (x, y, _, h) =
                        ime_cursor_rect(rect, col, row, cell_w, line_h);
                    use unicode_width::UnicodeWidthStr;
                    let w = (self.ime_preedit.width() as f32 * cell_w).max(cell_w);
                    // Clamp to the focused pane's right edge so a long
                    // composition near the right of a split doesn't paint
                    // its backdrop quad + glyphs over the neighbouring pane.
                    let max_w = (rect.left + rect.width - x).max(cell_w);
                    let w = w.min(max_w);
                    Some((
                        self.ime_preedit.clone(),
                        PaneRect { left: x, top: y, width: w, height: h },
                    ))
                })
            };
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
            let status_bar_draw =
                status_bar.map(|(spans, rect)| StatusBarDraw { spans, rect });
            // IME preedit inline draw: accent-coloured composition
            // spans at the cursor. Storage outlives the borrowed
            // spans below.
            let preedit_accent: [u8; 3] = [235, 200, 120];
            let preedit_draw = preedit_info.as_ref().map(|(text, rect)| PreeditDraw {
                spans: vec![(text.as_str(), preedit_accent, false)],
                rect: *rect,
            });
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
            // The paste modal (BOTH Edit and Confirm) needs
            // wrap-off. Edit's click-to-cursor relies on it; the
            // Confirm button hit-test derives the button row as a
            // logical-line count, so a wrapped preview line would
            // push the visual button row below its hit rect.
            let overlay_nowrap = settings_overlay_shown
                || self.paste_confirmation.is_some();
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
            // Solid backdrop behind the IME preedit so the
            // composition text stays legible over whatever cells
            // sit under the cursor. Drawn after pane glyphs and
            // under the preedit text (which is in `main_areas`).
            if let Some((_, rect)) = preedit_info.as_ref() {
                after_panes_quads.push(bg::BgQuad::from_srgb(
                    [rect.left, rect.top],
                    [rect.width, rect.height],
                    [40, 44, 52],
                    0.96,
                ));
            }
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
                status_bar_draw.as_ref(),
                preedit_draw.as_ref(),
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
}
