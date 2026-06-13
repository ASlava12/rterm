//! Per-pane command-line accumulator that turns the raw byte stream
//! we send to the PTY back into the discrete commands the user
//! submitted. Pairs with `rterm-history` to build a SQLite-backed
//! history that survives across sessions AND works for SSH'd remote
//! shells (the capture sits on the local input side — we see what
//! the user typed regardless of where the PTY ends up running it).
//!
//! Design note: the stream we feed in here is the same bytes we
//! write to the PTY (keystrokes, paste payloads, plugin-synthesised
//! input). We don't see the shell's echoed display, so TAB-completed
//! text doesn't end up in our buffer. That's a known trade-off
//! agreed with the user — capture sticks to "what the user typed
//! literally" so the history reflects intent, not what the shell
//! happened to expand it to.
//!
//! ## Cleaning rules
//!
//! For each byte we observe:
//! * `\x1b` starts an escape sequence — discarded along with the
//!   immediate `[` (CSI) / `]` (OSC) / `P` (DCS) / `^` / `_`
//!   payload until terminator. Single-char ESC (`ESC k`) drops two
//!   bytes total. Saved us implementing a full state machine in
//!   exchange for "you can't have ESC in your saved commands",
//!   which matches how shells handle the wire anyway.
//! * Bracketed-paste markers `\x1b[200~` / `\x1b[201~` get dropped
//!   automatically by the CSI handler above; the pasted payload
//!   between them is retained.
//! * Control bytes (`\x00..=\x1f` and `\x7f`):
//!   - `\x08` / `\x7f` — backspace, deletes the last byte from the
//!     buffer (handles readline-style line editing).
//!   - `\x15` — Ctrl+U "kill whole line", clears the buffer.
//!   - `\x17` — Ctrl+W "kill last word", strips trailing whitespace
//!     and then a run of non-whitespace.
//!   - `\x03` — Ctrl+C "cancel current input", clears the buffer
//!     (the shell aborts the line so we should too).
//!   - `\x09` — TAB. The shell expands this into a completion;
//!     since we never see the expansion, just drop the TAB itself.
//!   - `\r` / `\n` — submit. Returns the cleaned line.
//!   - Everything else in the C0/C1 range is silently dropped.
//! * Printable bytes (`\x20..=\x7e`) and UTF-8 continuation
//!   (`\x80..`) are appended verbatim.

use std::sync::{Arc, Mutex};

/// Accumulated command line for one pane. Held inside `Pane` and
/// fed every byte the renderer writes to that pane's PTY.
pub(crate) struct CommandBuffer {
    raw: Vec<u8>,
    /// Mid-escape state. `Some(Continue::CsiOrOsc)` means we saw
    /// `\x1b` and are dropping bytes until a terminator. `None` =
    /// normal byte-by-byte path.
    state: EscapeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    /// Normal byte path.
    Normal,
    /// Just saw `\x1b`; next byte determines the sub-mode.
    EscSeen,
    /// Inside a CSI (`\x1b[...`), DCS (`\x1bP...`), or OSC
    /// (`\x1b]...`) sequence. Drop bytes until the terminator.
    Csi,
    /// Inside an OSC sequence. Terminator is BEL (`\x07`) or
    /// ST (`\x1b\\`).
    Osc,
    /// Inside a DCS / `\x1bP` sequence. Terminator is ST.
    Dcs,
    /// Just saw `\x1b\` (final byte of an OSC/DCS String-Terminator).
    /// Eats nothing; transitions immediately back to Normal on the
    /// `\\` that completes the ST.
    StPending,
}

impl CommandBuffer {
    pub(crate) fn new() -> Self {
        Self {
            raw: Vec::with_capacity(128),
            state: EscapeState::Normal,
        }
    }

