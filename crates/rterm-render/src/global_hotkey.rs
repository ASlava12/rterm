//! Optional OS-level global hotkey for `toggle_guake`.
//!
//! Without this, `[guake]` keybinds only fire while the rterm window is
//! focused — half the point of a Guake-style drop-down (press a key
//! from anywhere, terminal appears) is lost. The platform surfaces:
//!
//! * Windows: `RegisterHotKey` + a worker thread running its own
//!   `GetMessage` pump. `WM_HOTKEY` arrives in the worker's queue,
//!   the worker forwards via `EventLoopProxy::send_event(...)`, the
//!   main thread runs `toggle_guake` on the resulting [`UserEvent`].
//! * Linux X11 / Wayland / macOS: not yet implemented. The function
//!   logs a `warn!` once and returns a no-op handle so the App stays
//!   functional with just the in-app keybind.
//!
//! Returns a `GlobalHotkeyHandle` whose `Drop` impl unregisters the
//! hotkey + signals the worker to exit. Holding the handle on `App`
//! keeps the worker alive for the lifetime of the GUI session.

use crate::keybind::parse_key_spec;
use crate::UserEvent;
use winit::event_loop::EventLoopProxy;
// `KeyMatch` / `NamedKey` / `ModifiersState` are only referenced from
// the cfg(windows) backend and from tests; gating the import keeps
// non-Windows release builds free of unused-import warnings without
// per-line `#[allow]`s.
#[cfg(any(windows, test))]
use crate::keybind::KeyMatch;
#[cfg(any(windows, test))]
use winit::keyboard::{ModifiersState, NamedKey};

/// Opaque RAII handle returned by [`install_global_hotkey`]. The inner
/// implementation differs per OS but the contract is the same:
/// dropping the handle unregisters the hotkey and joins the worker
/// thread (if any).
pub(crate) struct GlobalHotkeyHandle {
    #[cfg(windows)]
    inner: Option<windows_impl::WorkerHandle>,
    // Non-Windows targets carry no state — the constructor logs and
    // returns this empty handle.
    #[cfg(not(windows))]
    _stub: (),
}

impl GlobalHotkeyHandle {
    fn empty() -> Self {
        Self {
            #[cfg(windows)]
            inner: None,
            #[cfg(not(windows))]
            _stub: (),
        }
    }
}

