//! All user input for the renderer, extracted from `lib.rs` as an
//! `impl App` block (same pattern as `window_ops` / `overlay` /
//! `layout`). Behaviour is unchanged — this just keeps the input
//! surface in one place instead of the multi-thousand-line core:
//!
//! * **Paste** — clipboard / primary-selection reads, the shared
//!   paste-commit path (bracketed-paste wrapping + marker stripping).
//! * **Keyboard** — key → byte forwarding to the focused pane (or every
//!   pane when broadcast is on), `handle_key` and every overlay
//!   sub-dispatcher (palette / search / settings / context-menu /
//!   rename / paste-confirm / suggestion-popup / scrollback nav).
//! * **Mouse** — wheel routing (`handle_scroll`), press / drag
//!   selection (`handle_press` / `handle_drag`), and the modal / popup
//!   click entry-points.
//!
//! Deep hit-test geometry (`pixel_to_cell`, `*_rect`, `abs_point`, …)
//! stays in `lib.rs`; these methods reach it as crate-root-private
//! methods callable from this descendant module.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rterm_core::MouseTracking;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::clipboard::clipboard_set;
use crate::highlight;
use crate::{
    clamp_scroll_offset, encode_mouse, mouse_mod_bits, mouse_mode_for,
    paste_confirm, tab_drag_exceeds_threshold, word_back_delete_index,
    ActiveSelection, App, MenuItem, ScrollNav, SelectionMode, SelectionPoint, TabHit,
    TabSwapAnim, MULTI_CLICK_INTERVAL,
};