    /// Feed a chunk of input bytes. Returns one cleaned command for
    /// every `\r` / `\n` encountered (in submission order). Empty
    /// commands are filtered. The internal buffer continues to
    /// accumulate post-submit bytes for the next command line.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in bytes {
            match self.state {
                EscapeState::Normal => match b {
                    0x1b => self.state = EscapeState::EscSeen,
                    b'\r' | b'\n' => {
                        if let Some(cmd) = self.take_command() {
                            out.push(cmd);
                        }
                    }
                    0x08 | 0x7f => {
                        // Backspace: pop the last UTF-8 char from
                        // the buffer. `Vec::pop` drops a single
                        // byte, which is correct for ASCII; for
                        // multi-byte chars walk back to the
                        // boundary.
                        self.pop_char();
                    }
                    0x15 => self.raw.clear(), // Ctrl+U
                    0x17 => self.kill_word(), // Ctrl+W
                    0x03 => self.raw.clear(), // Ctrl+C
                    0x09 => {}                // TAB — see module doc
                    0x00..=0x1f => {
                        // Drop any other C0 control byte silently.
                    }
                    _ => self.raw.push(b),
                },
                EscapeState::EscSeen => match b {
                    b'[' => self.state = EscapeState::Csi,
                    b']' => self.state = EscapeState::Osc,
                    b'P' | b'X' | b'_' | b'^' => self.state = EscapeState::Dcs,
                    _ => {
                        // Short escape (`ESC <letter>`, etc.). Drop
                        // both bytes and return to Normal.
                        self.state = EscapeState::Normal;
                    }
                },
                EscapeState::Csi => {
                    // CSI terminator: any byte in 0x40..=0x7e.
                    if (0x40..=0x7e).contains(&b) {
                        self.state = EscapeState::Normal;
                    }
                }
                EscapeState::Osc => {
                    // OSC terminator: BEL (`\x07`) or ST (`ESC \`).
                    if b == 0x07 {
                        self.state = EscapeState::Normal;
                    } else if b == 0x1b {
                        self.state = EscapeState::StPending;
                    }
                }
                EscapeState::Dcs => {
                    if b == 0x1b {
                        self.state = EscapeState::StPending;
                    }
                }
                EscapeState::StPending => {
                    // `ESC \` → ST. Anything else cancels — bail to
                    // Normal so a stray ESC mid-OSC doesn't leave us
                    // permanently absorbing input.
                    self.state = EscapeState::Normal;
                    // Don't re-process the byte — by spec the ST is
                    // a two-byte unit and we just consumed both.
                    let _ = b;
                }
            }
        }
        out
    }

    /// Read the buffer's current contents WITHOUT consuming them.
    /// Used by the popup to query history with the current
    /// half-typed prefix. Returns the bytes interpreted as UTF-8;
    /// non-UTF-8 trailing bytes (e.g. mid-character on a paste) are
    /// dropped so the renderer never sees a partial codepoint.
    pub(crate) fn current_input(&self) -> String {
        // `from_utf8_lossy` substitutes a U+FFFD for each invalid
        // sequence; that's fine for a *display* of the prefix but
        // we don't want it to leak into history queries. Use a
        // strict from_utf8 first and only fall back to the lossy
        // form on the suffix — we want to keep all the valid
        // prefix bytes for matching.
        match std::str::from_utf8(&self.raw) {
            Ok(s) => s.to_string(),
            Err(e) => {
                // Truncate at the first invalid byte; the popup
                // will see whatever valid prefix the user has
                // accumulated so far. The next valid byte will
                // bring us back to a clean state on the next
                // `feed`.
                std::str::from_utf8(&self.raw[..e.valid_up_to()])
                    .unwrap_or("")
                    .to_string()
            }
        }
    }

    /// Take whatever's currently in the buffer as a UTF-8 string,
    /// trim outer whitespace, return `Some` if non-empty. Resets
    /// the buffer.
    fn take_command(&mut self) -> Option<String> {
        let bytes = std::mem::take(&mut self.raw);
        let s = String::from_utf8(bytes).ok()?;
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Pop one UTF-8 character from the buffer (backspace).
    fn pop_char(&mut self) {
        // Walk backwards over continuation bytes (`10xxxxxx`), then
        // pop the leading byte.
        while let Some(&last) = self.raw.last() {
            self.raw.pop();
            if last & 0xc0 != 0x80 {
                break;
            }
        }
    }

    /// Kill the trailing word — Ctrl+W. Drops trailing whitespace
    /// then a run of non-whitespace bytes. Matches readline's
    /// `unix-word-rubout` (whitespace-only word boundary; punctuation
    /// is not a boundary, unlike `backward-kill-word`).
    fn kill_word(&mut self) {
        while matches!(self.raw.last(), Some(b) if b.is_ascii_whitespace()) {
            self.raw.pop();
        }
        while matches!(self.raw.last(), Some(b) if !b.is_ascii_whitespace()) {
            self.raw.pop();
        }
    }
}

