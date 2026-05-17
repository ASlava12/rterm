//! System-clipboard write helper. The `paste` direction is handled
//! inline in `App::paste_clipboard` because it pulls bytes synchronously
//! and feeds them through the user-keybind / OSC pipeline — there's no
//! per-platform divergence worth a helper.
//!
//! The `set` direction does diverge: Linux X11 / Wayland ties
//! selection ownership to a live client connection, so the actual
//! `arboard::set()` call has to outlive the function. See the inline
//! doc on `clipboard_set` for the owner-thread + condvar design.

/// Write `text` to the system clipboard.
///
/// On macOS / Windows the OS stores the bytes directly, so a one-shot
/// `set_text` is enough.
///
/// On Linux (X11 / Wayland) the protocol ties selection ownership to a
/// live client connection: the process that called `set()` has to
/// remain reachable until another client requests the data. arboard's
/// `set().wait()` blocks the calling thread until that handover
/// happens. We need to call `wait()`, but spawning a *new* thread on
/// every Ctrl+Shift+C accumulates idle threads indefinitely if the
/// user copies a lot without any other application pasting (a real
/// failure mode reported during the audit: each thread sits idle
/// holding ~8 MB of virtual address space).
///
/// Hand the actual `wait()` to a single, lazily-started owner thread.
/// Callers write the latest text into a slot guarded by a `Mutex` +
/// `Condvar`; the worker takes the slot, runs `set().wait()`, and
/// loops back. Subsequent `clipboard_set` calls during a long-running
/// `wait()` overwrite the slot in place — the worker picks up the
/// freshest text once `wait()` returns. The slot is bounded to one
/// entry, so memory cannot grow.
#[cfg(target_os = "linux")]
pub(crate) fn clipboard_set(text: &str) {
    use arboard::SetExtLinux;
    use std::sync::{Condvar, Mutex, OnceLock};
    struct Slot {
        pending: Mutex<Option<String>>,
        cv: Condvar,
    }
    static SLOT: OnceLock<&'static Slot> = OnceLock::new();
    let slot = SLOT.get_or_init(|| {
        let s: &'static Slot = Box::leak(Box::new(Slot {
            pending: Mutex::new(None),
            cv: Condvar::new(),
        }));
        std::thread::spawn(move || loop {
            let text = {
                let mut g = s.pending.lock().unwrap_or_else(|e| e.into_inner());
                while g.is_none() {
                    g = s.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                g.take().unwrap_or_default()
            };
            let mut cb = match arboard::Clipboard::new() {
                Ok(cb) => cb,
                Err(e) => {
                    // We already took `text` out of the slot. Surface
                    // the failure — silently dropping the user's copy
                    // because the display server connection refused
                    // would otherwise look like the keystroke had no
                    // effect. Truncate the dropped payload to avoid
                    // logging a megabyte selection at warn level.
                    let preview_len = text.chars().take(80).count();
                    let preview: String = text.chars().take(preview_len).collect();
                    let elided = if text.chars().nth(80).is_some() { "…" } else { "" };
                    tracing::warn!(
                        error = %e,
                        dropped_bytes = text.len(),
                        preview = %format!("{preview}{elided}"),
                        "clipboard owner: Clipboard::new() failed; dropping queued copy",
                    );
                    continue;
                }
            };
            // wait() blocks until another client takes the selection.
            // While we block here, fresh `clipboard_set` calls keep
            // overwriting `pending` — the next loop iteration will
            // pick up the latest, not the queue of historical copies.
            if let Err(e) = cb.set().wait().text(text) {
                tracing::warn!("clipboard set failed: {e}");
            }
        });
        s
    });
    {
        let mut g = slot.pending.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(text.to_string());
    }
    slot.cv.notify_one();
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn clipboard_set(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        if let Err(e) = cb.set_text(text.to_string()) {
            tracing::warn!("clipboard set failed: {e}");
        }
    }
}
