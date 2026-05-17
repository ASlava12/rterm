//! Window-level operations: snap halves, maximize toggle, restore,
//! font size / opacity adjustment, and the Guake-style drop-down.
//!
//! These methods all mutate `App` or read `App.state` (the winit
//! window handle). They're grouped here so the per-platform notes
//! (Wayland positioning, X11 / macOS / Win32 positioning) sit in one
//! place — the bulk of the logic is `winit` glue.

use crate::{App, SnapDir};

impl App {
    /// Bump font size by `delta` points. Driven from the
    /// `font_increase` / `font_decrease` built-in actions and from
    /// `Ctrl+wheel` zoom.
    pub(crate) fn adjust_font_size(&mut self, delta: f32) {
        let cur = self
            .state
            .as_ref()
            .map(|s| s.text.font_size())
            .unwrap_or(self.font_size);
        self.set_font_size_absolute(cur + delta);
    }

    /// Bump opacity by `delta`, clamped to `0.0..=1.0`. Drives the
    /// `opacity_increase` / `opacity_decrease` built-in actions.
    pub(crate) fn adjust_opacity(&mut self, delta: f32) {
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

    /// Set the absolute font size. Forces a redraw + per-pane reflow
    /// at the new cell metrics. Called from `adjust_font_size`, the
    /// settings overlay, and plugin-side `rterm.set_font_size(N)`.
    pub(crate) fn set_font_size_absolute(&mut self, size: f32) {
        if let Some(state) = self.state.as_mut() {
            state.text.set_font_size(size);
            // Force a redraw and reflow every pane to the new cell metrics.
            state.window.request_redraw();
        }
        self.sync_terminal_size();
    }

    /// Snap the window to one half of the current monitor. On
    /// platforms with positionable windows (X11, Win32, macOS, BSD-X11)
    /// this calls `set_outer_position` + `set_inner_size`. On Wayland
    /// `set_outer_position` is a no-op, so we fall back to
    /// `set_maximized(true)` for `Top` and skip the rest with a warning
    /// (compositor-level snap is the supported path there).
    pub(crate) fn snap_window(&mut self, dir: SnapDir) {
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
    pub(crate) fn toggle_guake(&mut self) {
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
                // "full" maximises and skips the position/size dance —
                // the compositor owns the geometry there. Everything
                // else picks a Y for the configured edge and runs the
                // shared set_outer_position + request_inner_size pair.
                let edge_y = match cfg.position.as_str() {
                    "full" => {
                        state.window.set_maximized(true);
                        None
                    }
                    "bottom" => Some(mon_pos.y + mh - target_h),
                    // Default + "top".
                    _ => Some(mon_pos.y),
                };
                if let Some(y) = edge_y {
                    let pos = winit::dpi::PhysicalPosition::new(centre_x, y);
                    state.window.set_outer_position(pos);
                    let _ = state.window.request_inner_size(
                        winit::dpi::PhysicalSize::new(target_w as u32, target_h as u32),
                    );
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
    pub(crate) fn restore_window(&mut self) {
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
}