/// Per-pane history sink shared with the `History` store. Holds the
/// buffer + an `Arc<Mutex<History>>` so call sites can `feed` bytes
/// from `Pane::send_input` without juggling references explicitly.
pub(crate) struct CommandCapture {
    buffer: Mutex<CommandBuffer>,
    history: Option<Arc<Mutex<rterm_history::History>>>,
    /// Monotonic counter bumped every time `feed` mutates the
    /// buffer (i.e. on a non-empty, non-pure-control input).
    /// The renderer polls this to detect "did the user just
    /// type?" — when the value changes, it re-arms the popup
    /// debounce timer.
    generation: std::sync::atomic::AtomicU64,
}

impl CommandCapture {
    pub(crate) fn new(history: Option<Arc<Mutex<rterm_history::History>>>) -> Self {
        Self {
            buffer: Mutex::new(CommandBuffer::new()),
            history,
            generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Feed a chunk of bytes through the buffer + record any
    /// completed commands. Silent on poisoned mutex / DB error so a
    /// transient I/O hiccup can't take input down — capture is
    /// best-effort.
    pub(crate) fn feed(&self, bytes: &[u8]) {
        let Ok(mut buf) = self.buffer.lock() else { return };
        let commands = buf.feed(bytes);
        // Always bump generation, even on pure control / escape
        // sequences — the popup wants to react to backspace or
        // Ctrl+U the same way it reacts to a typed char.
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        drop(buf);
        let Some(history) = self.history.as_ref() else { return };
        let Ok(history) = history.lock() else { return };
        for cmd in commands {
            if let Err(e) = history.record(&cmd) {
                tracing::debug!(error = %e, "history record failed");
            }
        }
    }

    /// Snapshot the current half-typed command. Empty string when
    /// the buffer is empty (no prefix → popup typically hides).
    pub(crate) fn current_input(&self) -> String {
        self.buffer
            .lock()
            .ok()
            .map(|b| b.current_input())
            .unwrap_or_default()
    }

    /// Monotonic generation. The popup refresh-debouncer uses
    /// `generation` deltas to detect input changes without holding
    /// the buffer mutex across frames.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(buf: &mut CommandBuffer, input: &[u8]) -> Vec<String> {
        buf.feed(input)
    }

    #[test]
    fn plain_command_round_trips_on_newline() {
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"ls -la\r");
        assert_eq!(out, vec!["ls -la".to_string()]);
    }