impl App {
    pub(crate) fn handle_scroll(&mut self, delta: MouseScrollDelta) {
        // Translate the wheel delta into a raw notch count and an
        // amplified line count. The 3× multiplier makes CONTENT scrolling
        // (local scrollback, alt-screen pagers, overlays) feel right — the
        // raw notch was uncomfortably slow, tuned by feel against Chrome /
        // VSCode. But mouse-reporting apps (?1000–?1003) do their OWN
        // scroll-speed handling, so they must get the RAW notch count (one
        // report per physical notch, like every other terminal) —
        // amplifying there tripled their scroll.
        const WHEEL_SPEED_MULT: f32 = 3.0;
        let base_f = match delta {
            MouseScrollDelta::LineDelta(_, y) => y,
            MouseScrollDelta::PixelDelta(p) => {
                let line_h = self
                    .state
                    .as_ref()
                    .map(|s| s.text.line_height())
                    .unwrap_or(16.0)
                    .max(1.0);
                (p.y as f32) / line_h
            }
        };
        let step = (base_f * WHEEL_SPEED_MULT).round() as i32;
        if step == 0 {
            return;
        }
        // Raw notches for mouse-report forwarding; guarantee at least one
        // in the scroll direction whenever we scrolled (a sub-notch
        // trackpad delta reports a single event, not zero).
        let notch = {
            let n = base_f.round() as i32;
            if n == 0 { step.signum() } else { n }
        };
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
        if let Some((_mode, sgr, pixel)) = mouse_mode_for(pane) {
            let (cx, cy) =
                self.mouse_report_coords(target_idx, self.cursor_pos.x, self.cursor_pos.y, pixel);
            // Forward the RAW notch count (not the 3×-amplified `step`) —
            // the app scrolls by its own amount per report.
            let button =
                (if notch > 0 { 64 } else { 65 }) | mouse_mod_bits(self.modifiers);
            for _ in 0..notch.unsigned_abs() {
                let bytes = encode_mouse(sgr, button, cx, cy, true);
                pane.send_input(&bytes);
            }
            return;
        }
        // Else: no mouse reporting. On the alt-screen, translate the wheel
        // into cursor up/down keys when alternate-scroll (DECSET ?1007) is
        // on, so pagers (`less` / `man` / `git log` / `systemctl`) that
        // enter the alt-screen but never enable mouse tracking still
        // scroll. Off the alt-screen, drive the local scrollback view —
        // no-op on alt since `visible_row` pins the viewport to the alt
        // grid there, so updating `scroll_offset` would be dead state.
        let (max_offset, on_alt, alt_scroll, app_cursor) = if let Ok(term) = pane.terminal.lock() {
            (
                term.scrollback_len() as i32,
                term.is_on_alt_screen(),
                term.alternate_scroll(),
                term.app_cursor_keys(),
            )
        } else {
            (0, false, false, false)
        };
        if on_alt {
            if alt_scroll {
                let bytes = alt_scroll_bytes(step, app_cursor);
                pane.send_input(&bytes);
            }
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
        // While an IME composition is active, keys belong to the IME, not
        // the shell. Some platforms (Windows IMM, X11 ibus/fcitx) still
        // deliver a `KeyboardInput` alongside the `Ime` events; without
        // this guard a Backspace editing the preedit would ALSO send `\x7f`
        // to the PTY (corrupting the line), and a candidate-selection key
        // would double-fire. `ime_preedit` is empty in all non-IME use, so
        // this is a no-op for ordinary typing. (The confirming Enter that
        // arrives *after* `Ime::Commit` cleared the preedit is deliberately
        // NOT swallowed here — distinguishing it from a genuine submit is
        // platform-fragile, and erring toward "let Enter through" avoids
        // eating a real command submission.)
        if !self.ime_preedit.is_empty() {
            return;
        }
        let Some(pane) = self.focused_pane() else { return };
        // Note: the App-level handler clears `self.selection` and resets the
        // cursor blink phase after this. `app_cursor` is read from the
        // FOCUSED pane's mode even when broadcasting — a single mode is
        // the pragmatic choice (panes rarely disagree on DECCKM).
        let ctrl = self.modifiers.contains(ModifiersState::CONTROL);
        let alt = self.modifiers.contains(ModifiersState::ALT);
        let (app_cursor, kitty_flags) = pane
            .terminal
            .lock()
            .map(|t| (t.app_cursor_keys(), t.kitty_keyboard_flags()))
            .unwrap_or((false, 0));

        // Kitty keyboard protocol: when the focused pane has pushed
        // enhanced flags, encode text keys / Escape in the `CSI … u` form.
        // Functional keys return None and fall through to the legacy path,
        // which already emits the xterm modifier form the protocol reuses.
        if kitty_flags != 0 {
            if let Some(bytes) = kitty_encode_key(
                &event.logical_key,
                event.text.as_deref(),
                self.modifiers,
                event.repeat,
                kitty_flags,
            ) {
                self.events.emit("key", &format!("{:?}", event.logical_key));
                self.dispatch_input_bytes(&bytes);
                return;
            }
        }

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
        // Allow whatever key opened the overlay (the `open_settings`
        // action — no default binding, so a user-defined one) to close
        // it too, plus app-shortcuts like Ctrl+Q for quit.
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

    /// Returns `true` if the press caused the window to want exit
    /// (e.g. clicking × on the very last tab). The caller is expected
    /// to forward that to `event_loop.exit()`.
    #[must_use]
    pub(crate) fn handle_press(&mut self, x: f64, y: f64) -> bool {
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
        // Paste-confirmation modal is up. Clicks INSIDE the modal
        // card go to its own handler (buttons / cursor placement).
        // Clicks OUTSIDE the card pass through to the window
        // chrome only — resize edges, min/max/close, title-bar
        // drag — so the user can still move / resize / close the
        // window while the modal is open. All other outside clicks
        // (tab strip, hamburger, pane area) are absorbed so a
        // stray click can't quietly shift focus underneath.
        if self.paste_confirmation.is_some() {
            let modal_rect = self
                .paste_confirmation
                .as_ref()
                .and_then(|m| self.paste_confirmation_rect(m));
            let inside_modal = modal_rect.is_some_and(|r| {
                let xf = x as f32;
                let yf = y as f32;
                xf >= r.left
                    && xf < r.left + r.width
                    && yf >= r.top
                    && yf < r.top + r.height
            });
            if inside_modal {
                self.handle_paste_confirmation_press(x, y);
                return false;
            }
            if !self.os_decorations {
                if let Some(dir) = self.window_edge_at(x, y) {
                    if let Some(state) = self.state.as_ref() {
                        let _ = state.window.drag_resize_window(dir);
                    }
                    return false;
                }
            }
            if let Some(which) = self.window_control_at(x, y) {
                return self.click_window_control(which);
            }
            if let Some(rect) = self.header_rect() {
                let yf = y as f32;
                if yf >= rect.top && yf < rect.top + rect.height {
                    let on_chrome = self.tab_at(x, y).is_some()
                        || self.hamburger_at(x, y)
                        || self.new_tab_button_at(x, y);
                    if !on_chrome && !self.os_decorations {
                        if let Some(state) = self.state.as_ref() {
                            let _ = state.window.drag_window();
                        }
                    }
                }
            }
            return false;
        }
        // Suggestion popup: a click inside the popup picks the row
        // under the cursor + injects it (same effect as TAB after
        // ↓ nav). A click OUTSIDE the popup closes it but lets the
        // event continue to the normal pane-click handlers, so the
        // user can e.g. click a different pane to shift focus
        // while a popup was visible.
        if self.suggestion_popup.is_some() {
            if self.handle_suggestion_popup_press(x, y) {
                return false;
            }
            self.suggestion_popup = None;
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
            // Arm a PENDING drag — a real drag (with `tab.drag_start`
            // and reordering) only begins once the cursor moves past
            // `TAB_DRAG_THRESHOLD_PX` (see `handle_drag`). This keeps a
            // plain click from emitting a spurious start/end drag pair.
            // Capture the within-chip press offset now so the ghost
            // chip sits under the cursor without jumping when the drag
            // does start. `tab_layout` gives each tab's pixel left edge.
            let press_offset = self
                .tab_layout()
                .and_then(|l| l.entries.iter().find(|e| e.idx == t).map(|e| x - e.left))
                .unwrap_or(0.0);
            self.tab_drag_pending = Some((t, x, y, press_offset));
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
                    if let Some(ap) = self.abs_point(i, p) {
                        if let Some(sel) = self.selection.as_mut() {
                            sel.focus = ap;
                        }
                        self.mouse_dragging = true;
                        return false;
                    }
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
            if let Some((_mode, sgr, pixel)) = mouse_mode_for(pane) {
                let (cx, cy) = self.mouse_report_coords(i, x, y, pixel);
                let bytes = encode_mouse(sgr, mouse_mod_bits(self.modifiers), cx, cy, true);
                pane.send_input(&bytes);
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
                // `Alt+drag` triggers rectangular (block) selection —
                // matches Terminator / iTerm2 / kitty. `Ctrl` is
                // reserved for "open URL under cursor" (single click)
                // so it can't double as the block modifier. The mode
                // sticks until mouse-up; if the user is still holding
                // Alt when they later double-click, the second click
                // dispatches through the `count == 2` arm above and
                // we drop back into Word mode — same as Terminator.
                let mode = if self.modifiers.contains(ModifiersState::ALT) {
                    SelectionMode::Block
                } else {
                    SelectionMode::Char
                };
                let Some(ap) = self.abs_point(i, p) else { return false };
                self.selection = Some(ActiveSelection {
                    pane_idx: i,
                    anchor: ap,
                    focus: ap,
                    mode,
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
                    let Some(start_abs) = self.abs_point(i, sel.start) else { return false };
                    let Some(end_abs) = self.abs_point(i, end_inclusive) else { return false };
                    self.selection = Some(ActiveSelection {
                        pane_idx: i,
                        anchor: start_abs,
                        focus: end_abs,
                        mode: SelectionMode::Word,
                        pivot: Some((start_abs, end_abs)),
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
                    let Some(start_abs) = self.abs_point(i, sel.start) else { return false };
                    let Some(end_abs) = self.abs_point(i, end_inclusive) else { return false };
                    self.selection = Some(ActiveSelection {
                        pane_idx: i,
                        anchor: start_abs,
                        focus: end_abs,
                        mode: SelectionMode::Line,
                        pivot: Some((start_abs, end_abs)),
                    });
                    self.mouse_dragging = true;
                }
            }
        }
        false
    }

    /// Report bare-hover motion (no button held) to a pane running
    /// any-event mouse tracking (`?1003`). Deduped by reported cell (or
    /// pixel under `?1016`) so a TUI gets one motion event per cell
    /// crossed, not one per raw `CursorMoved`. Inert for every other mouse
    /// mode — `?1000`/`?1002` don't report button-less motion — so it does
    /// nothing unless an app explicitly turns on `?1003`.
    pub(crate) fn report_hover_motion(&mut self, x: f64, y: f64) {
        let Some(idx) = self.pane_at(x, y) else {
            // Left every pane — reset the dedup so re-entry reports fresh.
            self.last_hover_report = None;
            return;
        };
        // Read the mode, dropping the pane borrow before mutating `self`.
        let mode_info = self
            .active_tab()
            .and_then(|t| t.pane_at(idx))
            .and_then(mouse_mode_for);
        let Some((mode, sgr, pixel)) = mode_info else {
            self.last_hover_report = None;
            return;
        };
        if mode != MouseTracking::AnyEvent {
            return;
        }
        let (cx, cy) = self.mouse_report_coords(idx, x, y, pixel);
        if self.last_hover_report == Some((idx, cx, cy)) {
            return; // same cell/pixel — already reported
        }
        self.last_hover_report = Some((idx, cx, cy));
        // Button 3 (no button) + 32 (motion bit) + any modifiers.
        let button = 35 | mouse_mod_bits(self.modifiers);
        let bytes = encode_mouse(sgr, button, cx, cy, true);
        if let Some(pane) = self.active_tab().and_then(|t| t.pane_at(idx)) {
            pane.send_input(&bytes);
        }
    }

    pub(crate) fn handle_drag(&mut self, x: f64, y: f64) {
        // Promote a pending tab press to a real drag once the cursor
        // has moved past the threshold — only THEN fire `tab.drag_start`
        // and begin reordering. A press that never moves far enough
        // stays a plain click.
        if let Some((t, px, py, offset)) = self.tab_drag_pending {
            if tab_drag_exceeds_threshold(px, py, x, y) {
                self.tab_drag_pending = None;
                self.tab_dragging = Some(t);
                self.tab_drag_press_offset = offset;
                self.events.emit("tab.drag_start", &(t + 1).to_string());
            }
        }
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
                if let Some((mode, sgr, pixel)) = mouse_mode_for(pane) {
                    if matches!(mode, MouseTracking::ButtonEvent | MouseTracking::AnyEvent) {
                        // Button 0 (left) with +32 motion bit + modifiers.
                        let (cx, cy) = self.mouse_report_coords(i, x, y, pixel);
                        let bytes =
                            encode_mouse(sgr, 32 | mouse_mod_bits(self.modifiers), cx, cy, true);
                        pane.send_input(&bytes);
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

fn ctrl_byte(b: u8) -> Option<u8> {
    match b {
        b'@'..=b'_' => Some(b & 0x1f),
        b'a'..=b'z' => Some(b - b'a' + 1),
        b'?' => Some(0x7f),
        _ => None,
    }
}

/// Alternate-scroll (DECSET ?1007): translate a wheel `step` into repeated
/// cursor-key bytes for the focused pager. `step > 0` is wheel-up → cursor
/// Up; `step < 0` → cursor Down. `app_cursor` picks the SS3 (`ESC O …`)
/// form over CSI (`ESC [ …`) per DECCKM. Returns empty for `step == 0`.
fn alt_scroll_bytes(step: i32, app_cursor: bool) -> Vec<u8> {
    if step == 0 {
        return Vec::new();
    }
    let seq: &[u8] = match (step > 0, app_cursor) {
        (true, false) => b"\x1b[A",
        (false, false) => b"\x1b[B",
        (true, true) => b"\x1bOA",
        (false, true) => b"\x1bOB",
    };
    let n = step.unsigned_abs() as usize;
    let mut out = Vec::with_capacity(seq.len() * n);
    for _ in 0..n {
        out.extend_from_slice(seq);
    }
    out
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

// Kitty keyboard progressive-enhancement flag bits (see the protocol).
const KITTY_REPORT_EVENTS: u8 = 0b0_0010;
const KITTY_REPORT_ALL_ESC: u8 = 0b0_1000;
const KITTY_REPORT_TEXT: u8 = 0b1_0000;

/// Kitty keyboard modifier bitmask: shift=1, alt=2, ctrl=4, super=8.
fn kitty_mods(mods: ModifiersState) -> u8 {
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
    if mods.contains(ModifiersState::SUPER) {
        m |= 8;
    }
    m
}

/// Encode one key PRESS / REPEAT under the Kitty keyboard protocol given
/// the active `flags` (non-zero). Returns `Some(bytes)` for the keys the
/// protocol sends in `CSI number [;mods[:event]] [;text] u` form — Escape
/// (always, so it's distinguishable from an escape-sequence prefix), the
/// other legacy control keys (Enter / Tab / Backspace / Space) when
/// modified or when "report all keys" is on, and character keys carrying
/// a ctrl/alt/super modifier or under "report all keys". Returns `None` to
/// fall back to the legacy encoding, which already emits the xterm
/// modifier form for functional keys (arrows / F-keys / navigation) that
/// the protocol reuses verbatim.
///
/// Limitations (documented, non-corrupting): the base key code is taken
/// from the logical key, so a shifted number-row symbol (`!`) reports its
/// shifted codepoint rather than the unshifted base — letters, digits,
/// navigation and modifier combos, i.e. the cases apps disambiguate on,
/// are exact. Key-release events aren't reported (key-up is consumed
/// before the PTY-forward path); alternate-key reporting (flag 4) is not
/// emitted. Both degrade gracefully — the base encoding stays correct.
fn kitty_encode_key(
    key: &Key,
    text: Option<&str>,
    mods: ModifiersState,
    repeat: bool,
    flags: u8,
) -> Option<Vec<u8>> {
    let mbits = kitty_mods(mods);
    let non_shift = mbits & 0b1110 != 0; // ctrl / alt / super
    let all_esc = flags & KITTY_REPORT_ALL_ESC != 0;
    let is_escape = matches!(key, Key::Named(NamedKey::Escape));
    let is_char = matches!(key, Key::Character(_));

    let codepoint: u32 = match key {
        Key::Named(NamedKey::Escape) => 27,
        Key::Named(NamedKey::Enter) => 13,
        Key::Named(NamedKey::Tab) => 9,
        Key::Named(NamedKey::Backspace) => 127,
        Key::Named(NamedKey::Space) => 32,
        Key::Character(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    if c.is_ascii_uppercase() {
                        c.to_ascii_lowercase() as u32
                    } else {
                        c as u32
                    }
                }
                // Multi-char / dead-key composition → legacy path.
                _ => return None,
            }
        }
        // Functional keys (arrows, F-keys, Home/End/PgUp/…) → legacy.
        _ => return None,
    };

    let use_csi_u = if is_escape {
        true
    } else if is_char {
        non_shift || all_esc
    } else {
        // Enter / Tab / Backspace / Space: any modifier disambiguates.
        mbits != 0 || all_esc
    };
    if !use_csi_u {
        return None;
    }

    let report_events = flags & KITTY_REPORT_EVENTS != 0;
    let report_text = flags & KITTY_REPORT_TEXT != 0;
    let event = if report_events && repeat { Some(2u8) } else { None };
    // The associated-text field (flag 16) carries the text the key would
    // insert. A ctrl/alt/super combo produces no insertable text (it maps
    // to a control action), so per the Kitty spec the field must be
    // omitted — otherwise e.g. Ctrl+A emits `...;5;97u`, making a
    // Kitty-aware app insert a spurious `a` in addition to handling Ctrl+A.
    // Also drop any control char that slipped into `text`.
    let text_field = if report_text && !non_shift {
        text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
    } else {
        None
    };

    let mut s = format!("\x1b[{codepoint}");
    let mods_param = mbits + 1;
    if mods_param != 1 || event.is_some() || text_field.is_some() {
        s.push(';');
        s.push_str(&mods_param.to_string());
        if let Some(e) = event {
            s.push(':');
            s.push_str(&e.to_string());
        }
    }
    if let Some(t) = text_field {
        s.push(';');
        let cps: Vec<String> = t.chars().map(|c| (c as u32).to_string()).collect();
        s.push_str(&cps.join(":"));
    }
    s.push('u');
    Some(s.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_encode_key_covers_disambiguation_and_flags() {
        let ch = |s: &str| Key::Character(s.into());
        let esc = Key::Named(NamedKey::Escape);
        let tab = Key::Named(NamedKey::Tab);
        let enter = Key::Named(NamedKey::Enter);
        let none = ModifiersState::empty();
        let ctrl = ModifiersState::CONTROL;
        let shift = ModifiersState::SHIFT;
        let alt = ModifiersState::ALT;
        const DISAMBIG: u8 = 0b1;
        const EVENTS: u8 = 0b10;
        const ALL_ESC: u8 = 0b1000;
        const TEXT: u8 = 0b1_0000;

        // Ctrl+A disambiguates to `CSI 97;5u` (a=97, ctrl bit4 → mods 5).
        assert_eq!(
            kitty_encode_key(&ch("a"), Some("\x01"), ctrl, false, DISAMBIG),
            Some(b"\x1b[97;5u".to_vec())
        );
        // Alt+a → mods 3 (alt bit2 + 1).
        assert_eq!(
            kitty_encode_key(&ch("a"), None, alt, false, DISAMBIG),
            Some(b"\x1b[97;3u".to_vec())
        );
        // Uppercase 'A' (shift) uses the lowercased base; shift-only text
        // key stays legacy (None) unless "report all keys".
        assert_eq!(kitty_encode_key(&ch("A"), Some("A"), shift, false, DISAMBIG), None);
        // Plain 'a' with no modifiers → legacy.
        assert_eq!(kitty_encode_key(&ch("a"), Some("a"), none, false, DISAMBIG), None);
        // Escape is always disambiguated → `CSI 27u`.
        assert_eq!(kitty_encode_key(&esc, None, none, false, DISAMBIG), Some(b"\x1b[27u".to_vec()));
        // Shift+Tab → `CSI 9;2u` (any modifier on a control key uses CSI u).
        assert_eq!(kitty_encode_key(&tab, None, shift, false, DISAMBIG), Some(b"\x1b[9;2u".to_vec()));
        // Plain Enter → legacy.
        assert_eq!(kitty_encode_key(&enter, Some("\r"), none, false, DISAMBIG), None);
        // Report-all-keys: plain 'a' becomes `CSI 97u`.
        assert_eq!(kitty_encode_key(&ch("a"), Some("a"), none, false, ALL_ESC), Some(b"\x1b[97u".to_vec()));
        // Report-events + repeat: Ctrl+A repeat → `CSI 97;5:2u`.
        assert_eq!(
            kitty_encode_key(&ch("a"), Some("\x01"), ctrl, true, DISAMBIG | EVENTS),
            Some(b"\x1b[97;5:2u".to_vec())
        );
        // Report-text: plain 'a' under all-esc+text → `CSI 97;1;97u`.
        assert_eq!(
            kitty_encode_key(&ch("a"), Some("a"), none, false, ALL_ESC | TEXT),
            Some(b"\x1b[97;1;97u".to_vec())
        );
        // Report-text with a ctrl/alt/super combo must OMIT the text field
        // (those combos insert no text). Ctrl+A → `CSI 97;5u`, NOT
        // `...;5;1u`/`...;5;97u` — otherwise the app inserts a spurious char.
        assert_eq!(
            kitty_encode_key(&ch("a"), Some("\x01"), ctrl, false, DISAMBIG | TEXT),
            Some(b"\x1b[97;5u".to_vec())
        );
        assert_eq!(
            kitty_encode_key(&ch("a"), Some("a"), alt, false, DISAMBIG | TEXT),
            Some(b"\x1b[97;3u".to_vec())
        );
        // But shift alone DOES produce insertable text, so the field stays:
        // Shift+A under report-all+text → `CSI 97;2;65u`.
        assert_eq!(
            kitty_encode_key(&ch("A"), Some("A"), shift, false, ALL_ESC | TEXT),
            Some(b"\x1b[97;2;65u".to_vec())
        );
        // Functional keys fall through to legacy (None) — named_key_bytes
        // already emits the xterm modifier form the protocol reuses.
        assert_eq!(kitty_encode_key(&Key::Named(NamedKey::ArrowUp), None, ctrl, false, DISAMBIG), None);
    }

    #[test]
    fn alt_scroll_translates_wheel_to_cursor_keys() {
        // Wheel up (step > 0) → Up; down → Down. CSI form by default.
        assert_eq!(alt_scroll_bytes(1, false), b"\x1b[A".to_vec());
        assert_eq!(alt_scroll_bytes(-1, false), b"\x1b[B".to_vec());
        // App-cursor (DECCKM) → SS3 form.
        assert_eq!(alt_scroll_bytes(1, true), b"\x1bOA".to_vec());
        assert_eq!(alt_scroll_bytes(-1, true), b"\x1bOB".to_vec());
        // Magnitude repeats the key; zero yields nothing.
        assert_eq!(alt_scroll_bytes(3, false), b"\x1b[A\x1b[A\x1b[A".to_vec());
        assert!(alt_scroll_bytes(0, false).is_empty());
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

}
