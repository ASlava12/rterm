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

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState};

use crate::{ctrl_byte, named_key_bytes, paste_confirm, App};

impl App {
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
}