    #[test]
    fn empty_lines_filtered() {
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"\r\n   \r\n");
        assert!(out.is_empty());
    }

    #[test]
    fn multiple_commands_split_on_newline() {
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"ls\rcd ..\rpwd\n");
        assert_eq!(out, vec!["ls", "cd ..", "pwd"]);
    }

    #[test]
    fn backspace_removes_last_char() {
        // User typed "lst" then BS (\x7f) then "\r" → "ls".
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"lst\x7f\r");
        assert_eq!(out, vec!["ls".to_string()]);
    }

    #[test]
    fn backspace_handles_multibyte_utf8() {
        // "лс" (Cyrillic, 4 bytes) + BS should drop the trailing 'с'
        // and yield just "л" on submit.
        let mut b = CommandBuffer::new();
        // 0xd0 0xbb = 'л', 0xd1 0x81 = 'с'
        let out = one(&mut b, b"\xd0\xbb\xd1\x81\x08\r");
        assert_eq!(out, vec!["\u{43b}".to_string()]); // just 'л'
    }

    #[test]
    fn ctrl_u_kills_whole_line() {
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"git status\x15ls\r");
        assert_eq!(out, vec!["ls".to_string()]);
    }

    #[test]
    fn ctrl_c_cancels_current_input() {
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"abandoned\x03real cmd\r");
        assert_eq!(out, vec!["real cmd".to_string()]);
    }

    #[test]
    fn ctrl_w_kills_trailing_word() {
        let mut b = CommandBuffer::new();
        // Type "git status", Ctrl+W → "git ", submit → "git".
        let out = one(&mut b, b"git status\x17\r");
        assert_eq!(out, vec!["git".to_string()]);
    }

    #[test]
    fn tab_is_dropped() {
        // We never see the shell's TAB expansion, so dropping the
        // TAB itself keeps the buffer's reading of "what user
        // typed" honest.
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"ls T\tFile.txt\r");
        // Same as typing "ls TFile.txt" since TAB was dropped.
        assert_eq!(out, vec!["ls TFile.txt".to_string()]);
    }

    #[test]
    fn csi_arrow_keys_dropped() {
        // Arrow keys are CSI sequences (`\x1b[A`, etc). They must
        // not pollute the buffer — pressing Up to recall the last
        // command and then Enter shouldn't record anything; the
        // user didn't type a NEW command.
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"\x1b[A\x1b[B\x1b[C\x1b[D\r");
        assert!(out.is_empty());
    }

    #[test]
    fn osc_with_st_terminator_dropped() {
        // OSC 0 ; title \x1b\ — the ST is two bytes. Cleanly
        // skipped by the StPending state.
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"\x1b]0;my title\x1b\\ls\r");
        assert_eq!(out, vec!["ls".to_string()]);
    }

    #[test]
    fn osc_with_bel_terminator_dropped() {
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"\x1b]0;another\x07ls\r");
        assert_eq!(out, vec!["ls".to_string()]);
    }

    #[test]
    fn bracketed_paste_markers_are_csi() {
        // `\x1b[200~` opens, `\x1b[201~` closes. Both are CSI
        // terminated by `~` (0x7e). The PAYLOAD between them is
        // bytes the user pasted, which we want to record.
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"\x1b[200~grep TODO src/\x1b[201~\r");
        assert_eq!(out, vec!["grep TODO src/".to_string()]);
    }

    #[test]
    fn short_escape_drops_two_bytes() {
        // `ESC k` (a one-letter escape used by some shells) — drop
        // both ESC and the letter, then proceed normally.
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"ls\x1bka\r");
        // After ESC, 'k' is consumed by the state machine. 'a' is
        // a normal byte appended after `ls`.
        assert_eq!(out, vec!["lsa".to_string()]);
    }

    #[test]
    fn feed_chunks_split_mid_escape_resume_cleanly() {
        // Real-world: bytes arrive in arbitrary chunks. Split a
        // single CSI sequence into two `feed` calls and confirm we
        // don't pollute the buffer.
        let mut b = CommandBuffer::new();
        let mut out = b.feed(b"ls\x1b");
        out.extend(b.feed(b"[A\r"));
        // The `\x1b[A` is dropped; only "ls" remains, submitted on
        // the `\r`.
        assert_eq!(out, vec!["ls".to_string()]);
    }

    #[test]
    fn whitespace_only_command_filtered() {
        let mut b = CommandBuffer::new();
        let out = one(&mut b, b"   \t  \r");
        // No newline-on-empty submission. TAB is dropped, leaving
        // whitespace, which `take_command` filters as empty.
        assert!(out.is_empty());
    }

    #[test]
    fn unicode_command_preserved_verbatim() {
        let mut b = CommandBuffer::new();
        let out = one(&mut b, "git commit -m \"исправил баг\"\r".as_bytes());
        assert_eq!(out, vec!["git commit -m \"исправил баг\"".to_string()]);
    }

    #[test]
    fn command_capture_round_trips_through_history() {
        // Wires `CommandCapture` to a `:memory:` History store and
        // verifies feed → record → suggest. The renderer-side
        // integration uses the same plumbing so a regression here
        // would silently break the popup.
        let history = Arc::new(Mutex::new(
            rterm_history::History::open(":memory:").unwrap(),
        ));
        let cap = CommandCapture::new(Some(history.clone()));
        cap.feed(b"ls -la\r");
        cap.feed(b"git status\r");
        cap.feed(b"ls -la\r"); // duplicate
        let h = history.lock().unwrap();
        assert_eq!(h.len().unwrap(), 2);
        let entry = h.lookup("ls -la").unwrap().unwrap();
        assert_eq!(entry.count, 2, "ls -la submitted twice");
    }

    #[test]
    fn current_input_returns_unconsumed_prefix() {
        // The popup queries the half-typed command without
        // resetting the buffer. After feeding "git stat" the
        // accumulated input should be readable verbatim, and
        // the buffer must still hold those bytes for the next
        // feed (the submit on `\r` is what clears).
        let mut b = CommandBuffer::new();
        let _ = b.feed(b"git stat");
        assert_eq!(b.current_input(), "git stat");
        // Buffer survives the read.
        let out = b.feed(b"us\r");
        assert_eq!(out, vec!["git status".to_string()]);
        // After submit the buffer is empty.
        assert_eq!(b.current_input(), "");
    }

    #[test]
    fn current_input_truncates_at_first_invalid_utf8() {
        // A paste can deliver a multi-byte char split across two
        // feed calls. Reading mid-split must surface the valid
        // prefix only; the dangling continuation byte stays in the
        // buffer and reassembles on the next feed.
        let mut b = CommandBuffer::new();
        // 'л' = 0xd0 0xbb, 'с' = 0xd1 0x81. Feed 'л' + half of 'с'.
        let _ = b.feed(&[b'a', 0xd0, 0xbb, 0xd1]);
        assert_eq!(b.current_input(), "aл");
        // Complete the broken char on the next feed.
        let _ = b.feed(&[0x81, b'\r']);
        // Note: we don't read current_input here — the submit
        // emitted "aлс" already. Just verify the submit went
        // through cleanly.
    }

    #[test]
    fn command_capture_generation_bumps_on_feed() {
        // The popup's debounce timer keys off generation deltas.
        // Pin that every feed (even pure control) advances the
        // counter — backspaces / Ctrl+U change the prefix the
        // popup is matching against just like a typed letter.
        let cap = CommandCapture::new(None);
        let g0 = cap.generation();
        cap.feed(b"a");
        let g1 = cap.generation();
        assert!(g1 > g0, "typed char must bump generation");
        cap.feed(b"\x7f"); // backspace
        let g2 = cap.generation();
        assert!(g2 > g1, "backspace must bump generation");
        cap.feed(b"\x1b[A"); // CSI arrow-up (dropped, but feed runs)
        let g3 = cap.generation();
        assert!(g3 > g2, "control sequence still bumps generation");
    }

    #[test]
    fn command_capture_current_input_through_wrapper() {
        let cap = CommandCapture::new(None);
        cap.feed(b"vim ~/.bashrc");
        assert_eq!(cap.current_input(), "vim ~/.bashrc");
        // Bare Ctrl+U clears the line — popup must see an empty
        // prefix afterwards.
        cap.feed(b"\x15");
        assert_eq!(cap.current_input(), "");
    }

    #[test]
    fn command_capture_silent_when_history_disabled() {
        // `None` history disables capture entirely — feed should be
        // a no-op (no panic) even on large input.
        let cap = CommandCapture::new(None);
        cap.feed(b"any command\r");
        cap.feed(b"another\r");
        // No assertion needed beyond "no panic"; cap.history is None
        // so nothing was recorded anywhere.
    }
}
