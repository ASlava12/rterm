//! Input delivery to the PTY: clipboard / primary-selection paste, the
//! shared paste-commit path (bracketed-paste wrapping + marker
//! stripping), and key → byte forwarding that feeds keystrokes to the
//! focused pane — or every pane in the active tab when broadcast is on.
//!
//! Extracted from `lib.rs` as an `impl App` block (same pattern as
//! `window_ops` / `overlay` / `layout`). Behaviour is unchanged; the
//! paste-safety and key-encoding logic just lives in one place instead
//! of the multi-thousand-line renderer core.

use std::sync::atomic::Ordering;

use crate::clipboard::clipboard_set;
use crate::highlight;

use winit::event::{ElementState, KeyEvent, MouseScrollDelta};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::{
    clamp_scroll_offset, ctrl_byte, encode_mouse, is_bare_modifier_key,
    mouse_mode_for,
    named_key_bytes, paste_confirm, word_back_delete_index, App, MenuItem, ScrollNav,
    SelectionPoint,
};

impl App {
    pub(crate) fn handle_scroll(&mut self, delta: MouseScrollDelta) {
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

    pub(crate) fn paste_clipboard(&mut self) {
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
    pub(crate) fn paste_primary(&mut self) {
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
    pub(crate) fn write_paste(&mut self, text: &str) {
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
    pub(crate) fn commit_paste_now(&mut self, text: &str, target_uid: Option<u64>) {
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

    /// Deliver `bytes` to the keyboard-input target(s): the focused
    /// pane normally, or EVERY pane in the active tab when broadcast is
    /// on. Also resets each target's scrollback view to live and marks
    /// it for redraw, matching the single-pane behaviour.
    pub(crate) fn dispatch_input_bytes(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.broadcast_input {
            if let Some(tab) = self.active_tab() {
                for pane in tab.panes() {
                    pane.scroll_offset.store(0, Ordering::Relaxed);
                    pane.send_input(bytes);
                }
            }
        } else if let Some(pane) = self.focused_pane() {
            pane.scroll_offset.store(0, Ordering::Relaxed);
            pane.send_input(bytes);
        }
    }

    pub(crate) fn forward_key_to_pty(&self, event: &KeyEvent) {
        let Some(pane) = self.focused_pane() else { return };
        // Note: the App-level handler clears `self.selection` and resets the
        // cursor blink phase after this. `app_cursor` is read from the
        // FOCUSED pane's mode even when broadcasting — a single mode is
        // the pragmatic choice (panes rarely disagree on DECCKM).
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
                self.dispatch_input_bytes(&bytes);
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
                        self.dispatch_input_bytes(&[0x1b, m]);
                    } else {
                        self.dispatch_input_bytes(&[m]);
                    }
                    return;
                }
            }
            if alt {
                let mut out = Vec::with_capacity(text.len() + 1);
                out.push(0x1b);
                out.extend_from_slice(text.as_bytes());
                self.dispatch_input_bytes(&out);
            } else {
                self.dispatch_input_bytes(text.as_bytes());
            }
        }
    }

    /// Top-level keyboard entry. Returns `true` if the window should exit.
    pub(crate) fn handle_key(&mut self, event: &KeyEvent) -> bool {
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
    pub(crate) fn handle_palette_key(&mut self, event: &KeyEvent) -> bool {
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


    pub(crate) fn handle_search_key(&mut self, event: &KeyEvent) {
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

    /// Handle Shift+PageUp/PageDown/Home/End for keyboard scrollback nav.
    /// Returns `true` if the event was consumed.
    pub(crate) fn handle_scroll_key(&self, event: &KeyEvent) -> bool {
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

    pub(crate) fn handle_rename_key(&mut self, event: &KeyEvent) -> bool {
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

    /// Consume a key while the settings overlay is open. Returns `true`
    /// when the window should exit (only via the Quit shortcut delegated
    /// to `handle_app_shortcut`).
    pub(crate) fn handle_settings_key(&mut self, event: &KeyEvent) -> bool {
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

    /// Keyboard input while a context menu is open: arrows + Enter.
    pub(crate) fn handle_context_menu_key(&mut self, event: &KeyEvent) -> bool {
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
    pub(crate) fn handle_paste_confirmation_key(&mut self, event: &KeyEvent) {
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
    pub(crate) fn handle_suggestion_popup_key(&mut self, event: &KeyEvent) -> bool {
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

    /// Update mouse-hover on the context menu when the cursor moves.
    pub(crate) fn update_context_menu_hover(&mut self, x: f64, y: f64) {
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

    /// Mouse-click on the paste-confirmation modal. Confirm mode:
    /// hit-test the button row. Edit mode: project the click onto
    /// a (line, column) in the buffer and move the cursor there.
    pub(crate) fn handle_paste_confirmation_press(&mut self, x: f64, y: f64) {
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

    /// Mouse-wheel handling for the paste-confirmation modal. In
    /// edit mode, the wheel shifts the VIEWPORT (not the cursor) by
    /// a few lines per tick — same convention as every text editor.
    /// The cursor stays where the user left it; if they keep
    /// scrolling past it, the caret simply scrolls off-screen. As
    /// soon as they type / arrow, the clamp pulls the viewport back.
    /// In confirm mode, the wheel is absorbed silently to keep a
    /// stray scroll from leaking through to the pane below.
    pub(crate) fn handle_paste_confirmation_wheel(&mut self, delta: MouseScrollDelta) {
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
    pub(crate) fn handle_suggestion_popup_press(&mut self, x: f64, y: f64) -> bool {
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
}