/// Install the OS-level global hotkey. `spec` follows the same syntax
/// as `[[keybindings]].keys` ("F11", "Ctrl+Shift+`", "Super+Grave"...);
/// invalid specs log a `warn!` and return a no-op handle so the App
/// stays usable with the in-app binding alone.
pub(crate) fn install_global_hotkey(
    spec: &str,
    proxy: EventLoopProxy<UserEvent>,
) -> GlobalHotkeyHandle {
    let Some((mods, key)) = parse_key_spec(spec) else {
        tracing::warn!(
            spec = %spec,
            "[guake].global_hotkey: could not parse key spec — skipping",
        );
        return GlobalHotkeyHandle::empty();
    };
    #[cfg(windows)]
    {
        match windows_impl::register(mods, &key, proxy) {
            Ok(handle) => GlobalHotkeyHandle {
                inner: Some(handle),
            },
            Err(e) => {
                tracing::warn!(
                    spec = %spec,
                    error = %e,
                    "[guake].global_hotkey: Windows RegisterHotKey failed",
                );
                GlobalHotkeyHandle::empty()
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (mods, key, proxy);
        tracing::warn!(
            spec = %spec,
            "[guake].global_hotkey: not implemented on this platform yet; \
             the in-app binding still works while the window is focused",
        );
        GlobalHotkeyHandle::empty()
    }
}

/// Translate a [`NamedKey`] into the corresponding Win32 virtual-key
/// code (`VK_*`). Public to the module so the Windows worker and the
/// fallback `is_known_key` test can share one table. Returns `None`
/// when winit's NamedKey doesn't map to a Win32 VK we support.
///
/// Kept short — covers function keys, navigation, and the named
/// punctuation entries the keybind parser accepts. Adding a key
/// involves one line here.
#[cfg(any(windows, test))]
pub(crate) fn named_to_vk(key: &NamedKey) -> Option<u32> {
    // Windows virtual-key codes. Defined inline rather than pulling
    // the whole `winuser.h` bindings into the cross-platform tree —
    // the values are stable from Win32's earliest days.
    const VK_BACK: u32 = 0x08;
    const VK_TAB: u32 = 0x09;
    const VK_RETURN: u32 = 0x0D;
    const VK_ESCAPE: u32 = 0x1B;
    const VK_SPACE: u32 = 0x20;
    const VK_PRIOR: u32 = 0x21;
    const VK_NEXT: u32 = 0x22;
    const VK_END: u32 = 0x23;
    const VK_HOME: u32 = 0x24;
    const VK_LEFT: u32 = 0x25;
    const VK_UP: u32 = 0x26;
    const VK_RIGHT: u32 = 0x27;
    const VK_DOWN: u32 = 0x28;
    const VK_INSERT: u32 = 0x2D;
    const VK_DELETE: u32 = 0x2E;
    Some(match key {
        NamedKey::Backspace => VK_BACK,
        NamedKey::Tab => VK_TAB,
        NamedKey::Enter => VK_RETURN,
        NamedKey::Escape => VK_ESCAPE,
        NamedKey::Space => VK_SPACE,
        NamedKey::PageUp => VK_PRIOR,
        NamedKey::PageDown => VK_NEXT,
        NamedKey::End => VK_END,
        NamedKey::Home => VK_HOME,
        NamedKey::ArrowLeft => VK_LEFT,
        NamedKey::ArrowUp => VK_UP,
        NamedKey::ArrowRight => VK_RIGHT,
        NamedKey::ArrowDown => VK_DOWN,
        NamedKey::Insert => VK_INSERT,
        NamedKey::Delete => VK_DELETE,
        // VK_F1 = 0x70, VK_F2 = 0x71, …, VK_F24 = 0x87. Derived
        // arithmetically from the F1 anchor so a future extension to
        // F13+ only needs an extra match arm.
        NamedKey::F1 => 0x70,
        NamedKey::F2 => 0x71,
        NamedKey::F3 => 0x72,
        NamedKey::F4 => 0x73,
        NamedKey::F5 => 0x74,
        NamedKey::F6 => 0x75,
        NamedKey::F7 => 0x76,
        NamedKey::F8 => 0x77,
        NamedKey::F9 => 0x78,
        NamedKey::F10 => 0x79,
        NamedKey::F11 => 0x7A,
        NamedKey::F12 => 0x7B,
        _ => return None,
    })
}

/// Translate a one-character `KeyMatch::Char` into a Win32 VK code.
/// Letters and digits map straight to their ASCII upper-case byte (VK
/// codes for `A`..`Z` / `0`..`9` are literally those bytes). Other
/// single chars are not supported by `RegisterHotKey` (it wants a VK,
/// not a scan code), so we return None and let the caller log + skip.
#[cfg(any(windows, test))]
pub(crate) fn char_to_vk(c: &str) -> Option<u32> {
    let ch = c.chars().next().filter(|_| c.chars().count() == 1)?;
    let up = ch.to_ascii_uppercase();
    if up.is_ascii_alphanumeric() {
        Some(up as u32)
    } else {
        None
    }
}

#[cfg(any(windows, test))]
pub(crate) fn key_to_vk(key: &KeyMatch) -> Option<u32> {
    match key {
        KeyMatch::Named(n) => named_to_vk(n),
        KeyMatch::Char(s) => char_to_vk(s),
    }
}

/// Translate winit's [`ModifiersState`] bit set into the
/// `MOD_*` flags that `RegisterHotKey` expects. Returned as a
/// platform-agnostic `u32` so non-Windows callers (e.g. tests) can
/// reach the same mapping.
#[cfg(any(windows, test))]
pub(crate) fn mods_to_win32(mods: ModifiersState) -> u32 {
    // The constants are stable across every Windows SDK; defining
    // them inline keeps the function callable from cfg(test) on
    // non-Windows hosts.
    const MOD_ALT: u32 = 0x0001;
    const MOD_CONTROL: u32 = 0x0002;
    const MOD_SHIFT: u32 = 0x0004;
    const MOD_WIN: u32 = 0x0008;
    const MOD_NOREPEAT: u32 = 0x4000;
    let mut out = MOD_NOREPEAT;
    if mods.contains(ModifiersState::CONTROL) {
        out |= MOD_CONTROL;
    }
    if mods.contains(ModifiersState::SHIFT) {
        out |= MOD_SHIFT;
    }
    if mods.contains(ModifiersState::ALT) {
        out |= MOD_ALT;
    }
    if mods.contains(ModifiersState::SUPER) {
        out |= MOD_WIN;
    }
    out
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetMessageW, PostThreadMessageW, MSG, WM_HOTKEY, WM_QUIT,
    };

    /// RAII guard around the worker thread + hotkey registration.
    pub(crate) struct WorkerHandle {
        /// `JoinHandle` parked in an Option so `Drop` can take it.
        thread: Option<JoinHandle<()>>,
        /// Thread id of the worker — needed by `PostThreadMessageW`
        /// to push `WM_QUIT` into its message queue on shutdown.
        thread_id: u32,
        /// Latch the worker checks between messages. Drop flips it
        /// to true before sending WM_QUIT so a torn-down worker
        /// can't accidentally forward another hotkey press.
        stop: Arc<AtomicBool>,
    }

    impl Drop for WorkerHandle {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            // SAFETY: WM_QUIT is the documented mechanism for asking
            // `GetMessageW` to return zero, which terminates our
            // worker loop. The thread id was captured when the
            // worker started, so it's valid until the JoinHandle is
            // joined below.
            unsafe {
                PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
            }
            if let Some(handle) = self.thread.take() {
                let _ = handle.join();
            }
        }
    }

    /// Register the hotkey on a fresh worker thread and pump messages
    /// until `Drop` posts `WM_QUIT`.
    pub(crate) fn register(
        mods: ModifiersState,
        key: &KeyMatch,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<WorkerHandle, String> {
        let Some(vk) = key_to_vk(key) else {
            return Err("unsupported key for RegisterHotKey".into());
        };
        let fs_modifiers = mods_to_win32(mods);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);

        // Synchronise the parent until the worker reports back its
        // thread id (needed for the WM_QUIT post on shutdown) AND the
        // registration result (so we can surface failure as `Err`
        // without leaking a thread).
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<u32, String>>(1);

        let thread = std::thread::Builder::new()
            .name("rterm-global-hotkey".into())
            .spawn(move || {
                // Hotkey ID is a small int — any value works as long as
                // we use the same one for register and unregister. We
                // pick 1; there's a single hotkey per worker.
                const HOTKEY_ID: i32 = 1;
                // SAFETY: `RegisterHotKey(NULL, …)` makes WM_HOTKEY
                // post to the *calling thread's* message queue. The
                // thread id we report back to the parent identifies
                // exactly this queue.
                let tid = unsafe { thread_id() };
                let ok = unsafe {
                    RegisterHotKey(
                        std::ptr::null_mut(),
                        HOTKEY_ID,
                        fs_modifiers,
                        vk,
                    )
                };
                if ok == 0 {
                    let _ = tx.send(Err(format!(
                        "RegisterHotKey failed (last_error = {})",
                        unsafe { last_error() },
                    )));
                    return;
                }
                let _ = tx.send(Ok(tid));

                let mut msg: MSG = unsafe { std::mem::zeroed() };
                loop {
                    // SAFETY: standard message-pump idiom. `GetMessageW`
                    // returns 0 on WM_QUIT, -1 on error, or 1 with a
                    // valid message.
                    let r = unsafe {
                        GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0)
                    };
                    if r == 0 || r == -1 {
                        break;
                    }
                    if stop_for_thread.load(Ordering::Acquire) {
                        break;
                    }
                    if msg.message == WM_HOTKEY && msg.wParam as i32 == HOTKEY_ID {
                        // Best-effort send — if the EventLoop is gone
                        // (shutdown is racing the worker), drop the
                        // signal silently.
                        let _ = proxy.send_event(UserEvent::GuakeGlobalHotkey);
                    }
                }
                // SAFETY: same handle we registered against.
                unsafe {
                    UnregisterHotKey(std::ptr::null_mut(), HOTKEY_ID);
                }
            })
            .map_err(|e| format!("spawn worker thread: {e}"))?;

        let tid = rx
            .recv()
            .map_err(|e| format!("worker handshake closed: {e}"))?
            .map_err(|e| {
                // Reap the spawned thread that's now exiting on its own
                // so we don't leak the OS handle.
                let _ = thread;
                e
            })?;

        Ok(WorkerHandle {
            thread: Some(thread),
            thread_id: tid,
            stop,
        })
    }

    /// `GetCurrentThreadId()`. Defined as a thin wrapper so the call
    /// site stays self-documenting (the windows-sys-imported name is
    /// a noisy `pub use windows_sys::Win32::System::Threading::*`).
    unsafe fn thread_id() -> u32 {
        windows_sys::Win32::System::Threading::GetCurrentThreadId()
    }

    /// `GetLastError()`. Same self-doc rationale as `thread_id`.
    unsafe fn last_error() -> u32 {
        windows_sys::Win32::Foundation::GetLastError()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_to_vk_covers_function_keys() {
        // VK_F1..=VK_F12 are 0x70..=0x7B in Win32 — pin every entry
        // so a future edit to `named_to_vk` can't silently drop or
        // shift a function key, which is the most common
        // `[guake].global_hotkey` choice.
        let fn_keys = [
            (NamedKey::F1, 0x70),
            (NamedKey::F2, 0x71),
            (NamedKey::F3, 0x72),
            (NamedKey::F4, 0x73),
            (NamedKey::F5, 0x74),
            (NamedKey::F6, 0x75),
            (NamedKey::F7, 0x76),
            (NamedKey::F8, 0x77),
            (NamedKey::F9, 0x78),
            (NamedKey::F10, 0x79),
            (NamedKey::F11, 0x7A),
            (NamedKey::F12, 0x7B),
        ];
        for (key, expected) in fn_keys {
            assert_eq!(
                named_to_vk(&key),
                Some(expected),
                "wrong VK for {key:?}",
            );
        }
    }

    #[test]
    fn named_to_vk_covers_navigation_keys() {
        // Navigation cluster: Home, End, PgUp, PgDn, arrows, Ins/Del.
        // These are the second-most-common hotkey choices (e.g.
        // `Ctrl+Shift+Home`) and share the same drop / shift risk
        // as the function-key cluster.
        let nav_keys = [
            (NamedKey::PageUp, 0x21),
            (NamedKey::PageDown, 0x22),
            (NamedKey::End, 0x23),
            (NamedKey::Home, 0x24),
            (NamedKey::ArrowLeft, 0x25),
            (NamedKey::ArrowUp, 0x26),
            (NamedKey::ArrowRight, 0x27),
            (NamedKey::ArrowDown, 0x28),
            (NamedKey::Insert, 0x2D),
            (NamedKey::Delete, 0x2E),
        ];
        for (key, expected) in nav_keys {
            assert_eq!(
                named_to_vk(&key),
                Some(expected),
                "wrong VK for {key:?}",
            );
        }
    }

    #[test]
    fn char_to_vk_alphanumeric_only() {
        // Letters: 'a'..='z' all map to VK 0x41..=0x5A (their
        // uppercase ASCII byte). `RegisterHotKey` doesn't accept
        // non-letter / non-digit single chars (those need a scan-
        // code path); the parser rejects them here so the worker
        // can surface a clean error instead of a cryptic Win32 fail.
        assert_eq!(char_to_vk("a"), Some(b'A' as u32));
        assert_eq!(char_to_vk("Z"), Some(b'Z' as u32));
        assert_eq!(char_to_vk("0"), Some(b'0' as u32));
        assert_eq!(char_to_vk("9"), Some(b'9' as u32));
        // Punctuation / unsupported.
        assert_eq!(char_to_vk("`"), None);
        assert_eq!(char_to_vk("+"), None);
        // Multi-char inputs (`ab`, `tab` etc.) get rejected too.
        assert_eq!(char_to_vk("ab"), None);
        assert_eq!(char_to_vk(""), None);
    }

    #[test]
    fn mods_to_win32_sets_noreapeat_and_each_flag() {
        // The mapping is platform-agnostic enough to test on any
        // host. MOD_NOREPEAT (0x4000) is always set so a held-down
        // hotkey doesn't fire a flood of events.
        let none = mods_to_win32(ModifiersState::empty());
        assert_eq!(none, 0x4000);
        let ctrl = mods_to_win32(ModifiersState::CONTROL);
        assert_eq!(ctrl, 0x4000 | 0x0002);
        let ctrl_shift =
            mods_to_win32(ModifiersState::CONTROL | ModifiersState::SHIFT);
        assert_eq!(ctrl_shift, 0x4000 | 0x0002 | 0x0004);
        let all = mods_to_win32(
            ModifiersState::CONTROL
                | ModifiersState::SHIFT
                | ModifiersState::ALT
                | ModifiersState::SUPER,
        );
        assert_eq!(all, 0x4000 | 0x0001 | 0x0002 | 0x0004 | 0x0008);
    }

    #[test]
    fn key_to_vk_round_trips_through_parse_key_spec() {
        // Wire-level: a user typing `"F11"` in `config.toml` must
        // resolve to VK_F11 (0x7A) through the same path the worker
        // uses. Catches a regression where either `parse_key_spec`
        // shifts F-key indices (already tested) OR `key_to_vk`
        // drops an entry from the lookup table.
        let (mods, key) = parse_key_spec("F11").expect("parses");
        assert!(mods.is_empty());
        assert_eq!(key_to_vk(&key), Some(0x7A));
        // `Ctrl+Shift+`+G` — full path through both branches.
        let (mods, key) = parse_key_spec("Ctrl+Shift+G").expect("parses");
        assert!(mods.contains(ModifiersState::CONTROL));
        assert!(mods.contains(ModifiersState::SHIFT));
        assert_eq!(key_to_vk(&key), Some(b'G' as u32));
    }
}
