//! Cross-platform PTY for rterm. Wraps `portable-pty` (ConPTY on Windows,
//! openpty on Unix incl. FreeBSD) and exposes a small control surface that can
//! be cheaply cloned across threads.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use rterm_core::Size;

type SharedMaster = Arc<Mutex<Box<dyn MasterPty + Send>>>;
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
/// Shared, waitable handle to the spawned child. Cloned out via
/// [`Pty::child_handle`] so an exit-watcher thread can `try_wait` it
/// independently of the owning `Pty` (needed on Windows, where ConPTY
/// often never EOFs the master reader when the shell exits).
pub type SharedChild = Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>;

pub struct Pty {
    master: SharedMaster,
    child: SharedChild,
    writer: SharedWriter,
    pid: Option<u32>,
}

/// Side handle for writing input and resizing — cheaply Clone, Send + Sync.
#[derive(Clone)]
pub struct PtyControl {
    master: SharedMaster,
    writer: SharedWriter,
    pid: Option<u32>,
}

impl Pty {
    pub fn spawn(
        program: &str,
        args: &[String],
        size: Size,
        cwd: Option<&Path>,
    ) -> Result<Self> {
        Self::spawn_with_env(program, args, size, cwd, &[])
    }

    /// Same as `spawn` but with `env_extra` applied *after* the built-in
    /// `TERM=xterm-256color` / `COLORTERM=truecolor` / `TERM_PROGRAM*`
    /// defaults, so user-supplied entries (e.g. `RUST_BACKTRACE=1`) win
    /// the last-write race when their key matches a default's.
    pub fn spawn_with_env(
        program: &str,
        args: &[String],
        size: Size,
        cwd: Option<&Path>,
        env_extra: &[(String, String)],
    ) -> Result<Self> {
        let system = NativePtySystem::default();
        let pair = system
            .openpty(PtySize {
                cols: size.cols.max(1),
                rows: size.rows.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut cmd = CommandBuilder::new(program);
        for a in args {
            cmd.arg(a);
        }
        // Caller-supplied cwd wins; otherwise inherit the parent's.
        let chosen = cwd
            .map(|p| p.to_path_buf())
            .or_else(|| std::env::current_dir().ok());
        if let Some(c) = chosen {
            cmd.cwd(c);
        }
        // TERM advertises capabilities to the child. xterm-256color is the
        // safe default; tighten once we have a published terminfo entry.
        for (k, v) in default_env() {
            cmd.env(k, v);
        }
        for (k, v) in env_extra {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd).context("spawn_command failed")?;
        let pid = child.process_id();
        let writer = pair.master.take_writer().context("take writer")?;

        // Drop the slave so reads on master receive EOF when the child exits.
        drop(pair.slave);

        Ok(Self {
            master: Arc::new(Mutex::new(pair.master)),
            child: Arc::new(Mutex::new(child)),
            writer: Arc::new(Mutex::new(writer)),
            pid,
        })
    }

    pub fn control(&self) -> PtyControl {
        PtyControl {
            master: Arc::clone(&self.master),
            writer: Arc::clone(&self.writer),
            pid: self.pid,
        }
    }

    /// OS process id of the spawned shell, if the underlying platform
    /// reported one. Stable for the lifetime of this `Pty`.
    pub fn process_id(&self) -> Option<u32> {
        self.pid
    }

    pub fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>> {
        // Use the same `expect("...poisoned")` label as `write_input` /
        // `resize` for consistent panic messages — caller is the App's
        // pane-spawn path, where a poisoned master mutex means the PTY
        // is unusable anyway.
        let master = self.master.lock().expect("pty master mutex poisoned");
        master.try_clone_reader().context("clone reader")
    }

    pub fn try_wait(&mut self) -> Result<Option<portable_pty::ExitStatus>> {
        let mut child = self.child.lock().expect("pty child mutex poisoned");
        Ok(child.try_wait()?)
    }

    pub fn kill(&mut self) -> Result<()> {
        let mut child = self.child.lock().expect("pty child mutex poisoned");
        child.kill().context("pty kill")
    }

    /// Shared, waitable handle to the child for an out-of-band exit
    /// watcher. Polling `try_wait` on this lets the app detect a shell
    /// that exited without the master reader seeing EOF — the common
    /// ConPTY case behind "`exit` doesn't close the tab" on Windows.
    pub fn child_handle(&self) -> SharedChild {
        Arc::clone(&self.child)
    }
}

impl PtyControl {
    /// Same value as `Pty::process_id`. Cached at spawn so the control
    /// handle stays cheap to use across threads.
    pub fn process_id(&self) -> Option<u32> {
        self.pid
    }

    pub fn write_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().expect("pty writer mutex poisoned");
        w.write_all(bytes)?;
        w.flush()
    }

    pub fn resize(&self, size: Size) -> Result<()> {
        let master = self.master.lock().expect("pty master mutex poisoned");
        master
            .resize(PtySize {
                cols: size.cols.max(1),
                rows: size.rows.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize failed")
    }

    /// PID of the current foreground process group on this PTY (whatever
    /// is currently in the foreground of the shell — `bash` when sitting
    /// at a prompt, `vim` when an editor is open, etc.). Returns `None`
    /// on platforms / backends that don't implement
    /// `MasterPty::process_group_leader` (notably Windows + ConPTY). Cheap
    /// (one ioctl on Unix); safe to call per-frame from the render loop.
    #[cfg(unix)]
    pub fn foreground_pgid(&self) -> Option<u32> {
        let master = self.master.lock().ok()?;
        master.process_group_leader().and_then(|p| u32::try_from(p).ok())
    }
    /// Windows / ConPTY has no `process_group_leader` concept — there's
    /// no foreground-pgrp on the PTY master side. Return `None` so the
    /// fallback path (parse `tasklist` / look at `child.pid`) kicks in
    /// at the caller. portable-pty 0.8 doesn't gate the trait method
    /// `cfg(unix)`-only — the method just doesn't exist on the Windows
    /// `MasterPty` impl, which makes a single cross-platform call
    /// fail to compile on Windows.
    #[cfg(not(unix))]
    pub fn foreground_pgid(&self) -> Option<u32> {
        None
    }
}

/// Linux: read the `comm` (executable basename) of `pid` from `/proc`.
/// Returns `None` if the file is missing (the process exited, or this is
/// running on a non-Linux platform without `procfs`). The value is
/// short — typically ≤ 15 bytes, the kernel's TASK_COMM_LEN — and is the
/// closest portable fallback to "what is the user actually running right
/// now in the shell" when no OSC 0/1/2 title has been emitted.
pub fn read_proc_comm(pid: u32) -> Option<String> {
    let path = format!("/proc/{}/comm", pid);
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Default environment overrides applied to every shell spawned by rterm.
/// Listed separately (rather than inlined) so the set is easy to audit and
/// to unit-test. Returns a stable list — no allocation per call other than
/// the version `String`.
fn default_env() -> Vec<(&'static str, String)> {
    vec![
        ("TERM", "xterm-256color".into()),
        ("COLORTERM", "truecolor".into()),
        // Lets shells (and inner programs like `git`, `bat`, `lazygit`)
        // detect the host terminal without resorting to fragile ioctl
        // sniffing. The pair mirrors what iTerm2, Kitty, and WezTerm
        // advertise so shell-init scripts written for those work here.
        ("TERM_PROGRAM", "rterm".into()),
        ("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION").into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_proc_comm_returns_some_for_self_on_linux() {
        // The current process always has a /proc/<pid>/comm file on Linux —
        // a non-Linux build returns None, so gate the assertion. We don't
        // pin the exact value (it's whatever the test runner is called)
        // but check it's non-empty and short — comm is capped at 15 chars.
        let pid = std::process::id();
        let comm = read_proc_comm(pid);
        #[cfg(target_os = "linux")]
        {
            let c = comm.expect("/proc/self/comm should be readable");
            assert!(!c.is_empty());
            assert!(c.len() <= 16, "comm {c:?} longer than TASK_COMM_LEN");
            // Must NOT carry a trailing newline.
            assert!(!c.ends_with('\n'));
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(comm.is_none());
        }
    }

    #[test]
    fn read_proc_comm_returns_none_for_unlikely_pid() {
        // PID 0 is the kernel scheduler — not exposed in /proc as a real
        // process anywhere we run. The helper must return None instead
        // of panicking when the path doesn't exist.
        assert!(read_proc_comm(0).is_none());
        // A pid above the kernel's typical max (4_194_304 on 64-bit
        // Linux) is also safe — file simply isn't there.
        assert!(read_proc_comm(u32::MAX).is_none());
    }

    #[test]
    fn default_env_advertises_truecolor_and_no_duplicates() {
        // `TERM=xterm-256color` + `COLORTERM=truecolor` is the
        // standard pair shells like bash / zsh / fish look for
        // to enable truecolor SGR (38;2;…) without sniffing. Also
        // pin "no duplicate keys" — a shell setting TERM_PROGRAM
        // twice would shadow whichever wins; the test catches a
        // sloppy edit that adds a second entry.
        let env = default_env();
        assert!(env.iter().any(|(k, v)| *k == "COLORTERM" && v == "truecolor"));
        assert!(env.iter().any(|(k, v)| *k == "TERM" && v == "xterm-256color"));
        let mut keys: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        keys.sort();
        let dedup_len = {
            let mut k = keys.clone();
            k.dedup();
            k.len()
        };
        assert_eq!(keys.len(), dedup_len, "duplicate env keys: {keys:?}");
    }

    #[test]
    fn default_env_advertises_term_program() {
        // The TERM_PROGRAM signal is what shell dotfiles look at to enable
        // rterm-specific integrations (OSC 7 / 133 prompts, etc.). Pin the
        // exact name + a non-empty version so accidental rename breaks a
        // test, not user shell setups.
        let env = default_env();
        let lookup = |k: &str| env.iter().find(|(ek, _)| *ek == k).map(|(_, v)| v.as_str());
        assert_eq!(lookup("TERM"), Some("xterm-256color"));
        assert_eq!(lookup("COLORTERM"), Some("truecolor"));
        assert_eq!(lookup("TERM_PROGRAM"), Some("rterm"));
        let version = lookup("TERM_PROGRAM_VERSION").expect("TERM_PROGRAM_VERSION");
        assert!(!version.is_empty());
        // Sanity: version looks like a semver-ish dotted string. Don't pin
        // the exact value — it bumps with Cargo.toml — but ensure we're
        // not exposing something like `(unknown)`.
        assert!(version.contains('.'), "version {version:?} looks malformed");
    }
}
