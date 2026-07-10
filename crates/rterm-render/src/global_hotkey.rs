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
//! * macOS: Carbon `RegisterEventHotKey` + an `InstallEventHandler`
//!   on `GetEventDispatcherTarget()`. Carbon delivers `kEventHotKeyPressed`
//!   on the main run loop that winit already pumps, so no worker thread
//!   is needed — the handler forwards via `EventLoopProxy::send_event(...)`.
//!   (These few Carbon Event Manager APIs remain available in 64-bit /
//!   Apple Silicon.)
//! * Linux X11 / Wayland: not yet implemented. The function logs a
//!   `warn!` once and returns a no-op handle so the App stays functional
//!   with just the in-app keybind.
//!
//! Returns a `GlobalHotkeyHandle` whose `Drop` impl unregisters the
//! hotkey + signals the worker to exit. Holding the handle on `App`
//! keeps the worker alive for the lifetime of the GUI session.

use crate::keybind::parse_key_spec;
use crate::UserEvent;
use winit::event_loop::EventLoopProxy;
// `KeyMatch` / `NamedKey` / `ModifiersState` are only referenced from
// the cfg(windows) / cfg(macos) backends and from tests; gating the
// import keeps other release builds (e.g. Linux) free of unused-import
// warnings without per-line `#[allow]`s.
#[cfg(any(windows, target_os = "macos", test))]
use crate::keybind::KeyMatch;
#[cfg(any(windows, target_os = "macos", test))]
use winit::keyboard::{ModifiersState, NamedKey};

/// Opaque RAII handle returned by [`install_global_hotkey`]. The inner
/// implementation differs per OS but the contract is the same:
/// dropping the handle unregisters the hotkey and joins the worker
/// thread (if any).
pub(crate) struct GlobalHotkeyHandle {
    // Held purely for its `Drop` side effect (unregisters the hotkey
    // + joins the worker thread); the field is never read. The
    // `dead_code` allow is the standard way to silence the linter
    // for RAII guards.
    #[cfg(windows)]
    #[allow(dead_code)]
    inner: Option<windows_impl::WorkerHandle>,
    // macOS: RAII guard that unregisters the hotkey + removes the
    // Carbon event handler on drop. Held purely for that side effect.
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    inner: Option<macos_impl::MacHandle>,
    // Other targets carry no state — the constructor logs and
    // returns this empty handle.
    #[cfg(not(any(windows, target_os = "macos")))]
    _stub: (),
}

impl GlobalHotkeyHandle {
    fn empty() -> Self {
        Self {
            #[cfg(windows)]
            inner: None,
            #[cfg(target_os = "macos")]
            inner: None,
            #[cfg(not(any(windows, target_os = "macos")))]
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
    #[cfg(target_os = "macos")]
    {
        match macos_impl::register(mods, &key, proxy) {
            Ok(handle) => GlobalHotkeyHandle {
                inner: Some(handle),
            },
            Err(e) => {
                tracing::warn!(
                    spec = %spec,
                    error = %e,
                    "[guake].global_hotkey: macOS RegisterEventHotKey failed",
                );
                GlobalHotkeyHandle::empty()
            }
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
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

// --- macOS key mapping ------------------------------------------------
//
// Carbon `RegisterEventHotKey` wants a *virtual key code* (`kVK_*`),
// which — unlike Win32 — is NOT the ASCII byte of the letter. The codes
// are the fixed ANSI-keyboard positions from HIToolbox `Events.h`, so a
// lookup table is unavoidable. Gated `any(macos, test)` so the pure
// mapping is unit-tested on every host (mirrors the Win32 helpers).

/// Translate a [`NamedKey`] into its macOS virtual key code. Returns
/// `None` for keys Carbon can't bind (macOS keyboards have no Insert,
/// for instance) — the caller logs + skips.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn named_to_macos_vk(key: &NamedKey) -> Option<u32> {
    Some(match key {
        NamedKey::Backspace => 0x33, // kVK_Delete (the ⌫ key)
        NamedKey::Tab => 0x30,
        NamedKey::Enter => 0x24,
        NamedKey::Escape => 0x35,
        NamedKey::Space => 0x31,
        NamedKey::PageUp => 0x74,
        NamedKey::PageDown => 0x79,
        NamedKey::End => 0x77,
        NamedKey::Home => 0x73,
        NamedKey::ArrowLeft => 0x7B,
        NamedKey::ArrowUp => 0x7E,
        NamedKey::ArrowRight => 0x7C,
        NamedKey::ArrowDown => 0x7D,
        // macOS has no Insert key; map Delete to the forward-delete
        // (`⌦`) position so `Delete` still resolves to something real.
        NamedKey::Delete => 0x75, // kVK_ForwardDelete
        // kVK_F1..=kVK_F12 — deliberately non-contiguous in Carbon.
        NamedKey::F1 => 0x7A,
        NamedKey::F2 => 0x78,
        NamedKey::F3 => 0x63,
        NamedKey::F4 => 0x76,
        NamedKey::F5 => 0x60,
        NamedKey::F6 => 0x61,
        NamedKey::F7 => 0x62,
        NamedKey::F8 => 0x64,
        NamedKey::F9 => 0x65,
        NamedKey::F10 => 0x6D,
        NamedKey::F11 => 0x67,
        NamedKey::F12 => 0x6F,
        _ => return None,
    })
}

/// Translate a single-character `KeyMatch::Char` into a macOS virtual
/// key code. Letters/digits AND the common punctuation keys are covered
/// — notably backtick (`kVK_ANSI_Grave`), the classic Guake drop-down
/// key (`Super+\``). Case-insensitive; multi-char strings return `None`.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn char_to_macos_vk(c: &str) -> Option<u32> {
    let ch = c.chars().next().filter(|_| c.chars().count() == 1)?;
    Some(match ch.to_ascii_lowercase() {
        'a' => 0x00, 'b' => 0x0B, 'c' => 0x08, 'd' => 0x02, 'e' => 0x0E,
        'f' => 0x03, 'g' => 0x05, 'h' => 0x04, 'i' => 0x22, 'j' => 0x26,
        'k' => 0x28, 'l' => 0x25, 'm' => 0x2E, 'n' => 0x2D, 'o' => 0x1F,
        'p' => 0x23, 'q' => 0x0C, 'r' => 0x0F, 's' => 0x01, 't' => 0x11,
        'u' => 0x20, 'v' => 0x09, 'w' => 0x0D, 'x' => 0x07, 'y' => 0x10,
        'z' => 0x06,
        '0' => 0x1D, '1' => 0x12, '2' => 0x13, '3' => 0x14, '4' => 0x15,
        '5' => 0x17, '6' => 0x16, '7' => 0x1A, '8' => 0x1C, '9' => 0x19,
        '`' => 0x32, '-' => 0x1B, '=' => 0x18, '[' => 0x21, ']' => 0x1E,
        '\\' => 0x2A, ';' => 0x29, '\'' => 0x27, ',' => 0x2B, '.' => 0x2F,
        '/' => 0x2C,
        _ => return None,
    })
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn key_to_macos_vk(key: &KeyMatch) -> Option<u32> {
    match key {
        KeyMatch::Named(n) => named_to_macos_vk(n),
        KeyMatch::Char(s) => char_to_macos_vk(s),
    }
}

/// Translate winit's [`ModifiersState`] into the Carbon modifier mask
/// (`cmdKey`/`shiftKey`/`optionKey`/`controlKey` from `Events.h`) that
/// `RegisterEventHotKey` expects. Platform-agnostic `u32` so tests can
/// reach it on any host.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn mods_to_carbon(mods: ModifiersState) -> u32 {
    const CMD_KEY: u32 = 0x0100;
    const SHIFT_KEY: u32 = 0x0200;
    const OPTION_KEY: u32 = 0x0800;
    const CONTROL_KEY: u32 = 0x1000;
    let mut out = 0;
    if mods.contains(ModifiersState::CONTROL) {
        out |= CONTROL_KEY;
    }
    if mods.contains(ModifiersState::SHIFT) {
        out |= SHIFT_KEY;
    }
    if mods.contains(ModifiersState::ALT) {
        out |= OPTION_KEY;
    }
    if mods.contains(ModifiersState::SUPER) {
        out |= CMD_KEY;
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
            .inspect_err(|_| {
                // Reap the spawned thread that's now exiting on its own
                // so we don't leak the OS handle.
                let _ = thread;
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

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::*;
    use std::os::raw::c_void;

    // --- Carbon Event Manager FFI ------------------------------------
    //
    // Only the handful of symbols needed for a single global hotkey.
    // Declared by hand (rather than pulling a `core-foundation`/`carbon`
    // crate) to stay consistent with the hand-rolled `windows_impl`
    // above and add no dependency. All of these survive in 64-bit
    // Carbon (HIToolbox) on both Intel and Apple Silicon.

    type OsStatus = i32; // OSStatus = SInt32
    type EventTargetRef = *mut c_void;
    type EventHotKeyRef = *mut c_void;
    type EventHandlerRef = *mut c_void;
    type EventHandlerCallRef = *mut c_void;
    type EventRef = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EventHotKeyId {
        signature: u32, // OSType (FourCharCode)
        id: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct EventTypeSpec {
        event_class: u32, // OSType
        event_kind: u32,  // UInt32
    }

    // Modern macOS accepts a plain C function pointer as an
    // `EventHandlerUPP`; NUL-pointer optimisation makes this ABI-
    // identical to the `Option<fn>` some bindings use.
    type EventHandlerProc = unsafe extern "C" fn(
        call_ref: EventHandlerCallRef,
        event: EventRef,
        user_data: *mut c_void,
    ) -> OsStatus;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn GetEventDispatcherTarget() -> EventTargetRef;
        fn InstallEventHandler(
            target: EventTargetRef,
            handler: EventHandlerProc,
            num_types: usize, // ItemCount (unsigned long, LP64 == usize)
            list: *const EventTypeSpec,
            user_data: *mut c_void,
            out_ref: *mut EventHandlerRef,
        ) -> OsStatus;
        fn RemoveEventHandler(handler: EventHandlerRef) -> OsStatus;
        fn RegisterEventHotKey(
            key_code: u32,
            modifiers: u32,
            hot_key_id: EventHotKeyId,
            target: EventTargetRef,
            options: u32, // OptionBits
            out_ref: *mut EventHotKeyRef,
        ) -> OsStatus;
        fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OsStatus;
    }

    // FourCC 'keyb' = kEventClassKeyboard.
    const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
    const K_EVENT_HOTKEY_PRESSED: u32 = 5; // kEventHotKeyPressed
    // FourCC 'rtrm' — our app signature for the hotkey id. Value is
    // arbitrary; it only has to be stable between register/unregister.
    const HOTKEY_SIGNATURE: u32 = u32::from_be_bytes(*b"rtrm");

    /// RAII guard: unregisters the hotkey, removes the Carbon handler,
    /// and frees the boxed proxy on drop. Field order does not matter —
    /// `Drop` does it explicitly in the safe order.
    pub(crate) struct MacHandle {
        hotkey: EventHotKeyRef,
        handler: EventHandlerRef,
        // Boxed proxy handed to the C callback as `userData`; owned here
        // and freed on drop *after* the handler is removed so the
        // callback can never observe a dangling pointer.
        proxy: *mut EventLoopProxy<UserEvent>,
    }

    // The raw pointers are only ever touched on the main thread (install
    // + drop both run there), but `MacHandle` is stored on `App`, which
    // winit does not require to be `Send`. No cross-thread use.

    impl Drop for MacHandle {
        fn drop(&mut self) {
            // SAFETY: each handle/ref was produced by the matching
            // Carbon call in `register`; removing the handler first
            // guarantees the callback won't fire again, after which the
            // boxed proxy is safe to reclaim.
            unsafe {
                if !self.hotkey.is_null() {
                    UnregisterEventHotKey(self.hotkey);
                }
                if !self.handler.is_null() {
                    RemoveEventHandler(self.handler);
                }
                if !self.proxy.is_null() {
                    drop(Box::from_raw(self.proxy));
                }
            }
        }
    }

    /// Carbon event handler. We register exactly one hotkey with one
    /// `kEventHotKeyPressed` spec, so any invocation *is* our hotkey —
    /// no need to read the event's `EventHotKeyID` back out.
    unsafe extern "C" fn hotkey_handler(
        _call_ref: EventHandlerCallRef,
        _event: EventRef,
        user_data: *mut c_void,
    ) -> OsStatus {
        if !user_data.is_null() {
            // SAFETY: `user_data` is the `Box<EventLoopProxy>` pointer
            // passed to `InstallEventHandler`; it outlives the handler
            // (freed only after `RemoveEventHandler` in `MacHandle::drop`).
            let proxy = &*(user_data as *const EventLoopProxy<UserEvent>);
            // Best-effort: if the loop is gone (shutdown race), ignore.
            let _ = proxy.send_event(UserEvent::GuakeGlobalHotkey);
        }
        0 // noErr — event handled.
    }

    /// Register the hotkey + install the handler on the main thread's
    /// Carbon dispatcher. Returns an RAII handle; `Err` on any failure
    /// (unsupported key, Carbon rejected the registration).
    pub(crate) fn register(
        mods: ModifiersState,
        key: &KeyMatch,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<MacHandle, String> {
        let Some(code) = key_to_macos_vk(key) else {
            return Err("unsupported key for RegisterEventHotKey".into());
        };
        let carbon_mods = mods_to_carbon(mods);

        // Box the proxy so the C callback can reach it; the handle owns
        // the box and frees it on drop.
        let boxed = Box::into_raw(Box::new(proxy));
        let spec = EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOTKEY_PRESSED,
        };

        // SAFETY: standard Carbon Event Manager sequence. Called on the
        // main thread (see `run()` in lib.rs — before `run_app`). Every
        // early return frees the box so it can't leak.
        unsafe {
            let target = GetEventDispatcherTarget();
            if target.is_null() {
                drop(Box::from_raw(boxed));
                return Err("GetEventDispatcherTarget returned null".into());
            }
            let mut handler_ref: EventHandlerRef = std::ptr::null_mut();
            let st = InstallEventHandler(
                target,
                hotkey_handler,
                1,
                &spec,
                boxed as *mut c_void,
                &mut handler_ref,
            );
            if st != 0 {
                drop(Box::from_raw(boxed));
                return Err(format!("InstallEventHandler failed: OSStatus {st}"));
            }
            let hotkey_id = EventHotKeyId {
                signature: HOTKEY_SIGNATURE,
                id: 1,
            };
            let mut hotkey_ref: EventHotKeyRef = std::ptr::null_mut();
            let st = RegisterEventHotKey(
                code,
                carbon_mods,
                hotkey_id,
                target,
                0,
                &mut hotkey_ref,
            );
            if st != 0 {
                RemoveEventHandler(handler_ref);
                drop(Box::from_raw(boxed));
                return Err(format!("RegisterEventHotKey failed: OSStatus {st}"));
            }
            Ok(MacHandle {
                hotkey: hotkey_ref,
                handler: handler_ref,
                proxy: boxed,
            })
        }
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

    #[test]
    fn macos_named_vk_covers_function_and_nav_keys() {
        // The Carbon kVK_F* codes are non-contiguous — pin every entry
        // so a future edit can't silently shift a function key (the most
        // common `[guake].global_hotkey` choice on macOS).
        let fn_keys = [
            (NamedKey::F1, 0x7A),
            (NamedKey::F2, 0x78),
            (NamedKey::F3, 0x63),
            (NamedKey::F4, 0x76),
            (NamedKey::F5, 0x60),
            (NamedKey::F6, 0x61),
            (NamedKey::F7, 0x62),
            (NamedKey::F8, 0x64),
            (NamedKey::F9, 0x65),
            (NamedKey::F10, 0x6D),
            (NamedKey::F11, 0x67),
            (NamedKey::F12, 0x6F),
        ];
        for (key, expected) in fn_keys {
            assert_eq!(named_to_macos_vk(&key), Some(expected), "F-key {key:?}");
        }
        // Navigation cluster.
        assert_eq!(named_to_macos_vk(&NamedKey::Home), Some(0x73));
        assert_eq!(named_to_macos_vk(&NamedKey::End), Some(0x77));
        assert_eq!(named_to_macos_vk(&NamedKey::ArrowUp), Some(0x7E));
        assert_eq!(named_to_macos_vk(&NamedKey::Space), Some(0x31));
        // macOS keyboards have no Insert key — Carbon can't bind it.
        assert_eq!(named_to_macos_vk(&NamedKey::Insert), None);
    }

    #[test]
    fn macos_char_vk_is_case_insensitive_and_covers_grave() {
        // Letters map to fixed ANSI positions, NOT their ASCII byte.
        assert_eq!(char_to_macos_vk("a"), Some(0x00));
        assert_eq!(char_to_macos_vk("A"), Some(0x00)); // case-insensitive
        assert_eq!(char_to_macos_vk("z"), Some(0x06));
        assert_eq!(char_to_macos_vk("0"), Some(0x1D));
        assert_eq!(char_to_macos_vk("9"), Some(0x19));
        // Backtick = kVK_ANSI_Grave — the classic Guake drop-down key.
        assert_eq!(char_to_macos_vk("`"), Some(0x32));
        // Unlike Win32's RegisterHotKey, punctuation resolves here.
        assert_eq!(char_to_macos_vk("/"), Some(0x2C));
        // Multi-char / empty are rejected.
        assert_eq!(char_to_macos_vk("ab"), None);
        assert_eq!(char_to_macos_vk(""), None);
    }

    #[test]
    fn mods_to_carbon_maps_each_flag() {
        // Carbon masks from Events.h; no NOREPEAT equivalent (Carbon
        // hotkeys don't auto-repeat), so empty mods => 0.
        assert_eq!(mods_to_carbon(ModifiersState::empty()), 0);
        assert_eq!(mods_to_carbon(ModifiersState::SUPER), 0x0100);
        assert_eq!(mods_to_carbon(ModifiersState::SHIFT), 0x0200);
        assert_eq!(mods_to_carbon(ModifiersState::ALT), 0x0800);
        assert_eq!(mods_to_carbon(ModifiersState::CONTROL), 0x1000);
        let all = mods_to_carbon(
            ModifiersState::CONTROL
                | ModifiersState::SHIFT
                | ModifiersState::ALT
                | ModifiersState::SUPER,
        );
        assert_eq!(all, 0x1000 | 0x0800 | 0x0200 | 0x0100);
    }

    #[test]
    fn key_to_macos_vk_round_trips_through_parse_key_spec() {
        // Wire-level: `"F11"` in config.toml must reach kVK_F11 (0x67)
        // through the same path the macOS backend uses.
        let (mods, key) = parse_key_spec("F11").expect("parses");
        assert!(mods.is_empty());
        assert_eq!(key_to_macos_vk(&key), Some(0x67));
        // `Super+\`` — the canonical Guake binding — resolves to Cmd +
        // kVK_ANSI_Grave.
        let (mods, key) = parse_key_spec("Super+`").expect("parses");
        assert!(mods.contains(ModifiersState::SUPER));
        assert_eq!(mods_to_carbon(mods), 0x0100);
        assert_eq!(key_to_macos_vk(&key), Some(0x32));
    }
}
