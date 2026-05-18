//! rterm entrypoint.
//!
//! Default: open the GUI window (winit + wgpu + glyphon) and run the
//! configured shell inside it. `--smoke` runs a headless PTY+parser pipeline
//! used for CI/dev-loop validation.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use rterm_config::Config;
use rterm_core::{Position, Size, Terminal};
use rterm_plugin::PluginHost;
use rterm_pty::{Pty, PtyControl};
use rterm_render::palette::Palette;
use rterm_render::{EventSink, Pane, PaneSpawner, SharedTerminal, TerminalIo, UserBinding};

struct PtyAdapter(PtyControl);

impl TerminalIo for PtyAdapter {
    fn write_input(&self, bytes: &[u8]) {
        if let Err(e) = self.0.write_input(bytes) {
            tracing::warn!("pty write failed: {e}");
        }
    }
    fn resize(&self, cols: u16, rows: u16) {
        if let Err(e) = self.0.resize(rterm_core::Size { cols, rows }) {
            tracing::warn!("pty resize failed: {e:#}");
        }
    }
    fn process_id(&self) -> Option<u32> {
        self.0.process_id()
    }
    fn foreground_pgid(&self) -> Option<u32> {
        self.0.foreground_pgid()
    }
}

struct GuiSpawner {
    config: Config,
    /// Shared history sink — same Arc the App holds, so every pane
    /// records into one SQLite file. `None` when history is disabled
    /// (open-failure or `--smoke`).
    history: Option<Arc<Mutex<rterm_history::History>>>,
}

impl PaneSpawner for GuiSpawner {
    fn spawn_pane(&self, cwd: Option<&str>) -> Result<Pane> {
        // Start with a generous default; the renderer will resize to fit on
        // first sync_terminal_size pass.
        let initial = Size { cols: 100, rows: 32 };
        let mut term = Terminal::new(initial);
        // Seed OSC 10/11 responses with the palette's default colours.
        let pal = rterm_render::palette::palette();
        term.set_default_colors(pal.default_fg, pal.default_bg);
        // Seed OSC 4 replies so apps see the configured 16-colour palette.
        term.set_named_palette(pal.named);
        // Seed OSC 12 replies with the configured cursor colour, if any.
        term.set_cursor_color(pal.cursor);
        term.set_scrollback_limit(self.config.terminal.scrollback);
        let terminal: SharedTerminal = Arc::new(Mutex::new(term));

        let (program, args) = resolve_shell(&self.config);
        let cwd_path = cwd.map(std::path::Path::new);
        // User-supplied `[shell.env]` entries are forwarded to every
        // spawned pane. Empty in the default config.
        let env_extra: Vec<(String, String)> = self
            .config
            .shell
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let pty = Pty::spawn_with_env(&program, &args, initial, cwd_path, &env_extra)?;
        let reader = pty.try_clone_reader()?;
        let control = pty.control();

        let alive = Arc::new(AtomicBool::new(true));
        let activity = Arc::new(AtomicBool::new(false));
        let last_output_ms = Arc::new(AtomicU64::new(now_ms()));
        let join = spawn_reader_thread(
            reader,
            Arc::clone(&terminal),
            Arc::clone(&alive),
            Arc::clone(&activity),
            Arc::clone(&last_output_ms),
        );
        let io: Arc<dyn TerminalIo> = Arc::new(PtyAdapter(control));

        let keepalive: Box<dyn std::any::Any + Send> = Box::new((pty, join));
        Ok(Pane::new(terminal, io, program, alive, activity, last_output_ms, keepalive, self.history.clone()))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `$XDG_CACHE_HOME/rterm` (or `~/.cache/rterm`) — same path the manual
/// `save_scrollback` action uses. Used by plugins via `rterm.cache_dir()`.
fn cache_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = std::path::PathBuf::from(h);
                p.push(".cache");
                p
            })
        })?;
    Some(base.join("rterm"))
}

/// Resolve the path of the persistent command-history SQLite store.
/// Returns `None` when no cache dir is available (e.g. a sandboxed
/// run with neither `XDG_CACHE_HOME` nor `HOME` exported). Shared by
/// the GUI bootstrap and the `--history` CLI so both see the same
/// file.
fn history_db_path() -> Option<std::path::PathBuf> {
    cache_dir().map(|d| d.join("history.sqlite3"))
}

/// Dispatch table for `rterm --history <subcommand>`. Parses the
/// first positional after `--history` and runs it. Unknown / missing
/// subcommand → usage hint + exit 0 (matches `rterm --help` style:
/// info-only commands never fail).
fn run_history_subcommand<I: IntoIterator<Item = String>>(args: I) -> Result<()> {
    let mut iter = args.into_iter();
    let sub = iter.next().unwrap_or_default();
    let Some(path) = history_db_path() else {
        eprintln!("--history: no cache directory available on this platform");
        return Ok(());
    };
    let h = match rterm_history::History::open(&path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("--history: cannot open {}: {e:#}", path.display());
            return Ok(());
        }
    };
    match sub.as_str() {
        "list" => {
            // Optional `[prefix]` after `list` filters by prefix;
            // optional `--limit N` clamps output (default 50, to
            // keep one screen on a typical terminal).
            let mut limit: usize = 50;
            let mut prefix = String::new();
            while let Some(a) = iter.next() {
                if a == "--limit" {
                    if let Some(n) = iter.next().and_then(|s| s.parse::<usize>().ok()) {
                        limit = n;
                    }
                } else if let Some(s) = a.strip_prefix("--limit=") {
                    if let Ok(n) = s.parse::<usize>() {
                        limit = n;
                    }
                } else if prefix.is_empty() {
                    prefix = a;
                }
            }
            let rows = h.suggest(&prefix, limit).context("--history list")?;
            if rows.is_empty() {
                if prefix.is_empty() {
                    eprintln!("--history: empty (capture is on but no commands have been submitted yet)");
                } else {
                    eprintln!("--history: no entries match prefix {prefix:?}");
                }
                return Ok(());
            }
            // Header row uses padded literals directly so clippy's
            // `print_literal` lint doesn't fire — it's right that
            // `println!("{}", "command")` is silly, even if the
            // padding made it readable in source.
            println!("count  last_used   command");
            for s in rows {
                println!("{:>5}  {:>10}  {}", s.count, s.last_used, s.text);
            }
        }
        "count" => {
            println!("{}", h.len()?);
        }
        "clear" => {
            h.clear()?;
            eprintln!("--history: cleared");
        }
        "forget" => {
            let Some(target) = iter.next() else {
                eprintln!("--history forget <text>: missing argument");
                return Ok(());
            };
            if h.forget(&target)? {
                eprintln!("--history: forgot {target:?}");
            } else {
                eprintln!("--history: no entry for {target:?}");
            }
        }
        _ => {
            eprintln!(
                "Usage: rterm --history <subcommand>\n  \
                 list [prefix] [--limit N]   most-frequent commands matching prefix\n  \
                 count                       number of unique commands recorded\n  \
                 forget <text>               drop one entry\n  \
                 clear                       drop every entry",
            );
        }
    }
    Ok(())
}

/// Print a shell-init snippet that emits the OSC 133 semantic-
/// prompt markers rterm consumes for prompt-jump navigation, the
/// slow-command event, and (in upcoming work) the command-history
/// capture. Output is meant for `eval "$(rterm --shell-integration
/// bash)"` so users don't have to copy-paste the snippet by hand
/// — it stays in lock-step with whatever future markers rterm
/// learns to read.
///
/// Supported shells:
/// * `bash` (also accepts `sh`)
/// * `zsh`
/// * `fish`
/// * `powershell` (also accepts `pwsh`)
///
/// Unknown / missing argument prints usage to stderr and exits 0.
fn run_shell_integration(shell: &str) -> Result<()> {
    let snippet = match shell.to_ascii_lowercase().as_str() {
        "bash" | "sh" => BASH_SNIPPET,
        "zsh" => ZSH_SNIPPET,
        "fish" => FISH_SNIPPET,
        "powershell" | "pwsh" => POWERSHELL_SNIPPET,
        "" => {
            eprintln!(
                "Usage: rterm --shell-integration <shell>\n  \
                 Supported: bash, zsh, fish, powershell.\n  \
                 Typical use: add `eval \"$(rterm --shell-integration bash)\"`\n  \
                 (or the equivalent for your shell) to your rc file.",
            );
            return Ok(());
        }
        other => {
            eprintln!(
                "--shell-integration: unknown shell {other:?}. \
                 Supported: bash, zsh, fish, powershell.",
            );
            return Ok(());
        }
    };
    print!("{snippet}");
    Ok(())
}

/// Bash hook set. `PROMPT_COMMAND` runs before every prompt; we use
/// it to emit `133;A` (start) then `133;B` (input start) wrapping
/// the actual prompt via a `PS1` injection. `DEBUG` trap fires
/// just before each command runs → `133;C`. `133;D` carries the
/// exit code after each command.
const BASH_SNIPPET: &str = r#"# rterm shell integration (bash).
# Emits OSC 133 semantic-prompt markers so rterm can detect prompt
# boundaries, jump between prior commands, and (future) feed its
# command-history popup with what the remote shell actually ran.
#
# Add to ~/.bashrc:
#
#   eval "$(rterm --shell-integration bash)"
#
__rterm_133_A() { printf '\e]133;A\e\\'; }
__rterm_133_B() { printf '\e]133;B\e\\'; }
__rterm_133_C() { printf '\e]133;C\e\\'; }
__rterm_133_D() { printf '\e]133;D;%s\e\\' "$?"; }
case "$PROMPT_COMMAND" in
    *__rterm_133_D*) ;;
    "") PROMPT_COMMAND='__rterm_133_D' ;;
    *)  PROMPT_COMMAND='__rterm_133_D;'"$PROMPT_COMMAND" ;;
esac
PS1='\[\e]133;A\e\\\]'"$PS1"'\[\e]133;B\e\\\]'
trap '__rterm_133_C' DEBUG
"#;

/// Zsh hook set — preexec / precmd already give us the right
/// edges; OSC 133;A goes at the start of the prompt, 133;B just
/// before the user gets to type, 133;C right before the command
/// runs, 133;D with the exit code after it finishes.
const ZSH_SNIPPET: &str = r#"# rterm shell integration (zsh).
# Adds OSC 133 semantic-prompt markers via preexec / precmd hooks.
#
# Add to ~/.zshrc:
#
#   eval "$(rterm --shell-integration zsh)"
#
__rterm_preexec() { printf '\e]133;C\e\\'; }
__rterm_precmd()  { printf '\e]133;D;%s\e\\' "$?"; printf '\e]133;A\e\\'; }
typeset -ga preexec_functions precmd_functions
preexec_functions+=(__rterm_preexec)
precmd_functions+=(__rterm_precmd)
PS1=$'%{\e]133;A\e\\%}'$PS1$'%{\e]133;B\e\\%}'
"#;

/// Fish hook set — uses fish's event-driven hooks
/// (`fish_prompt` / `fish_preexec` / `fish_postexec`).
const FISH_SNIPPET: &str = r#"# rterm shell integration (fish).
# Wires fish_prompt / fish_preexec / fish_postexec to OSC 133.
#
# Add to ~/.config/fish/config.fish:
#
#   rterm --shell-integration fish | source
#
function __rterm_prompt --on-event fish_prompt
    printf '\e]133;A\e\\'
end
function __rterm_preexec --on-event fish_preexec
    printf '\e]133;C\e\\'
end
function __rterm_postexec --on-event fish_postexec
    printf '\e]133;D;%s\e\\' "$status"
end
"#;

/// PowerShell hook — wraps the `prompt` function so each prompt
/// emits the OSC sequence at the right edges. Works with both
/// Windows PowerShell 5.1 and PowerShell Core 7+.
const POWERSHELL_SNIPPET: &str = r#"# rterm shell integration (PowerShell).
# Add to $PROFILE:
#
#   Invoke-Expression (& rterm --shell-integration powershell | Out-String)
#
$global:__RtermOriginalPrompt = if (Test-Path Function:\prompt) { Get-Item Function:\prompt | Select-Object -ExpandProperty Definition } else { $null }
function global:prompt {
    $exit = $LASTEXITCODE
    if ($null -ne $exit) {
        [Console]::Write([char]27 + "]133;D;" + $exit + [char]27 + "\")
    }
    [Console]::Write([char]27 + "]133;A" + [char]27 + "\")
    $body = if ($global:__RtermOriginalPrompt) { Invoke-Expression $global:__RtermOriginalPrompt } else { "PS " + (Get-Location) + "> " }
    [Console]::Write($body)
    [Console]::Write([char]27 + "]133;B" + [char]27 + "\")
    ""
}
"#;

struct PluginBridge(Arc<Mutex<PluginHost>>);

impl EventSink for PluginBridge {
    fn emit(&self, event: &str, payload: &str) {
        if let Ok(host) = self.0.lock() {
            if let Err(e) = host.emit(event, payload.to_string()) {
                tracing::warn!("plugin emit '{event}' failed: {e:#}");
            }
        }
    }

    fn list_actions(&self) -> Vec<String> {
        self.0.lock().map(|h| h.action_names()).unwrap_or_default()
    }

    fn run_action(&self, name: &str) {
        if let Ok(host) = self.0.lock() {
            if let Err(e) = host.run_action(name) {
                tracing::warn!("plugin action '{name}' failed: {e:#}");
            }
        }
    }

    fn drain_pending_routed_input_by_uid(&self) -> Vec<(u64, Vec<u8>)> {
        self.0
            .lock()
            .map(|h| h.drain_pending_routed_input_by_uid())
            .unwrap_or_default()
    }

    fn drain_pending_routed_input(&self) -> Vec<((usize, usize), Vec<u8>)> {
        self.0
            .lock()
            .map(|h| h.drain_pending_routed_input())
            .unwrap_or_default()
    }

    fn take_pending_attention(&self) -> bool {
        self.0
            .lock()
            .map(|h| h.take_pending_attention())
            .unwrap_or(false)
    }

    fn drain_pending_commands(&self) -> Vec<rterm_core::PluginCmd> {
        self.0
            .lock()
            .map(|h| h.drain_pending_commands())
            .unwrap_or_default()
    }

    fn take_pending_focus_by_uid(&self) -> Option<u64> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_focus_by_uid())
    }

    fn take_pending_focus(&self) -> Option<(usize, usize)> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_focus())
    }

    fn take_pending_tab_focus(&self) -> Option<usize> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_tab_focus())
    }

    fn take_pending_copy(&self) -> Option<String> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_copy())
    }

    fn take_pending_scroll_to_line(&self) -> Option<usize> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_scroll_to_line())
    }

    fn take_pending_start_search(&self) -> Option<(String, bool)> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_start_search())
    }

    fn take_pending_font_size(&self) -> Option<f32> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_font_size())
    }

    fn take_pending_opacity(&self) -> Option<f32> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_opacity())
    }

    fn take_pending_bell(&self) -> bool {
        self.0
            .lock()
            .map(|h| h.take_pending_bell())
            .unwrap_or(false)
    }

    fn take_pending_palette(
        &self,
    ) -> Option<(
        Option<[u8; 3]>,
        Option<[u8; 3]>,
        Option<[u8; 3]>,
        Option<[[u8; 3]; 16]>,
    )> {
        let p = self.0.lock().ok().and_then(|h| h.take_pending_palette())?;
        Some((p.default_fg, p.default_bg, p.cursor, p.named))
    }

    fn take_pending_theme(&self) -> Option<String> {
        self.0.lock().ok().and_then(|h| h.take_pending_theme())
    }

    fn drain_pending_tab_titles(&self) -> Vec<(Option<usize>, String)> {
        self.0
            .lock()
            .map(|h| h.drain_pending_tab_titles())
            .unwrap_or_default()
    }

    fn take_pending_window_title(&self) -> Option<Option<String>> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_window_title())
    }

    fn drain_pending_pane_titles_by_uid(&self) -> Vec<(u64, String)> {
        self.0
            .lock()
            .map(|h| h.drain_pending_pane_titles_by_uid())
            .unwrap_or_default()
    }

    fn drain_pending_pane_titles(&self) -> Vec<(usize, usize, String)> {
        self.0
            .lock()
            .map(|h| h.drain_pending_pane_titles())
            .unwrap_or_default()
    }

    fn take_pending_scrollback_limit(&self) -> Option<usize> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_scrollback_limit())
    }

    fn take_pending_tab_silence_ms(&self) -> Option<u64> {
        self.0
            .lock()
            .ok()
            .and_then(|h| h.take_pending_tab_silence_ms())
    }

    fn take_pending_cursor_blink(&self) -> Option<bool> {
        self.0.lock().ok().and_then(|h| h.take_pending_cursor_blink())
    }
    fn take_pending_show_scrollbar(&self) -> Option<bool> {
        self.0.lock().ok().and_then(|h| h.take_pending_show_scrollbar())
    }
    fn take_pending_scroll_on_output(&self) -> Option<bool> {
        self.0.lock().ok().and_then(|h| h.take_pending_scroll_on_output())
    }
    fn take_pending_bell_visual(&self) -> Option<bool> {
        self.0.lock().ok().and_then(|h| h.take_pending_bell_visual())
    }
    fn take_pending_bell_urgent(&self) -> Option<bool> {
        self.0.lock().ok().and_then(|h| h.take_pending_bell_urgent())
    }
    fn take_pending_font_family(&self) -> Option<String> {
        self.0.lock().ok().and_then(|h| h.take_pending_font_family())
    }
    fn take_pending_guake(&self) -> Option<Option<rterm_render::GuakeRunConfig>> {
        self.0.lock().ok().and_then(|h| h.take_pending_guake()).map(
            |inner| {
                inner.map(|(enabled, position, height_pct, width_pct)| {
                    rterm_render::GuakeRunConfig {
                        enabled,
                        position,
                        height_pct,
                        width_pct,
                        // Plugin-driven hot-reload of `[guake]` doesn't
                        // touch the global hotkey today — the worker
                        // is registered once at startup against the
                        // initial config. A Lua plugin that wants to
                        // change the spec on the fly can rewrite
                        // config.toml instead.
                        global_hotkey: String::new(),
                    }
                })
            },
        )
    }
    fn drain_pending_pane_bell_mute_by_uid(&self) -> Vec<(u64, bool)> {
        self.0
            .lock()
            .map(|h| h.drain_pending_pane_bell_mute_by_uid())
            .unwrap_or_default()
    }

    fn drain_pending_pane_bell_mute(&self) -> Vec<(usize, usize, bool)> {
        self.0
            .lock()
            .map(|h| h.drain_pending_pane_bell_mute())
            .unwrap_or_default()
    }
    fn take_pending_slow_command_ms(&self) -> Option<u64> {
        self.0.lock().ok().and_then(|h| h.take_pending_slow_command_ms())
    }

    fn set_terminal_state(&self, snap: rterm_render::TerminalSnapshot) {
        if let Ok(host) = self.0.lock() {
            let panes = snap
                .panes
                .into_iter()
                .map(|p| rterm_plugin::PaneInfo {
                    tab: p.tab,
                    pane: p.pane,
                    uid: p.uid,
                    title: p.title,
                    focused: p.focused,
                    idle_ms: p.idle_ms,
                    scroll_offset: p.scroll_offset,
                    alt_screen: p.alt_screen,
                    reverse_screen: p.reverse_screen,
                    cwd: p.cwd,
                    cols: p.cols,
                    rows: p.rows,
                    cursor_row: p.cursor_row,
                    cursor_col: p.cursor_col,
                    scrollback_len: p.scrollback_len,
                    cursor_visible: p.cursor_visible,
                    cursor_shape: p.cursor_shape.to_string(),
                    cursor_blink: p.cursor_blink,
                    mouse_mode: p.mouse_mode.to_string(),
                    prompt_marks: p.prompt_marks,
                    command_marks: p.command_marks,
                    pid: p.pid,
                    foreground_pgid: p.foreground_pgid,
                    foreground_process: p.foreground_process,
                    bell_muted: p.bell_muted,
                    last_exit_code: p.last_exit_code,
                    progress: p.progress,
                    text: p.text,
                    scrollback_tail: p.scrollback_tail,
                })
                .collect();
            let tabs = snap
                .tabs
                .into_iter()
                .map(|t| rterm_plugin::TabInfo {
                    idx: t.idx,
                    focused: t.focused,
                    pane_count: t.pane_count,
                    focused_pane: t.focused_pane,
                    focused_pane_uid: t.focused_pane_uid,
                    zoomed: t.zoomed,
                    custom_title: t.custom_title,
                    idle_ms: t.idle_ms,
                    unread: t.unread,
                    progress: t.progress,
                })
                .collect();
            host.set_state(rterm_plugin::TerminalState {
                cwd: snap.cwd,
                title: snap.title,
                cols: snap.cols,
                rows: snap.rows,
                panes,
                tabs,
                grid_text: snap.grid_text,
                font_size: snap.font_size,
                font_family: snap.font_family,
                cell_width: snap.cell_width,
                line_height: snap.line_height,
                tab_silence_ms: snap.tab_silence_ms,
                slow_command_ms: snap.slow_command_ms,
                scroll_on_output: snap.scroll_on_output,
                show_scrollbar: snap.show_scrollbar,
                bell_visual: snap.bell_visual,
                bell_urgent: snap.bell_urgent,
                cursor_blink: snap.cursor_blink,
                named_palette: snap.named_palette,
                dragging_tab: snap.dragging_tab,
                scrollback_limit: snap.scrollback_limit,
                selection_text: snap.selection_text,
                opacity: snap.opacity,
                window_focused: snap.window_focused,
                last_exit_code: snap.last_exit_code,
                prompt_mark_lines: snap.prompt_mark_lines,
                command_mark_lines: snap.command_mark_lines,
                theme_fg: snap.theme_fg,
                theme_bg: snap.theme_bg,
                theme_cursor: snap.theme_cursor,
                scrollback_text: snap.scrollback_text,
                search_active: snap.search_active,
                search_query: snap.search_query,
                search_match_index: snap.search_match_index,
                search_match_total: snap.search_match_total,
                search_regex_mode: snap.search_regex_mode,
                active_theme: snap.active_theme,
            });
        }
    }

    fn match_output_line(&self, line: &str) -> Vec<(String, Vec<String>)> {
        self.0
            .lock()
            .map(|h| h.match_output_line(line))
            .unwrap_or_default()
    }
}

fn main() -> Result<()> {
    // `--version` / `-V` / `--help` / `-h` exit before doing any setup so
    // they work without a display, without a writable config dir, and
    // without spawning a PTY. Cheap CLI parsing — no clap dep.
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                if std::env::args().any(|a| a == "--json") {
                    // `{"rterm":"<v>","target_os":"linux","target_arch":"x86_64","profile":"debug"}`
                    // — structured form for build / packaging scripts.
                    // `profile` lets bug-reporters confirm they're not
                    // running an unoptimised binary; `target_arch` is
                    // useful when bundling cross-architecture artifacts.
                    let profile = if cfg!(debug_assertions) {
                        "debug"
                    } else {
                        "release"
                    };
                    let obj = serde_json::json!({
                        "rterm": env!("CARGO_PKG_VERSION"),
                        "target_os": std::env::consts::OS,
                        "target_arch": std::env::consts::ARCH,
                        "profile": profile,
                    });
                    println!("{obj}");
                } else {
                    println!("rterm {}", env!("CARGO_PKG_VERSION"));
                }
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "rterm {}\n\
                     Usage: rterm [OPTIONS]\n\n\
                     Options:\n  \
                       --config <path> Load this config instead of ~/.config/rterm/config.toml\n  \
                       --smoke [--json]  Headless PTY+parser sanity run (no GUI); --json\n  \
                                       emits the result as a single-line JSON object\n  \
                       --render-test   Open a window, present one clear-only frame, exit OK/FAIL\n  \
                       --list-actions [prefix] [--labels|--json]  Print canonical action names\n  \
                                       (filter by prefix; --labels adds a label column; --json\n  \
                                       emits `[{{\"name\":...,\"label\":...}}, ...]` for scripts)\n  \
                       --list-events [prefix] [--json]  Print plugin event names\n  \
                                       (filter by prefix; --json emits a JSON array of strings)\n  \
                       --list-keybindings [action-substr] [--json]  Print user-configured\n  \
                                       keybindings (substring match on action; validity tag;\n  \
                                       --json emits `[{{\"keys\":...,\"action\":...,\"valid\":bool}}]`)\n  \
                       --list-fonts [substr] [--json]  Print installed monospace families\n  \
                                       (case-insensitive substring filter; --json emits\n  \
                                       `{{\"default\":\"...\",\"families\":[...]}}`)\n  \
                       --print-config  Print resolved config as TOML and exit\n  \
                       --print-default-config [--lang en|ru]  Print the bundled\n  \
                                       default config template (annotated with\n  \
                                       comments) for first-run setup. The first-run\n  \
                                       writer also picks the language automatically\n  \
                                       from RTERM_LANG / LC_ALL / LANG (POSIX locale\n  \
                                       priority); `--lang` overrides that for the dump\n  \
                                       only. Currently supported: en (default), ru.\n  \
                       --print-paths [--json]  Print resolved config / plugins / cache paths and exit\n  \
                       --history <subcmd>  Inspect / manage the command-history\n  \
                                       SQLite store. Subcommands: list [prefix]\n  \
                                       [--limit N] | count | forget <text> | clear.\n  \
                                       The store lives at <cache>/history.sqlite3.\n  \
                       --shell-integration <shell>  Print a shell-init snippet that\n  \
                                       emits OSC 133 semantic-prompt markers. Wire\n  \
                                       via `eval \"$(rterm --shell-integration\n  \
                                       bash)\"`. Supported: bash, zsh, fish,\n  \
                                       powershell.\n  \
                       --check         Validate config and exit (non-zero on parse error)\n  \
                       --font-size <pt>  Override font size for this run\n  \
                       --font-family <s> Override font family for this run\n  \
                       --version [--json]  Print version and exit; --json emits\n  \
                                       `{{\"rterm\":\"...\",\"target_os\":\"...\",\
                                       \"target_arch\":\"...\",\"profile\":\"...\"}}`\n  \
                                       (profile is debug or release)\n  \
                       --help          Print this help and exit\n\n\
                     Environment:\n  \
                       RUST_LOG        Logging filter (default silences wgpu/winit chatter).\n  \
                                       Try `RUST_LOG=wgpu_hal=info,info` to debug GPU init.\n  \
                       WGPU_DEBUG=1    Enable wgpu validation + debug callbacks.\n  \
                       WGPU_BACKEND    Force a backend (vulkan|gl|metal|dx12).\n  \
                                       On WSL2 the default is `gl` (Vulkan via mesa can stall).\n  \
                       WGPU_PRESENT_MODE  fifo | mailbox | immediate | autovsync | autonovsync.\n  \
                                       Default is autovsync (fifo on WSL2 to avoid llvmpipe stalls).\n  \
                       WAYLAND_DISPLAY If unset, winit falls back to X11. Useful on WSLg\n  \
                                       systems where the Wayland surface won't map.\n  \
                       SHELL           Fallback shell when `[shell] program` is unset.\n  \
                       RTERM_CONFIG_PATH  Override the default config path \
                                       (~/.config/rterm/config.toml) for every\n  \
                                       sub-flag that resolves it (--check, --print-config, GUI).\n  \
                       RTERM_LANG      Comment language for the auto-generated\n  \
                                       config.toml (`en` (default) or `ru`). Wins\n  \
                                       over LC_ALL / LANG; overridden by\n  \
                                       `--lang` on `--print-default-config`.\n  \
                       RTERM_SMOKE_COMMAND  Replace `echo hello rterm` in `--smoke`. The value\n  \
                                       is passed verbatim to `sh -c` / `cmd /C`. Useful for CI.",
                    env!("CARGO_PKG_VERSION"),
                );
                return Ok(());
            }
            "--list-actions" => {
                // Sorted output so manual lookup ("does rterm have
                // 'split_horizontal'?") is `grep | less`-friendly. The
                // raw source order is mnemonic, but for CLI use sort wins.
                // Optional prefix filter mirrors `--list-events`:
                // `--list-actions opacity_` drills into a category
                // without piping through grep.
                // `--labels` (anywhere in argv) switches to two-column
                // output: `<name>  <human label>` — useful for users
                // who want to know what `swap_pane_next` actually does
                // without scrolling the help overlay.
                let prefix = arg_after_flag("--list-actions");
                let with_labels = std::env::args().any(|a| a == "--labels");
                let json = std::env::args().any(|a| a == "--json");
                let mut pairs = rterm_render::AppAction::name_label_pairs();
                pairs.sort_by(|a, b| a.0.cmp(b.0));
                let filtered: Vec<(&'static str, &'static str)> = pairs
                    .into_iter()
                    .filter(|(name, _)| match prefix.as_deref() {
                        Some(pref) => name.starts_with(pref),
                        None => true,
                    })
                    .collect();
                if json {
                    // `[{ "name": "...", "label": "..." }, ...]`. Use
                    // serde_json so control bytes / arbitrary unicode
                    // in plugin-registered names get the correct
                    // escapes (the old hand-rolled writer only handled
                    // `\\` and `"`).
                    let arr: Vec<_> = filtered
                        .iter()
                        .map(|(name, label)| {
                            serde_json::json!({ "name": name, "label": label })
                        })
                        .collect();
                    println!("{}", serde_json::Value::Array(arr));
                } else if with_labels {
                    for (name, label) in filtered {
                        println!("{:<22} {}", name, label);
                    }
                } else {
                    for (name, _) in filtered {
                        println!("{}", name);
                    }
                }
                return Ok(());
            }
            "--list-events" => {
                // Optional prefix filter: `--list-events pane.` shows
                // only pane-scoped events. Plain prefix match — no
                // globbing — so the contract is predictable for
                // shell-side completion. `--json` flips output to a
                // JSON array of strings for scripting consumers
                // (mirrors `--list-actions --json` / `--print-paths
                // --json`).
                let prefix = arg_after_flag("--list-events");
                let json = std::env::args().any(|a| a == "--json");
                let mut names = builtin_event_names();
                names.sort();
                let filtered: Vec<String> = names
                    .into_iter()
                    .filter(|e| match prefix.as_deref() {
                        Some(pref) => e.starts_with(pref),
                        None => true,
                    })
                    .collect();
                if json {
                    println!("{}", serde_json::Value::Array(
                        filtered.iter().map(|e| serde_json::Value::String(e.clone())).collect(),
                    ));
                } else {
                    for e in filtered {
                        println!("{}", e);
                    }
                }
                return Ok(());
            }
            "--list-keybindings" => {
                // Resolve the same config the GUI would (so `--config
                // path` is honoured) and dump every `[[keybindings]]`
                // entry, alongside a "(invalid)" tag for entries whose
                // spec or action wouldn't survive `UserBinding::from_config`.
                // Optional substring filter matches the *action* name
                // (case-insensitive, contains) — power users with 30+
                // bindings find a specific entry without `grep`.
                let mut path: Option<std::path::PathBuf> = None;
                let mut iter = std::env::args().skip(1);
                while let Some(a) = iter.next() {
                    if a == "--config" {
                        if let Some(p) = iter.next() {
                            path = Some(std::path::PathBuf::from(p));
                        }
                    } else if let Some(p) = a.strip_prefix("--config=") {
                        path = Some(std::path::PathBuf::from(p));
                    }
                }
                let filter = arg_after_flag("--list-keybindings").map(|s| s.to_lowercase());
                let path = path.or_else(Config::default_path);
                let cfg = match path.as_ref() {
                    Some(p) => Config::load_from(p).unwrap_or_default(),
                    None => Config::default(),
                };
                let matches: Vec<&rterm_config::Keybinding> = cfg
                    .keybindings
                    .iter()
                    .filter(|kb| match filter.as_deref() {
                        Some(needle) => kb.action.to_lowercase().contains(needle),
                        None => true,
                    })
                    .collect();
                let json = std::env::args().any(|a| a == "--json");
                if json {
                    // `[{"keys":"Ctrl+T","action":"new_tab","valid":true}, ...]`.
                    let arr: Vec<_> = matches
                        .iter()
                        .map(|kb| {
                            let valid =
                                UserBinding::from_config(&kb.keys, &kb.action).is_some();
                            serde_json::json!({
                                "keys": kb.keys,
                                "action": kb.action,
                                "valid": valid,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::Value::Array(arr));
                    return Ok(());
                }
                if cfg.keybindings.is_empty() {
                    println!("(no [[keybindings]] in config — only built-in shortcuts active)");
                } else if matches.is_empty() {
                    println!(
                        "(no [[keybindings]] match {:?})",
                        filter.unwrap_or_default(),
                    );
                } else {
                    for kb in &matches {
                        let ok = UserBinding::from_config(&kb.keys, &kb.action).is_some();
                        let tag = if ok { "" } else { "  (invalid)" };
                        println!("{:<24} {}{}", kb.keys, kb.action, tag);
                    }
                }
                return Ok(());
            }
            "--list-fonts" => {
                // Diagnostic: which monospace families does the system
                // have, and which one would rterm pick when the user
                // leaves `font.family` blank? Useful for chasing the
                // "uneven character widths" class of bugs. Optional
                // case-insensitive substring filter — `--list-fonts jet`
                // surfaces "JetBrains Mono" without grep indirection or
                // remembering the exact case. The lookup uses
                // `contains` (not `starts_with` like the other CLI
                // filters) since font family names commonly start with
                // a vendor word ("DejaVu", "JetBrains") the user might
                // not remember.
                let filter = arg_after_flag("--list-fonts").map(|s| s.to_lowercase());
                let json = std::env::args().any(|a| a == "--json");
                let families = rterm_render::list_monospace_families();
                let chosen = rterm_render::default_monospace_family();
                let filtered: Vec<&String> = families
                    .iter()
                    .filter(|f| match filter.as_deref() {
                        Some(needle) => f.to_lowercase().contains(needle),
                        None => true,
                    })
                    .collect();
                if json {
                    let obj = serde_json::json!({
                        "default": chosen,
                        "families": filtered,
                    });
                    println!("{obj}");
                    return Ok(());
                }
                match chosen.as_deref() {
                    Some(name) => println!("default: {name}"),
                    None => println!("default: (cosmic-text Family::Monospace fallback)"),
                }
                for f in &families {
                    if let Some(needle) = filter.as_deref() {
                        if !f.to_lowercase().contains(needle) {
                            continue;
                        }
                    }
                    println!("{}", f);
                }
                return Ok(());
            }
            "--check" => {
                // Validate config and exit. Same `--config` honouring as
                // --print-config; non-zero exit on parse failure makes this
                // CI-friendly. Also parses init.lua + plugins/*.lua syntax
                // (without executing them) so a typo in a hook can't
                // surprise users at runtime.
                let mut path: Option<std::path::PathBuf> = None;
                let mut args = std::env::args().skip(1);
                while let Some(a) = args.next() {
                    if a == "--config" {
                        if let Some(p) = args.next() {
                            path = Some(std::path::PathBuf::from(p));
                        }
                    } else if let Some(p) = a.strip_prefix("--config=") {
                        path = Some(std::path::PathBuf::from(p));
                    }
                }
                let path = path.or_else(Config::default_path);
                let mut errors = 0usize;
                let mut checked = 0usize;
                let mut warnings = 0usize;
                match path.as_ref() {
                    Some(p) => match Config::load_from(p) {
                        Ok(cfg) => {
                            println!("ok: {}", p.display());
                            checked += 1;
                            // Duplicate-keybinding warning. Two entries
                            // with the same key combo are almost always
                            // a user mistake — the second one shadows
                            // the first and the user wonders why their
                            // binding doesn't take. We warn (don't
                            // error) so the config still loads.
                            let mut seen: std::collections::HashMap<String, usize> =
                                std::collections::HashMap::new();
                            for kb in &cfg.keybindings {
                                let norm = kb.keys.trim().to_ascii_lowercase();
                                *seen.entry(norm).or_insert(0) += 1;
                            }
                            for (k, n) in seen.iter().filter(|(_, n)| **n > 1) {
                                eprintln!(
                                    "warning: keybinding {:?} defined {} times — only the last wins",
                                    k, n,
                                );
                                warnings += 1;
                            }
                            // Invalid keys / unknown action — same path
                            // as the runtime warning, but surfaced at
                            // --check time so CI catches the typo before
                            // the user discovers their binding doesn't
                            // fire. We can't easily tell which side of
                            // the (keys, action) pair failed, so report
                            // both.
                            for kb in &cfg.keybindings {
                                if UserBinding::from_config(&kb.keys, &kb.action).is_none() {
                                    eprintln!(
                                        "warning: keybinding {:?} → action {:?} ignored \
                                         (unparseable key spec or unknown action)",
                                        kb.keys, kb.action,
                                    );
                                    warnings += 1;
                                }
                            }
                            for msg in config_range_warnings(&cfg) {
                                eprintln!("warning: {}", msg);
                                warnings += 1;
                            }
                            // Catch the common "I copied a config from
                            // my dotfiles and the font isn't installed
                            // on this machine" scenario. The renderer
                            // would silently fall back to whatever
                            // cosmic-text picks, but the user wonders
                            // why their typography looks off. Empty
                            // `font.family` = system default, no warn.
                            if !cfg.font.family.trim().is_empty() {
                                let installed = rterm_render::list_monospace_families();
                                let want = cfg.font.family.trim();
                                let found = installed
                                    .iter()
                                    .any(|f| f.eq_ignore_ascii_case(want));
                                if !found {
                                    let suggestion = closest_font_match(want, &installed);
                                    match suggestion {
                                        Some(s) => eprintln!(
                                            "warning: font.family = {:?} is not installed as \
                                             a monospace face; did you mean {:?}? \
                                             Falling back to system default.",
                                            want, s,
                                        ),
                                        None => eprintln!(
                                            "warning: font.family = {:?} is not installed as \
                                             a monospace face; will fall back to system default",
                                            want,
                                        ),
                                    }
                                    warnings += 1;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("error: {}: {:#}", p.display(), e);
                            errors += 1;
                        }
                    },
                    None => println!("ok: (using built-in defaults; no config path)"),
                }
                let config_dir = path.as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf()));
                if let Some(dir) = config_dir {
                    let init_lua = dir.join("init.lua");
                    if init_lua.exists() {
                        match PluginHost::validate_script(&init_lua) {
                            Ok(()) => {
                                println!("ok: {}", init_lua.display());
                                checked += 1;
                            }
                            Err(e) => {
                                eprintln!("error: {:#}", e);
                                errors += 1;
                            }
                        }
                    }
                    let plugins_dir = dir.join("plugins");
                    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
                        // Sort so output is deterministic across runs —
                        // helpful for diff-friendly CI logs.
                        let mut paths: Vec<_> = entries
                            .flatten()
                            .map(|e| e.path())
                            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lua"))
                            .collect();
                        paths.sort();
                        for p in paths {
                            match PluginHost::validate_script(&p) {
                                Ok(()) => {
                                    println!("ok: {}", p.display());
                                    checked += 1;
                                }
                                Err(e) => {
                                    eprintln!("error: {:#}", e);
                                    errors += 1;
                                }
                            }
                        }
                    }
                }
                if errors > 0 {
                    eprintln!(
                        "summary: {} ok, {} error{} (of {} total)",
                        checked,
                        errors,
                        if errors == 1 { "" } else { "s" },
                        checked + errors,
                    );
                    std::process::exit(1);
                }
                if checked > 1 || warnings > 0 {
                    let warn_part = if warnings > 0 {
                        format!(", {} warning{}", warnings, if warnings == 1 { "" } else { "s" })
                    } else {
                        String::new()
                    };
                    println!("summary: {} files checked{}", checked, warn_part);
                }
                return Ok(());
            }
            "--print-paths" => {
                // Print the resolved filesystem locations rterm uses
                // for config / plugins / cache. Useful for users who
                // want to know "where's my config?" or for shell
                // completion scripts that need to bootstrap from an
                // explicit path. Honours --config and RTERM_CONFIG_PATH
                // (via Config::default_path) so the answer matches
                // what the GUI would actually load.
                let mut path: Option<std::path::PathBuf> = None;
                let mut iter = std::env::args().skip(1);
                while let Some(a) = iter.next() {
                    if a == "--config" {
                        if let Some(p) = iter.next() {
                            path = Some(std::path::PathBuf::from(p));
                        }
                    } else if let Some(p) = a.strip_prefix("--config=") {
                        path = Some(std::path::PathBuf::from(p));
                    }
                }
                let resolved_config = path.or_else(Config::default_path);
                let resolved_dir = resolved_config
                    .as_ref()
                    .and_then(|p| p.parent().map(|d| d.to_path_buf()));
                let init_lua = resolved_dir.as_ref().map(|d| d.join("init.lua"));
                let plugins = resolved_dir.as_ref().map(|d| d.join("plugins"));
                let cache = cache_dir();
                let session = cache.as_ref().map(|d| d.join("session.toml"));
                let history = history_db_path();
                // `--json` switches to single-line JSON so shell
                // scripts / jq pipelines don't have to parse the
                // labelled lines. Bare flag is the default text form.
                let json = std::env::args().any(|a| a == "--json");
                if json {
                    let to_str = |p: Option<&std::path::Path>| {
                        p.map(|p| p.display().to_string())
                    };
                    let obj = serde_json::json!({
                        "config": to_str(resolved_config.as_deref()),
                        "init_lua": to_str(init_lua.as_deref()),
                        "plugins": to_str(plugins.as_deref()),
                        "cache": to_str(cache.as_deref()),
                        "session": to_str(session.as_deref()),
                        "history": to_str(history.as_deref()),
                    });
                    println!("{obj}");
                } else {
                    let show = |label: &str, p: Option<&std::path::Path>| {
                        let s = p
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "(unset)".to_string());
                        println!("{:<12} {}", label, s);
                    };
                    show("config:", resolved_config.as_deref());
                    show("init.lua:", init_lua.as_deref());
                    show("plugins:", plugins.as_deref());
                    show("cache:", cache.as_deref());
                    show("session:", session.as_deref());
                    show("history:", history.as_deref());
                }
                return Ok(());
            }
            "--shell-integration" => {
                // Print a shell-init snippet that emits OSC 133;A
                // / 133;B / 133;C / 133;D markers. Users wire it
                // into their shell rc via `eval "$(rterm
                // --shell-integration bash)"`. The markers feed
                // rterm's existing prompt-jump / slow-command
                // detection, and (once OSC 133-based capture
                // lands) the command-history capture too.
                let shell = std::env::args().nth(2).unwrap_or_default();
                return run_shell_integration(&shell);
            }
            "--history" => {
                // Subcommand router for the rterm-history store
                // (`list` / `count` / `clear` / `forget <text>`).
                // Sister to `--list-actions` / `--list-events`: opens
                // the DB at the same path the GUI uses, runs one
                // operation, prints, exits. Designed so users can
                // verify the capture is working end-to-end without
                // having to bring up the popup UI.
                return run_history_subcommand(std::env::args().skip(2));
            }
            "--print-default-config" => {
                // Emit the bundled `default.toml` template verbatim
                // — all keys with their default values and the
                // per-section comments first-time users want to
                // skim. Distinct from `--print-config`, which
                // serialises the *resolved* struct (no comments,
                // user overrides merged in). Pipe to a file for
                // first-run setup:
                //   `rterm --print-default-config > ~/.config/rterm/config.toml`
                //
                // Language selection: pick up `--lang ru` / `--lang en`
                // anywhere on the rest of the command line; without it
                // we fall back to the env-driven `CommentLang::detect`
                // (`RTERM_LANG` > `LC_ALL` > `LANG` > English).
                let lang = parse_lang_flag(std::env::args().skip(2))
                    .unwrap_or_else(rterm_config::CommentLang::detect);
                print!("{}", rterm_config::default_template_for(lang));
                return Ok(());
            }
            "--print-config" => {
                // Load the resolved config (defaults merged with the user
                // file, if any) and dump it back as TOML. Useful for
                // debugging which keys actually took effect. Honours both
                // `--config path` / `--config=path` and `--font-size <pt>`
                // so the dump reflects what the running terminal will use.
                let GuiCliOverrides {
                    config: path,
                    font_size: font_size_override,
                    font_family: font_family_override,
                } = parse_gui_overrides(std::env::args().skip(1));
                let path = path.or_else(Config::default_path);
                let mut cfg = match path.as_ref() {
                    Some(p) => match Config::load_from(p) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!(
                                "warning: could not load {}: {:#}",
                                p.display(),
                                e
                            );
                            Config::default()
                        }
                    },
                    None => Config::default(),
                };
                if let Some(v) = font_size_override {
                    cfg.font.size = v;
                }
                if let Some(s) = font_family_override {
                    cfg.font.family = s;
                }
                // Flush stderr first so the warning lands before the TOML
                // dump in interactive terminals where stdout/stderr share
                // a TTY — otherwise the buffer order is unpredictable.
                use std::io::Write;
                let _ = std::io::stderr().flush();
                match toml::to_string_pretty(&cfg) {
                    Ok(s) => {
                        // Prepend a header so users redirecting this to
                        // a file (`rterm --print-config > config.toml`)
                        // immediately see what they're looking at.
                        println!(
                            "# rterm — resolved config dump (rterm --print-config).\n\
                             # Generated from {}.\n\
                             # All keys are optional in your real config; defaults apply when omitted.",
                            path.as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "built-in defaults".to_string()),
                        );
                        print!("{}", s);
                    }
                    Err(e) => eprintln!("serialize failed: {e}"),
                }
                return Ok(());
            }
            _ => {}
        }
    }

    // Second-pass arg parse for runtime overrides. Done after the
    // info-only flags so `--version --config x` still exits early.
    let GuiCliOverrides {
        config: explicit_config,
        font_size: explicit_font_size,
        font_family: explicit_font_family,
    } = parse_gui_overrides(std::env::args().skip(1));

    init_tracing();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        target = std::env::consts::OS,
        "rterm starting",
    );

    // `load_or_warn` falls back to defaults on parse failure so a typo'd
    // config doesn't lock the user out of the terminal. Errors are still
    // logged loudly so the cause is obvious.
    fn load_or_warn(path: &std::path::Path) -> Config {
        match Config::load_from(path) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::error!(
                    path = %path.display(),
                    "config parse failed, using defaults: {e:#}"
                );
                Config::default()
            }
        }
    }

    let config = if let Some(path) = explicit_config.as_ref() {
        if !path.exists() {
            tracing::warn!(path = %path.display(), "--config: file not found, using defaults");
        } else {
            tracing::info!(path = %path.display(), "loading explicit config (--config)");
        }
        load_or_warn(path)
    } else {
        match Config::default_path() {
            Some(path) => {
                match Config::ensure_default(&path) {
                    Ok(true) => tracing::info!(path = %path.display(), "wrote default config"),
                    Ok(false) => {}
                    Err(e) => tracing::warn!("could not create default config: {e:#}"),
                }
                load_or_warn(&path)
            }
            None => Config::default(),
        }
    };

    let plugins = Arc::new(Mutex::new(PluginHost::new()?));
    if let Ok(host) = plugins.lock() {
        host.set_clipboard_reader(Arc::new(|| {
            arboard::Clipboard::new()
                .and_then(|mut cb| cb.get_text())
                .ok()
        }));
    }
    // Anchor init.lua / plugins/ to the same directory the config came
    // from. With `--config <path>` this means `<path>/../init.lua` rather
    // than `~/.config/rterm/init.lua` — keeps explicit configs fully
    // self-contained in one directory.
    let config_dir: Option<PathBuf> = explicit_config
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .or_else(|| {
            Config::default_path()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        });

    if let Some(dir) = config_dir.as_ref() {
        if let Ok(host) = plugins.lock() {
            host.set_config_dir(dir.display().to_string());
        }
    }
    if let Some(dir) = cache_dir() {
        if let Ok(host) = plugins.lock() {
            host.set_cache_dir(dir.display().to_string());
        }
    }
    {
        let (shell_program, _) = resolve_shell(&config);
        if let Ok(host) = plugins.lock() {
            host.set_shell_program(shell_program);
        }
    }
    if let Ok(host) = plugins.lock() {
        host.set_builtin_actions(rterm_render::AppAction::canonical_names());
        host.set_builtin_action_labels(
            rterm_render::AppAction::name_label_pairs()
                .into_iter()
                .map(|(n, l)| (n.to_string(), l.to_string()))
                .collect(),
        );
        host.set_builtin_events(builtin_event_names());
    }
    load_user_lua(&plugins, config_dir.as_deref());
    if let Ok(host) = plugins.lock() {
        let _ = host.emit("startup", "rterm".to_string());
    }

    if let Some(dir) = config_dir.as_ref() {
        let toml_path = explicit_config
            .clone()
            .unwrap_or_else(|| dir.join("config.toml"));
        spawn_watcher(Arc::clone(&plugins), dir.clone(), toml_path);
    }

    let render_test_only = std::env::args().any(|a| a == "--render-test");
    // Resolve the path that backs the running config so `[appearance].theme`
    // changes can be persisted there. Same resolution as load_config: an
    // explicit `--config` wins; otherwise fall back to the default XDG
    // location.
    let config_path = explicit_config
        .clone()
        .or_else(Config::default_path);
    if std::env::args().any(|a| a == "--smoke") {
        run_smoke(&config)
    } else {
        run_gui(
            &config,
            plugins,
            explicit_font_size,
            explicit_font_family,
            render_test_only,
            config_path,
        )
    }
}

fn load_user_lua(plugins: &Arc<Mutex<PluginHost>>, config_dir: Option<&Path>) {
    let dir: PathBuf = match config_dir {
        Some(d) => d.to_path_buf(),
        None => {
            let Some(cfg_path) = Config::default_path() else { return };
            let Some(d) = cfg_path.parent() else { return };
            d.to_path_buf()
        }
    };
    let init_lua = dir.join("init.lua");
    let plugins_dir = dir.join("plugins");
    let Ok(host) = plugins.lock() else { return };
    if init_lua.exists() {
        if let Err(e) = host.load_script(&init_lua) {
            tracing::warn!("init.lua failed: {e:#}");
        } else {
            tracing::info!("init.lua loaded");
        }
    }
    match host.load_dir(&plugins_dir) {
        Ok(n) if n > 0 => tracing::info!(loaded = n, "plugins loaded"),
        Ok(_) => {}
        Err(e) => tracing::warn!("plugin dir scan failed: {e:#}"),
    }
}

fn watched_files(config_dir: &Path, config_toml: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let init_lua = config_dir.join("init.lua");
    if init_lua.exists() {
        out.push(init_lua);
    }
    if config_toml.exists() {
        out.push(config_toml.to_path_buf());
    }
    let plugins_dir = config_dir.join("plugins");
    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("lua") {
                out.push(p);
            }
        }
    }
    out
}

fn spawn_watcher(plugins: Arc<Mutex<PluginHost>>, config_dir: PathBuf, config_toml: PathBuf) {
    thread::spawn(move || {
        let mut state: HashMap<PathBuf, SystemTime> = HashMap::new();
        for p in watched_files(&config_dir, &config_toml) {
            if let Ok(m) = p.metadata().and_then(|m| m.modified()) {
                state.insert(p, m);
            }
        }
        loop {
            thread::sleep(Duration::from_secs(1));
            let current = watched_files(&config_dir, &config_toml);
            let mut lua_changed = false;
            let mut toml_changed = false;
            for p in &current {
                if let Ok(m) = p.metadata().and_then(|m| m.modified()) {
                    if state.get(p) != Some(&m) {
                        state.insert(p.clone(), m);
                        if *p == config_toml {
                            toml_changed = true;
                        } else {
                            lua_changed = true;
                        }
                    }
                }
            }
            // Detect deletions too.
            let cur_set: std::collections::HashSet<_> = current.iter().cloned().collect();
            let removed: Vec<_> = state.keys().filter(|k| !cur_set.contains(*k)).cloned().collect();
            for r in removed {
                if r == config_toml {
                    toml_changed = true;
                } else {
                    lua_changed = true;
                }
                state.remove(&r);
            }
            if toml_changed {
                match Config::load_from(&config_toml) {
                    Ok(cfg) => {
                        // Refresh palette + bold-is-bright live so theme
                        // tweaks take effect without a restart. Mirrors
                        // the startup path: built-in by name first, then
                        // `[colors]` overrides on top.
                        let theme_name = cfg.appearance.theme.trim();
                        if !theme_name.is_empty() {
                            if let Some((_, pal)) =
                                rterm_render::palette::theme_by_name(theme_name)
                            {
                                rterm_render::palette::init_palette(pal);
                            }
                        } else {
                            rterm_render::palette::init_palette(Palette::default());
                        }
                        rterm_render::palette::init_palette(build_palette_over(&cfg.colors));
                        rterm_render::palette::set_bold_is_bright(cfg.font.bold_is_bright);
                        if let Ok(host) = plugins.lock() {
                            // Single batched call — `apply_config_snapshot`
                            // owns the field list, so adding a new hot-
                            // reloadable knob touches one method in
                            // rterm-plugin instead of two sites here.
                            host.apply_config_snapshot(
                                cfg.terminal.scrollback,
                                cfg.terminal.tab_silence_ms,
                                cfg.terminal.cursor_blink,
                                cfg.terminal.show_scrollbar,
                                cfg.terminal.scroll_on_output,
                                cfg.terminal.bell_visual,
                                cfg.terminal.bell_urgent,
                                cfg.terminal.slow_command_ms,
                                if cfg.guake.enabled {
                                    Some((
                                        true,
                                        cfg.guake.position.clone(),
                                        cfg.guake.height_pct,
                                        cfg.guake.width_pct,
                                    ))
                                } else {
                                    None
                                },
                                cfg.font.size,
                                cfg.font.family.clone(),
                                cfg.window.opacity,
                            );
                            let _ = host.emit("reload", "config".to_string());
                            // Let plugins re-style their overlays when the
                            // palette swaps.
                            let _ = host.emit("theme", "config".to_string());
                        }
                        tracing::info!("hot-reloaded config.toml");
                    }
                    Err(e) => tracing::warn!("config.toml reload failed: {e:#}"),
                }
            }
            if lua_changed {
                if let Ok(host) = plugins.lock() {
                    tracing::info!("hot-reloading Lua");
                    host.reset_handlers();
                    let init_lua = config_dir.join("init.lua");
                    if init_lua.exists() {
                        if let Err(e) = host.load_script(&init_lua) {
                            tracing::warn!("init.lua reload failed: {e:#}");
                        }
                    }
                    if let Err(e) = host.load_dir(&config_dir.join("plugins")) {
                        tracing::warn!("plugins reload failed: {e:#}");
                    }
                    let _ = host.emit("reload", "lua".to_string());
                }
            }
        }
    });
}

fn run_gui(
    config: &Config,
    plugins: Arc<Mutex<PluginHost>>,
    font_size_override: Option<f32>,
    font_family_override: Option<String>,
    render_test_only: bool,
    config_path: Option<PathBuf>,
) -> Result<()> {
    // Theme order: 1) built-in by name from `[appearance] theme = "..."`,
    // 2) explicit `[colors]` overrides on top. This lets a user pick a
    // ready-made theme and still tweak individual cells.
    let initial_theme = config.appearance.theme.trim().to_string();
    if !initial_theme.is_empty() {
        if let Some((_, pal)) = rterm_render::palette::theme_by_name(&initial_theme) {
            rterm_render::palette::init_palette(pal);
        } else {
            tracing::warn!(
                theme = %initial_theme,
                "unknown theme name in [appearance].theme — falling back to built-in default"
            );
        }
    }
    // Apply `[colors]` overrides on top of whatever theme is now live.
    rterm_render::palette::init_palette(build_palette_over(&config.colors));
    rterm_render::palette::set_bold_is_bright(config.font.bold_is_bright);

    // Log the resolved font family so a user chasing "uneven character
    // widths" can confirm which face actually got picked (matches the
    // `--list-fonts` diagnostic without forcing them to relaunch).
    let user_family = font_family_override
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| Some(config.font.family.as_str()).filter(|s| !s.trim().is_empty()));
    match user_family {
        Some(name) => tracing::info!(font = name, "using user-pinned font family"),
        None => match rterm_render::default_monospace_family() {
            Some(name) => tracing::info!(font = %name, "resolved default monospace family"),
            None => tracing::warn!(
                "no preferred monospace family installed; falling back to \
                 cosmic-text's built-in choice (character widths may look \
                 uneven). Install e.g. DejaVu Sans Mono or JetBrains Mono."
            ),
        },
    }
    let user_bindings: Vec<UserBinding> = config
        .keybindings
        .iter()
        .filter_map(|kb| match UserBinding::from_config(&kb.keys, &kb.action) {
            Some(b) => Some(b),
            None => {
                tracing::warn!(
                    keys = %kb.keys,
                    action = %kb.action,
                    "ignoring invalid keybinding"
                );
                None
            }
        })
        .collect();
    // Open the persistent command-history database. Failures are
    // non-fatal — disable the feature and warn, so a corrupted /
    // unreadable file can't keep rterm from launching at all.
    // Master kill switch: `[history].enabled = false` skips opening
    // the DB entirely — that disables capture (panes are constructed
    // with `history: None`) AND popup queries (the renderer's
    // refresh path bails when `App.history` is None). Users who want
    // the feature off get neither side-effect.
    let history = if config.history.enabled {
        history_db_path().and_then(|path| match rterm_history::History::open(&path) {
            Ok(h) => Some(Arc::new(Mutex::new(h))),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "could not open command history db — feature disabled this session",
                );
                None
            }
        })
    } else {
        tracing::info!("[history].enabled = false — capture + popup disabled");
        None
    };
    let spawner: Arc<dyn PaneSpawner> = Arc::new(GuiSpawner {
        config: config.clone(),
        history: history.clone(),
    });
    let events: Arc<dyn EventSink> = Arc::new(PluginBridge(plugins));
    let session_path = cache_dir().map(|d| d.join("session.toml"));
    let (session_restore, session_active) = if config.terminal.restore_session {
        session_path
            .as_ref()
            .and_then(|p| read_session(p))
            .unwrap_or_default()
    } else {
        (Vec::new(), None)
    };
    // Persistence callback for `[appearance].theme`. Fires on every
    // theme switch (cycle_theme / settings UI / Lua set_theme). Writes
    // the new name into the user's config.toml so the choice survives
    // restarts. Skipped when we don't know which file backs the running
    // config (e.g. an embedder that loaded Config in-memory).
    let on_theme_change: Option<rterm_render::ThemeChangeCallback> = config_path
        .map(|path| {
            let arc: rterm_render::ThemeChangeCallback =
                Arc::new(move |name: &str| {
                    if let Err(e) = persist_theme_to_config(&path, name) {
                        tracing::warn!(
                            error = %e,
                            theme = %name,
                            path = %path.display(),
                            "failed to persist [appearance].theme to config.toml"
                        );
                    }
                });
            arc
        });
    rterm_render::run(rterm_render::RunConfig {
        title: "rterm".to_string(),
        size: (config.window.width, config.window.height),
        font_size: font_size_override.unwrap_or(config.font.size),
        font_family: font_family_override.unwrap_or_else(|| config.font.family.clone()),
        opacity: config.window.opacity,
        user_bindings,
        spawner,
        events,
        save_scrollback_on_exit: config.terminal.save_scrollback_on_exit,
        scroll_on_output: config.terminal.scroll_on_output,
        cursor_blink: config.terminal.cursor_blink,
        show_scrollbar: config.terminal.show_scrollbar,
        bell_visual: config.terminal.bell_visual,
        bell_urgent: config.terminal.bell_urgent,
        tab_silence_ms: config.terminal.tab_silence_ms,
        slow_command_ms: config.terminal.slow_command_ms,
        session_save: config.terminal.restore_session,
        session_path,
        session_restore,
        session_active,
        render_test_only,
        active_theme: initial_theme,
        on_theme_change,
        os_decorations: config.window.os_decorations,
        allow_osc52: config.terminal.allow_osc52,
        // Pass the [guake] snapshot through unconditionally. The
        // renderer's `toggle_guake` now honours the action regardless
        // of `enabled`, so withholding the user's `[guake]`-section
        // settings here would strand them on hardcoded defaults
        // whenever they bound the action without flipping the flag.
        // The renderer still distinguishes "enabled = true" (silent
        // use) from "enabled = false" (one-time info log on first
        // press) so it stays an opt-in signal.
        guake: Some(rterm_render::GuakeRunConfig {
            enabled: config.guake.enabled,
            position: config.guake.position.clone(),
            height_pct: config.guake.height_pct,
            width_pct: config.guake.width_pct,
            global_hotkey: config.guake.global_hotkey.clone(),
        }),
        history: history.clone(),
        history_popup: rterm_render::HistoryPopupConfig {
            enabled: config.history.enabled,
            popup_rows: config.history.popup_rows,
            popup_debounce_ms: config.history.popup_debounce_ms,
            min_prefix_len: config.history.min_prefix_len,
        },
        paste_confirm: rterm_render::PasteConfirmConfig {
            confirm_multiline: config.paste.confirm_multiline,
            min_bytes: config.paste.confirm_min_bytes,
        },
    })
}

/// Rewrite the user's `config.toml` so that `[appearance].theme = "<name>"`
/// matches `name`. Uses `toml_edit` for in-place updates so the user's
/// hand-written comments / section ordering / blank-line layout survive
/// the rewrite (the previous `toml::to_string_pretty(&cfg)` round-trip
/// wiped them).
///
/// Creates parent directories as needed. Falls back to a full serialise
/// only when the file doesn't exist or fails to parse as a TOML document.
fn persist_theme_to_config(path: &std::path::Path, name: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Existing config: parse via toml_edit, replace just the one value,
    // serialise back. Comments / whitespace preserved.
    if path.exists() {
        if let Ok(body) = std::fs::read_to_string(path) {
            if let Ok(mut doc) = body.parse::<toml_edit::DocumentMut>() {
                // Skip the write when the value is already correct —
                // avoids touching mtime (the file watcher would
                // otherwise see our own save and trigger a reload).
                let current = doc.get("appearance")
                    .and_then(|a| a.get("theme"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if current == name {
                    return Ok(());
                }
                // Ensure `[appearance]` table exists, then set `theme`.
                let appearance = doc
                    .entry("appearance")
                    .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
                if let Some(tbl) = appearance.as_table_mut() {
                    tbl["theme"] = toml_edit::value(name);
                }
                std::fs::write(path, doc.to_string()).with_context(|| {
                    format!("write {} for theme persistence", path.display())
                })?;
                return Ok(());
            }
        }
    }
    // Path missing or unparseable — fall back to a minimal full
    // serialise. The previous code path would have done the same.
    let mut cfg = Config::default();
    cfg.appearance.theme = name.to_string();
    let serialized = toml::to_string_pretty(&cfg).context("serialize config")?;
    std::fs::write(path, serialized).with_context(|| {
        format!("write {} for theme persistence", path.display())
    })?;
    Ok(())
}

/// Parse a saved-session TOML file into `(tabs, active_index?)`. Returns
/// `None` on read / parse failure; an unparseable file means "no restore".
/// Bound applied at restore time. A corrupted or hand-edited session
/// file could otherwise ask us to spawn thousands of PTYs at startup;
/// 256 is well past what anyone reasonably keeps open but small enough
/// to make an accidental DoS impossible. Exported for tests.
const MAX_RESTORE_TABS: usize = 256;

fn parse_session(body: &str) -> Option<(Vec<rterm_render::RestoredTab>, Option<usize>)> {
    #[derive(serde::Deserialize)]
    struct Saved {
        #[serde(default)]
        active: Option<usize>,
        #[serde(default)]
        tab: Vec<SavedTab>,
    }
    #[derive(serde::Deserialize)]
    struct SavedTab {
        cwd: Option<String>,
        title: Option<String>,
    }
    let parsed: Saved = toml::from_str(body).ok()?;
    if parsed.tab.len() > MAX_RESTORE_TABS {
        tracing::warn!(
            count = parsed.tab.len(),
            cap = MAX_RESTORE_TABS,
            "session file claims more tabs than the restore cap; truncating",
        );
    }
    let tabs: Vec<_> = parsed
        .tab
        .into_iter()
        .take(MAX_RESTORE_TABS)
        .map(|t| rterm_render::RestoredTab {
            cwd: t.cwd,
            title: t.title,
        })
        .collect();
    Some((tabs, parsed.active))
}

fn read_session(
    path: &std::path::Path,
) -> Option<(Vec<rterm_render::RestoredTab>, Option<usize>)> {
    let body = std::fs::read_to_string(path).ok()?;
    parse_session(&body)
}

#[cfg(test)]
fn build_palette(c: &rterm_config::ColorsConfig) -> Palette {
    overlay_palette(Palette::default(), c)
}

/// Same as `build_palette`, but starts from the currently-installed
/// palette instead of `Palette::default()`. Used at startup so a chosen
/// `[appearance] theme = "dracula"` survives the `[colors]` overlay
/// (any color the user didn't explicitly override stays from the theme).
fn build_palette_over(c: &rterm_config::ColorsConfig) -> Palette {
    overlay_palette(rterm_render::palette::palette(), c)
}

fn overlay_palette(mut p: Palette, c: &rterm_config::ColorsConfig) -> Palette {
    if let Some(v) = c.fg { p.default_fg = v; }
    if let Some(v) = c.bg { p.default_bg = v; }
    if let Some(v) = c.cursor { p.cursor = Some(v); }
    if let Some(v) = c.black { p.named[0] = v; }
    if let Some(v) = c.red { p.named[1] = v; }
    if let Some(v) = c.green { p.named[2] = v; }
    if let Some(v) = c.yellow { p.named[3] = v; }
    if let Some(v) = c.blue { p.named[4] = v; }
    if let Some(v) = c.magenta { p.named[5] = v; }
    if let Some(v) = c.cyan { p.named[6] = v; }
    if let Some(v) = c.white { p.named[7] = v; }
    if let Some(v) = c.bright_black { p.named[8] = v; }
    if let Some(v) = c.bright_red { p.named[9] = v; }
    if let Some(v) = c.bright_green { p.named[10] = v; }
    if let Some(v) = c.bright_yellow { p.named[11] = v; }
    if let Some(v) = c.bright_blue { p.named[12] = v; }
    if let Some(v) = c.bright_magenta { p.named[13] = v; }
    if let Some(v) = c.bright_cyan { p.named[14] = v; }
    if let Some(v) = c.bright_white { p.named[15] = v; }
    p
}

fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    terminal: SharedTerminal,
    alive: Arc<AtomicBool>,
    activity: Arc<AtomicBool>,
    last_output_ms: Arc<AtomicU64>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // If the renderer thread panicked while holding the
                    // terminal mutex, the lock is poisoned. Earlier this
                    // hard-crashed the reader; instead, log + exit the
                    // thread cleanly so the panel can be reaped via the
                    // usual `alive = false` path. Matches the renderer's
                    // own `lock().ok()` discipline on the other side of
                    // the same mutex.
                    let mut t = match terminal.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            tracing::warn!(
                                "pty reader exiting: terminal mutex poisoned: {e}"
                            );
                            break;
                        }
                    };
                    t.advance(&buf[..n]);
                    activity.store(true, Ordering::Relaxed);
                    last_output_ms.store(now_ms(), Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!("pty read error: {e}");
                    break;
                }
            }
        }
        alive.store(false, Ordering::Release);
        tracing::debug!("pty reader thread exiting");
    })
}

/// Parsed CLI overrides that apply to the live GUI path (the info-only
/// flags like `--version` / `--check` exit before this runs). Each field
/// is `Some` only when explicitly passed; missing flags fall through to
/// the config file's values.
#[derive(Debug, Default, PartialEq)]
struct GuiCliOverrides {
    config: Option<std::path::PathBuf>,
    font_size: Option<f32>,
    font_family: Option<String>,
}

/// Walk an iterator of CLI arguments (sans argv[0]) and pluck out the
/// GUI-affecting overrides. Unknown flags are silently ignored — same
/// behaviour as the previous inline parser — so info-only flags handled
/// in the first-pass loop still slip through this second pass without
/// triggering an error.
fn parse_gui_overrides<I: IntoIterator<Item = String>>(args: I) -> GuiCliOverrides {
    let mut out = GuiCliOverrides::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--config" {
            if let Some(path) = iter.next() {
                out.config = Some(std::path::PathBuf::from(path));
            }
        } else if let Some(p) = arg.strip_prefix("--config=") {
            out.config = Some(std::path::PathBuf::from(p));
        } else if arg == "--font-size" {
            if let Some(v) = iter.next().and_then(|s| s.parse::<f32>().ok()).filter(|v| v.is_finite()) {
                out.font_size = Some(v);
            }
        } else if let Some(v) = arg
            .strip_prefix("--font-size=")
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|v| v.is_finite())
        {
            out.font_size = Some(v);
        } else if arg == "--font-family" {
            if let Some(s) = iter.next() {
                out.font_family = Some(s);
            }
        } else if let Some(s) = arg.strip_prefix("--font-family=") {
            out.font_family = Some(s.to_string());
        }
    }
    out
}

/// Scan a CLI-args iterator for the first `--lang en|ru` (or
/// `--lang=value`) and return the parsed language. Unknown values and
/// missing arguments yield `None` so the caller can fall back to
/// `CommentLang::detect()`. Used only by `--print-default-config`.
fn parse_lang_flag<I: IntoIterator<Item = String>>(args: I) -> Option<rterm_config::CommentLang> {
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--lang" {
            if let Some(v) = iter.next() {
                return rterm_config::CommentLang::parse(&v);
            }
        } else if let Some(v) = arg.strip_prefix("--lang=") {
            return rterm_config::CommentLang::parse(v);
        }
    }
    None
}

fn resolve_shell(config: &Config) -> (String, Vec<String>) {
    // Honour an explicit shell program, but treat an empty string as
    // "unset" — the bundled `default.toml` shows `# program = ""` as
    // a commented hint, and a user accidentally uncommenting that line
    // would otherwise try to spawn "" (PTY error). Matches what
    // `--print-config` users see and round-trip with.
    if let Some(program) = &config.shell.program {
        if !program.trim().is_empty() {
            return (program.clone(), config.shell.args.clone());
        }
    }
    #[cfg(windows)]
    {
        ("powershell.exe".into(), vec![])
    }
    #[cfg(not(windows))]
    {
        let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        (sh, vec!["-i".into()])
    }
}

fn run_smoke(_config: &Config) -> Result<()> {
    let size = Size { cols: 80, rows: 24 };
    let mut term = Terminal::new(size);

    let (program, args) = smoke_command();
    let mut pty = Pty::spawn(&program, &args, size, None)?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut reader = pty.try_clone_reader()?;
    let reader_thread = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut exit_status = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => term.advance(&chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(s) = pty.try_wait()? {
                    exit_status = Some(s);
                    while let Ok(chunk) = rx.try_recv() {
                        term.advance(&chunk);
                    }
                    break;
                }
                if Instant::now() >= deadline {
                    tracing::warn!("smoke deadline reached");
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Reader thread may have exited via EOF (Disconnected branch) or
    // we may have broken without seeing the exit status above. Try one
    // more time so the smoke summary reflects the real exit code when
    // the shell finished cleanly.
    if exit_status.is_none() {
        if let Ok(Some(s)) = pty.try_wait() {
            exit_status = Some(s);
        }
    }
    let _ = pty.kill();
    let _ = reader_thread.join();

    let first_line: String = (0..term.size().cols)
        .filter_map(|c| term.grid().cell(Position { col: c, row: 0 }).map(|cell| cell.ch))
        .collect();
    let cursor = term.cursor();
    // Include the resolved monospace family in the smoke output so CI
    // logs surface "did we pick the right font" without needing
    // `--list-fonts` separately. Empty string = cosmic-text built-in
    // fallback (no preferred monospace installed).
    let font = rterm_render::default_monospace_family().unwrap_or_default();
    // Exit status of the spawned shell. CI can grep for `exit=0` to
    // distinguish a clean shell run from a deadline-killed one.
    let exit_repr = exit_status
        .map(|s| {
            if s.success() {
                "0".to_string()
            } else {
                format!("{}", s.exit_code())
            }
        })
        .unwrap_or_else(|| "timeout".to_string());
    // Surface the resolved shell command so CI can tell apart "default
    // payload" from "RTERM_SMOKE_COMMAND took effect". The args vec is
    // typically `["-c", "<payload>"]` on Unix or `["/C", "<payload>"]`
    // on Windows; printing the last element captures the payload
    // without the leading shell-spawn flag.
    let payload = args.last().cloned().unwrap_or_default();
    // `--smoke --json` switches to a single-line JSON object so CI
    // pipelines can assert on individual fields without parsing the
    // labelled text form.
    if std::env::args().any(|a| a == "--json") {
        let obj = serde_json::json!({
            "cols": term.size().cols,
            "rows": term.size().rows,
            "cursor": [cursor.row, cursor.col],
            "font": font,
            "exit": exit_repr,
            "shell": program,
            "payload": payload,
            "row0": first_line.trim_end(),
        });
        println!("{obj}");
        return Ok(());
    }
    println!(
        "rterm headless OK ({}x{}); cursor=({},{}); font={:?}; exit={}; shell={:?} payload={:?}; row 0: {:?}",
        term.size().cols,
        term.size().rows,
        cursor.row,
        cursor.col,
        font,
        exit_repr,
        program,
        payload,
        first_line.trim_end()
    );
    Ok(())
}

/// Canonical list of plugin event names. Surfaced via the `--list-events`
/// CLI and `rterm.builtin_events()` Lua API. Keep in sync with `emit(...)`
/// call sites across rterm-render and rterm-core.
/// Return the bare positional argument that immediately follows `flag`
/// in `args`, or `None` if there isn't one or the following token is
/// itself a `--`-prefixed flag. Used by the `--list-*` info flags to
/// accept an optional prefix filter without complicating the outer
/// arg-loop with two-token consumption rules. The two-arg form
/// (`arg_after_flag_in`) is testable; the wrapper hits the process env.
fn arg_after_flag_in<I: IntoIterator<Item = String>>(
    args: I,
    flag: &str,
) -> Option<String> {
    args.into_iter()
        .skip_while(|a| a != flag)
        .nth(1)
        .filter(|p| !p.starts_with("--"))
}

fn arg_after_flag(flag: &str) -> Option<String> {
    arg_after_flag_in(std::env::args(), flag)
}

/// Cheap "did you mean?" suggestion for a misspelled font family. Walks
/// `installed` looking for a case-insensitive substring overlap in
/// either direction (so `"jetbrains"` finds `"JetBrains Mono"`, and
/// `"JetBrainsMono Nerd Font Mono"` finds `"JetBrainsMono Nerd Font"`).
/// Ties broken by shortest installed name — usually the more general
/// face the user likely intended. Returns `None` when no overlap.
fn closest_font_match(want: &str, installed: &[String]) -> Option<String> {
    let want_l = want.to_lowercase();
    if want_l.is_empty() {
        return None;
    }
    // Pre-lowercase each installed name ONCE so the per-pair check
    // does substring-find on cached strings instead of re-lowering
    // the same name for every haystack pass. The list is short
    // (~tens to low hundreds), allocation is fine and one-shot.
    let installed_l: Vec<(usize, String)> =
        installed.iter().map(|f| (f.len(), f.to_lowercase())).collect();
    installed
        .iter()
        .zip(installed_l.iter())
        .filter(|(_, (_, fl))| fl.contains(&want_l) || want_l.contains(fl))
        .min_by_key(|(_, (len, _))| *len)
        .map(|(orig, _)| orig.clone())
}

/// Return human-readable warnings for any `Config` values that fall
/// outside the runtime clamp ranges the renderer silently corrects. The
/// renderer is forgiving — a borked value lands at the boundary rather
/// than crashing — but the user typically wants to know their input was
/// ignored. `--check` surfaces these so CI catches a typo before the
/// user runs the GUI and goes "my opacity setting isn't doing anything".
fn config_range_warnings(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if !cfg.font.size.is_finite() || cfg.font.size < 6.0 || cfg.font.size > 96.0 {
        out.push(format!(
            "font.size = {} outside renderer range [6.0, 96.0]; will be clamped",
            cfg.font.size,
        ));
    }
    if !cfg.window.opacity.is_finite()
        || !(0.0..=1.0).contains(&cfg.window.opacity)
    {
        out.push(format!(
            "window.opacity = {} outside [0.0, 1.0]; will be clamped",
            cfg.window.opacity,
        ));
    } else if cfg.window.opacity > 0.0 && cfg.window.opacity < 0.1 {
        // Inside the valid range but below the readability floor —
        // a typo (`0.05` instead of `0.5`) leaves the window nearly
        // invisible, the user can't see their config to fix it.
        // 0.0 is explicitly NOT flagged: it's the "fully click-through
        // overlay" sentinel some compositors actually want.
        out.push(format!(
            "window.opacity = {} below practical floor 0.1; window \
             will be nearly invisible",
            cfg.window.opacity,
        ));
    }
    if cfg.window.width < 320 || cfg.window.height < 200 {
        out.push(format!(
            "window.width × window.height = {}×{} below renderer minimum \
             320×200; will be enlarged at startup",
            cfg.window.width, cfg.window.height,
        ));
    }
    // Threshold sanity: 0 is the documented "disable" value, so
    // allow it without complaint. A positive value below 100ms
    // is almost certainly a typo (the event would fire constantly
    // and overwhelm plugin handlers); flag it so the user notices
    // before runtime spam appears.
    if cfg.terminal.tab_silence_ms > 0 && cfg.terminal.tab_silence_ms < 100 {
        out.push(format!(
            "terminal.tab_silence_ms = {} below practical floor 100ms; \
             events will fire on essentially every frame",
            cfg.terminal.tab_silence_ms,
        ));
    }
    if cfg.terminal.slow_command_ms > 0 && cfg.terminal.slow_command_ms < 100 {
        out.push(format!(
            "terminal.slow_command_ms = {} below practical floor 100ms; \
             nearly every command will be flagged as slow",
            cfg.terminal.slow_command_ms,
        ));
    }
    // Scrollback is bounded only by available memory. 1_000_000
    // lines × ~200 bytes per line ≈ 200 MB per pane — pathological
    // for an interactive workflow, almost always a typo. Warn so
    // the user can drop a trailing zero.
    if cfg.terminal.scrollback > 1_000_000 {
        out.push(format!(
            "terminal.scrollback = {} is pathologically large (>1M lines); \
             expect hundreds of MB of resident memory per pane",
            cfg.terminal.scrollback,
        ));
    }
    out
}

fn builtin_event_names() -> Vec<String> {
    [
        "startup", "shutdown", "reload", "theme",
        "key", "ready", "frame.tick",
        "tab.new", "tab.close", "tab.switch", "tab.activity", "tab.silence", "tab.title", "tab.move",
        "tab.alt_enter", "tab.alt_leave", "tab.progress", "tab.unread", "tab.read",
        "tab.drag_start", "tab.drag_end",
        "pane.focus", "pane.close", "pane.exit", "pane.split", "pane.swap",
        "pane.output", "pane.title", "pane.cwd", "pane.silence", "pane.resize",
        "pane.alt_enter", "pane.alt_leave", "pane.zoom", "pane.bell_mute",
        "pane.cursor_shape", "pane.cursor_blink", "pane.cursor_visible",
        "pane.mouse_mode", "pane.reverse_screen",
        "pane.scrollback_enter", "pane.scrollback_leave",
        "cwd", "title", "resize", "window.resize", "window.focus",
        "scroll", "selection.end",
        "paste", "copy", "link.open", "link.blocked", "link.hover", "link.unhover",
        "search.start", "search.end", "search.step",
        "palette.open", "palette.close",
        "output.line", "match", "shell.exit", "pane.shell_exit",
        "pane.command_finish", "pane.slow_command",
        "scrollback.save", "scrollback.clear",
        "prompt.jump", "command.jump",
        "bell", "notification", "progress",
        "osc52.write", "osc52.blocked",
        // Fires when `toggle_guake` is invoked while `[guake] enabled =
        // false`. Lets a plugin convert what used to be a silent no-op
        // into a toast / settings-overlay nudge so first-time users
        // don't think their binding is broken.
        "guake.disabled",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn smoke_command() -> (String, Vec<String>) {
    // `RTERM_SMOKE_COMMAND` overrides the default `echo hello rterm`
    // payload so CI / integration tests can drive the headless run
    // against arbitrary input without forking the binary. The value
    // is passed verbatim to `sh -c` / `cmd /C` — the same shell the
    // default uses.
    let custom = std::env::var("RTERM_SMOKE_COMMAND").ok().filter(|s| !s.is_empty());
    let payload = custom.unwrap_or_else(|| "echo hello rterm".to_string());
    #[cfg(windows)]
    {
        ("cmd.exe".into(), vec!["/C".into(), payload])
    }
    #[cfg(not(windows))]
    {
        ("/bin/sh".into(), vec!["-c".into(), payload])
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    // Default filter silences `request_user_attention isn't supported` from
    // winit on Wayland (logged on every bell when the window is unfocused)
    // since it's a known platform limitation, not an rterm bug. Users can
    // override via `RUST_LOG=info` or `RUST_LOG=winit=warn` if they need
    // to debug winit issues.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // Silence the GPU stack's loader-enumeration chatter (hundreds of
        // INFO lines on Linux that delay the GUI window appearing) and the
        // Wayland `request_user_attention` warn. Render-bug debugging:
        // override with `RUST_LOG=wgpu_hal=info`.
        //
        // `sctk_adwaita::config=off` mutes a noisy startup ERROR on
        // Wayland environments where the XDG settings portal doesn't
        // respond within 100 ms — not actionable for the user, and the
        // window still opens fine (sctk_adwaita just falls back to
        // built-in defaults for the title-bar accent colour).
        EnvFilter::new(
            "info,\
             winit::platform_impl::linux::wayland::window=error,\
             wgpu_hal=warn,\
             wgpu_core=warn,\
             sctk_adwaita=warn,\
             sctk_adwaita::config=off,\
             naga=warn",
        )
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_caps_huge_inputs() {
        // Build a session body with 5× the restore cap and confirm we
        // never hand out more than MAX_RESTORE_TABS entries — that's the
        // DoS bound a malicious / corrupted file can't push past.
        let mut body = String::from("active = 0\n");
        for i in 0..(MAX_RESTORE_TABS * 5) {
            body.push_str(&format!("[[tab]]\ncwd = \"/tmp/{i}\"\n\n"));
        }
        let (tabs, _) = parse_session(&body).expect("valid toml parses");
        assert_eq!(tabs.len(), MAX_RESTORE_TABS);
        // The truncation keeps the *first* N tabs (oldest), so the cwd of
        // the last entry reflects the cap, not the input length.
        assert_eq!(
            tabs.last().and_then(|t| t.cwd.as_deref()),
            Some(format!("/tmp/{}", MAX_RESTORE_TABS - 1).as_str()),
        );
    }

    #[test]
    fn parse_session_round_trip_with_active() {
        // Realistic two-tab session, with the second tab marked active
        // and a custom title. Pin the field-by-field mapping so a serde
        // rename can't break restore silently.
        let body = r#"
            active = 1

            [[tab]]
            cwd = "/tmp/a"

            [[tab]]
            cwd = "/tmp/b"
            title = "build"
        "#;
        let (tabs, active) = parse_session(body).expect("parses");
        assert_eq!(active, Some(1));
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].cwd.as_deref(), Some("/tmp/a"));
        assert_eq!(tabs[0].title, None);
        assert_eq!(tabs[1].cwd.as_deref(), Some("/tmp/b"));
        assert_eq!(tabs[1].title.as_deref(), Some("build"));
    }

    #[test]
    fn parse_session_handles_empty_input() {
        // Empty file = no tabs, no active. Malformed = None.
        let (tabs, active) = parse_session("").expect("empty parses");
        assert!(tabs.is_empty());
        assert!(active.is_none());
        assert!(parse_session("not = [valid = toml").is_none());
    }

    #[test]
    fn build_palette_maps_every_color_slot() {
        // Easy to accidentally skip a slot when copy-pasting these 18
        // `if let Some` lines, and the user's custom colour would
        // silently fall back to the default. Catch that by setting a
        // unique sentinel per slot and asserting each one lands.
        let cfg = rterm_config::ColorsConfig {
            fg: Some([1, 0, 0]),
            bg: Some([2, 0, 0]),
            cursor: Some([3, 0, 0]),
            black: Some([10, 0, 0]),
            red: Some([11, 0, 0]),
            green: Some([12, 0, 0]),
            yellow: Some([13, 0, 0]),
            blue: Some([14, 0, 0]),
            magenta: Some([15, 0, 0]),
            cyan: Some([16, 0, 0]),
            white: Some([17, 0, 0]),
            bright_black: Some([20, 0, 0]),
            bright_red: Some([21, 0, 0]),
            bright_green: Some([22, 0, 0]),
            bright_yellow: Some([23, 0, 0]),
            bright_blue: Some([24, 0, 0]),
            bright_magenta: Some([25, 0, 0]),
            bright_cyan: Some([26, 0, 0]),
            bright_white: Some([27, 0, 0]),
        };
        let p = build_palette(&cfg);
        assert_eq!(p.default_fg, [1, 0, 0]);
        assert_eq!(p.default_bg, [2, 0, 0]);
        assert_eq!(p.cursor, Some([3, 0, 0]));
        let expected_named = [
            [10, 0, 0], [11, 0, 0], [12, 0, 0], [13, 0, 0],
            [14, 0, 0], [15, 0, 0], [16, 0, 0], [17, 0, 0],
            [20, 0, 0], [21, 0, 0], [22, 0, 0], [23, 0, 0],
            [24, 0, 0], [25, 0, 0], [26, 0, 0], [27, 0, 0],
        ];
        assert_eq!(p.named, expected_named);
    }

    #[test]
    fn parse_gui_overrides_supports_both_separator_styles() {
        // Both `--flag value` and `--flag=value` are documented in
        // `--help`; the parser must accept either form per flag.
        let argv = ["--config", "/tmp/c.toml", "--font-size=14", "--font-family", "JetBrains Mono"];
        let out = parse_gui_overrides(argv.iter().map(|s| s.to_string()));
        assert_eq!(out.config, Some(std::path::PathBuf::from("/tmp/c.toml")));
        assert_eq!(out.font_size, Some(14.0));
        assert_eq!(out.font_family.as_deref(), Some("JetBrains Mono"));

        let argv = ["--config=/tmp/c.toml", "--font-size", "12.5", "--font-family=Fira Code"];
        let out = parse_gui_overrides(argv.iter().map(|s| s.to_string()));
        assert_eq!(out.config, Some(std::path::PathBuf::from("/tmp/c.toml")));
        assert_eq!(out.font_size, Some(12.5));
        assert_eq!(out.font_family.as_deref(), Some("Fira Code"));
    }

    #[test]
    fn parse_gui_overrides_silently_skips_unknown_flags() {
        // Unknown flags don't error — the same iterator is run BOTH for
        // info-only flags (`--version`, `--check`) in the first pass and
        // for overrides here. Erroring on the latter would break the
        // former. Trailing partial flags (`--font-size` without a value)
        // are also tolerated.
        let argv = [
            "--unknown",
            "--config",
            "/tmp/c.toml",
            "--also-unknown=foo",
            "--font-size", // missing value at end
        ];
        let out = parse_gui_overrides(argv.iter().map(|s| s.to_string()));
        assert_eq!(out.config, Some(std::path::PathBuf::from("/tmp/c.toml")));
        assert_eq!(out.font_size, None, "trailing flag without value drops");
        assert_eq!(out.font_family, None);
    }

    #[test]
    fn parse_gui_overrides_rejects_non_finite_font_size() {
        // `f32::clamp(NaN, ..)` panics in Rust, so a `--font-size NaN`
        // (or `inf`) would crash startup. Guard the parse path so the
        // bogus value drops to None and we fall through to the config
        // / default. Tested for both NaN and Infinity, both separators.
        for v in ["NaN", "inf", "-inf"] {
            let argv = ["--font-size", v];
            let out = parse_gui_overrides(argv.iter().map(|s| s.to_string()));
            assert_eq!(out.font_size, None, "`--font-size {v}` should reject");

            let inline = [format!("--font-size={v}")];
            let out = parse_gui_overrides(inline);
            assert_eq!(out.font_size, None, "`--font-size={v}` should reject");
        }
        // Non-numeric junk also drops to None (parse() returns Err).
        for v in ["garbage", "12px", "abc"] {
            let argv = ["--font-size", v];
            let out = parse_gui_overrides(argv.iter().map(|s| s.to_string()));
            assert_eq!(out.font_size, None, "junk `--font-size {v}` should reject");
        }
        // Finite values pass through, including negatives — the
        // renderer clamps to [6.0, 96.0] on use and `--check` flags
        // out-of-range values for config sources separately.
        let argv = ["--font-size", "16.0"];
        let out = parse_gui_overrides(argv.iter().map(|s| s.to_string()));
        assert_eq!(out.font_size, Some(16.0));
        let argv = ["--font-size=-5"];
        let out = parse_gui_overrides(argv.iter().map(|s| s.to_string()));
        assert_eq!(out.font_size, Some(-5.0));
    }

    #[test]
    fn parse_gui_overrides_returns_default_on_empty() {
        let out = parse_gui_overrides(std::iter::empty::<String>());
        assert_eq!(out, GuiCliOverrides::default());
    }

    #[test]
    fn resolve_shell_treats_empty_as_unset() {
        // The bundled default.toml shows `# program = ""` as a hint —
        // a user accidentally uncommenting it should NOT crash the PTY
        // spawn. Falls through to the platform default.
        let mut cfg = Config::default();
        cfg.shell.program = Some(String::new());
        let (program, _args) = resolve_shell(&cfg);
        assert!(!program.is_empty(), "fell through to platform default");
        // Whitespace-only also gets the fall-through.
        cfg.shell.program = Some("   ".to_string());
        let (program, _args) = resolve_shell(&cfg);
        assert!(!program.is_empty());
        // An actual program string is honoured verbatim.
        cfg.shell.program = Some("/bin/zsh".to_string());
        cfg.shell.args = vec!["-l".to_string()];
        let (program, args) = resolve_shell(&cfg);
        assert_eq!(program, "/bin/zsh");
        assert_eq!(args, vec!["-l".to_string()]);
    }

    #[test]
    fn build_palette_default_preserves_baseline() {
        // Empty ColorsConfig (every field None) must leave the
        // Palette::default() values intact, so users who don't set any
        // colour keys still get the curated xterm-ish defaults.
        let cfg = rterm_config::ColorsConfig::default();
        let p = build_palette(&cfg);
        let baseline = Palette::default();
        assert_eq!(p.default_fg, baseline.default_fg);
        assert_eq!(p.default_bg, baseline.default_bg);
        assert_eq!(p.cursor, baseline.cursor);
        assert_eq!(p.named, baseline.named);
    }

    #[test]
    fn arg_after_flag_returns_optional_prefix_or_none() {
        let mk = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Flag present, followed by a bare arg → that arg.
        assert_eq!(
            arg_after_flag_in(mk(&["rterm", "--list-events", "pane."]), "--list-events"),
            Some("pane.".to_string()),
        );
        // Flag present at the end → no following arg.
        assert_eq!(
            arg_after_flag_in(mk(&["rterm", "--list-events"]), "--list-events"),
            None,
        );
        // Flag followed by another flag → no prefix (filter rejects
        // `--`-prefixed tokens so `--list-events --version` doesn't
        // accidentally use `--version` as a filter).
        assert_eq!(
            arg_after_flag_in(
                mk(&["rterm", "--list-events", "--config", "/tmp/x"]),
                "--list-events",
            ),
            None,
        );
        // Flag missing → None.
        assert_eq!(
            arg_after_flag_in(mk(&["rterm", "--version"]), "--list-events"),
            None,
        );
        // Flag in the middle, prefix after it (other args before / after
        // shouldn't interfere).
        assert_eq!(
            arg_after_flag_in(
                mk(&["rterm", "--config", "/tmp/c", "--list-actions", "opacity_"]),
                "--list-actions",
            ),
            Some("opacity_".to_string()),
        );
    }

    #[test]
    fn cli_json_modes_escape_control_bytes_correctly() {
        // After the switch to `serde_json`, every `--json` CLI mode
        // routes user-supplied strings through `serde_json::json!`
        // which DOES escape control bytes. Pin the contract for the
        // smoke-mode struct so a regression to hand-rolled escaping
        // can't sneak in.
        let obj = serde_json::json!({
            "payload": "line\nwith\ttabs\"and quotes",
            "shell":   "back\\slash",
            "emoji":   "\u{1F600}",
        });
        let s = obj.to_string();
        // Round-trip via the parser — it MUST accept the string we
        // just produced. The old hand-rolled helper only escaped
        // `\\` and `"`, so `\n`/`\t` would have produced invalid
        // JSON and this `from_str` would fail.
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        assert_eq!(parsed["payload"], "line\nwith\ttabs\"and quotes");
        assert_eq!(parsed["shell"], "back\\slash");
        assert_eq!(parsed["emoji"], "\u{1F600}");
    }

    #[test]
    fn closest_font_match_suggests_substring_overlap() {
        let installed: Vec<String> = [
            "JetBrains Mono",
            "JetBrainsMono Nerd Font",
            "Fira Code",
            "DejaVu Sans Mono",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Lowercase / partial match → shortest installed wins.
        assert_eq!(
            closest_font_match("jetbrains", &installed).as_deref(),
            Some("JetBrains Mono"),
        );
        // Substring in reverse direction (typed longer than installed).
        assert_eq!(
            closest_font_match("Fira Code Nerd Font", &installed).as_deref(),
            Some("Fira Code"),
        );
        // No overlap → None.
        assert_eq!(
            closest_font_match("Cascadia Mono", &installed),
            None,
        );
        // Empty input → None.
        assert_eq!(closest_font_match("", &installed), None);
    }

    #[test]
    fn config_range_warnings_flags_clampable_values_and_passes_defaults() {
        // The renderer silently clamps borked config values. --check
        // calls into this helper to flag them so the user knows their
        // input was ignored. Pin the boundary cases so a future refactor
        // can't accidentally pass through (e.g. a NaN that would panic
        // the renderer further down).
        let mut cfg = Config::default();
        assert!(
            config_range_warnings(&cfg).is_empty(),
            "default config must produce zero warnings",
        );

        // Negative font size → flagged.
        cfg.font.size = -1.0;
        let warnings = config_range_warnings(&cfg);
        assert!(
            warnings.iter().any(|w| w.contains("font.size")),
            "expected font.size warning, got {:?}",
            warnings,
        );

        // NaN opacity → flagged.
        cfg = Config::default();
        cfg.window.opacity = f32::NAN;
        let warnings = config_range_warnings(&cfg);
        assert!(
            warnings.iter().any(|w| w.contains("window.opacity")),
            "expected window.opacity warning, got {:?}",
            warnings,
        );

        // Too-small window → flagged.
        cfg = Config::default();
        cfg.window.width = 100;
        cfg.window.height = 50;
        let warnings = config_range_warnings(&cfg);
        assert!(
            warnings.iter().any(|w| w.contains("window.width")),
            "expected window size warning, got {:?}",
            warnings,
        );

        // 1.5 opacity, 200pt font → multiple warnings in one config.
        cfg = Config::default();
        cfg.font.size = 200.0;
        cfg.window.opacity = 1.5;
        assert_eq!(config_range_warnings(&cfg).len(), 2);

        // tab_silence_ms = 0 is the disable sentinel → no warning.
        cfg = Config::default();
        cfg.terminal.tab_silence_ms = 0;
        assert!(
            !config_range_warnings(&cfg)
                .iter()
                .any(|w| w.contains("tab_silence_ms")),
            "0 is the disable sentinel and must not be flagged",
        );

        // tab_silence_ms below the 100ms floor → flagged.
        cfg.terminal.tab_silence_ms = 50;
        let warnings = config_range_warnings(&cfg);
        assert!(
            warnings.iter().any(|w| w.contains("tab_silence_ms")),
            "expected tab_silence_ms floor warning, got {:?}",
            warnings,
        );

        // slow_command_ms below the 100ms floor → flagged. 0 disabled.
        cfg = Config::default();
        cfg.terminal.slow_command_ms = 0;
        assert!(!config_range_warnings(&cfg)
            .iter()
            .any(|w| w.contains("slow_command_ms")));
        cfg.terminal.slow_command_ms = 50;
        let warnings = config_range_warnings(&cfg);
        assert!(
            warnings.iter().any(|w| w.contains("slow_command_ms")),
            "expected slow_command_ms floor warning, got {:?}",
            warnings,
        );

        // Pathologically large scrollback → flagged.
        cfg = Config::default();
        cfg.terminal.scrollback = 2_000_000;
        let warnings = config_range_warnings(&cfg);
        assert!(
            warnings.iter().any(|w| w.contains("scrollback")),
            "expected scrollback warning, got {:?}",
            warnings,
        );

        // window.opacity inside [0.0, 1.0] but below the practical
        // readability floor (0.1) → flagged with a "nearly invisible"
        // message. 0.0 is the explicit pass-through sentinel and must
        // not be flagged.
        cfg = Config::default();
        cfg.window.opacity = 0.0;
        assert!(
            !config_range_warnings(&cfg)
                .iter()
                .any(|w| w.contains("nearly invisible")),
            "opacity 0.0 is a pass-through sentinel, not a typo",
        );
        cfg.window.opacity = 0.05;
        let warnings = config_range_warnings(&cfg);
        assert!(
            warnings.iter().any(|w| w.contains("nearly invisible")),
            "expected sub-floor opacity warning, got {:?}",
            warnings,
        );
        // 0.1 is the inclusive lower bound — must not warn.
        cfg.window.opacity = 0.1;
        assert!(!config_range_warnings(&cfg)
            .iter()
            .any(|w| w.contains("nearly invisible")));
    }

    #[test]
    fn default_toml_template_mentions_every_canonical_action() {
        // The `# Actions:` comment in the auto-generated `config.toml`
        // is what first-run users skim to learn what they can bind.
        // When a new action is added without updating this comment, the
        // user has no on-disk record that it exists (and
        // `--list-actions` is easy to miss). Pin the contract for every
        // shipped comment language so a future variant addition fails
        // the test until BOTH templates are updated alongside —
        // otherwise a `ru_RU` user would silently lose visibility into
        // newer actions even after the EN template gets the comment.
        let names = rterm_render::AppAction::canonical_names();
        for lang in [
            rterm_config::CommentLang::En,
            rterm_config::CommentLang::Ru,
        ] {
            let template = rterm_config::default_template_for(lang);
            for name in &names {
                assert!(
                    template.contains(name),
                    "{lang:?} default-template missing canonical action {name:?}",
                );
            }
        }
    }

    #[test]
    fn builtin_event_names_have_no_duplicates() {
        // Plugins register on names like "pane.output". If the list ever
        // contained the same name twice, `--list-events` and the Lua
        // `rterm.builtin_events()` getter would advertise it as two
        // separate events — confusing, and a sign of a sloppy edit.
        let names = builtin_event_names();
        let mut sorted = names.clone();
        sorted.sort();
        let dedup = {
            let mut d = sorted.clone();
            d.dedup();
            d
        };
        assert_eq!(
            sorted.len(),
            dedup.len(),
            "duplicate names in builtin_event_names: {:?}",
            sorted
                .windows(2)
                .filter_map(|w| if w[0] == w[1] { Some(&w[0]) } else { None })
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn builtin_events_include_core_lifecycle_and_silence() {
        // `tab.silence` is an edge-triggered event consumers rely on for
        // "ping me when this finishes" plugins. If it (or any of the other
        // anchor names) vanishes from the surface API, a plugin that
        // registered for it via `rterm.on(...)` silently stops firing — so
        // pin the list here as the authoritative source.
        let names = builtin_event_names();
        for must_have in [
            "startup", "shutdown", "ready", "reload", "theme", "key",
            "tab.new", "tab.close", "tab.switch", "tab.move",
            "tab.activity", "tab.silence", "tab.title",
            "tab.alt_enter", "tab.alt_leave",
            "tab.progress", "tab.unread", "tab.read",
            "tab.drag_start", "tab.drag_end",
            "pane.split", "pane.close", "pane.exit", "pane.focus", "pane.cwd",
            "pane.swap", "pane.zoom", "pane.bell_mute",
            "pane.alt_enter", "pane.alt_leave",
            "pane.output", "pane.title", "pane.silence", "pane.resize",
            "pane.cursor_shape", "pane.cursor_blink", "pane.cursor_visible",
            "pane.mouse_mode", "pane.reverse_screen",
            "pane.scrollback_enter", "pane.scrollback_leave",
            "pane.command_finish", "pane.slow_command",
            "match", "output.line", "shell.exit", "pane.shell_exit",
            "search.start", "search.end", "search.step",
            "palette.open", "palette.close",
            "prompt.jump", "command.jump",
            "link.open", "link.blocked", "link.hover", "link.unhover",
            "window.focus", "window.resize",
            "scroll", "selection.end", "progress",
            "scrollback.save", "scrollback.clear",
            "copy", "paste", "osc52.write", "osc52.blocked",
            "frame.tick", "cwd", "title", "resize",
            "bell", "notification",
            // Surfaced by `toggle_guake` when `[guake] enabled = false`.
            // Plugins use this to convert what would otherwise be a
            // silent no-op into a toast / settings-overlay nudge —
            // dropping the entry would silently break those handlers.
            "guake.disabled",
        ] {
            assert!(
                names.iter().any(|n| n == must_have),
                "builtin event list missing {must_have:?}; got {names:?}",
            );
        }
    }

    #[test]
    fn plugin_theme_names_match_renderer_builtins() {
        // The plugin crate hardcodes the list of valid theme names so
        // it can validate `rterm.set_theme(name)` without a circular
        // dep on rterm-render. Pin both lists to the same canonical
        // order here so adding a theme to one without the other fails
        // loudly.
        let renderer: Vec<&str> = rterm_render::palette::builtin_themes()
            .iter()
            .map(|(n, _)| *n)
            .collect();
        let host = rterm_plugin::PluginHost::new().expect("PluginHost::new");
        let plugin = host.known_theme_names();
        let plugin: Vec<&str> = plugin.iter().map(String::as_str).collect();
        assert_eq!(renderer, plugin, "renderer and plugin theme tables disagree");
    }

    #[test]
    fn persist_theme_round_trips_via_appearance_table() {
        // Write a theme, parse it back, assert the field made the round
        // trip. Covers the persist path used after every `cycle_theme`.
        let dir = std::env::temp_dir().join("rterm-test-persist-theme");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        super::persist_theme_to_config(&path, "dracula").unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.appearance.theme, "dracula");

        // A second call to the same name should be a no-op (early return).
        super::persist_theme_to_config(&path, "dracula").unwrap();
        // And switching writes the new name.
        super::persist_theme_to_config(&path, "nord").unwrap();
        let cfg2 = Config::load_from(&path).unwrap();
        assert_eq!(cfg2.appearance.theme, "nord");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn persist_theme_preserves_existing_comments_and_sections() {
        // Seed the file with a hand-written config: comments, ordering,
        // unrelated sections. After persist_theme, those must survive
        // verbatim — only the `theme = "..."` value changes.
        let dir = std::env::temp_dir().join("rterm-test-persist-preserve");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original = r#"
# user-authored header comment that must survive
[font]
family = "Fira Code"  # inline comment
size = 14.0

# Window section — keep me too.
[window]
opacity = 0.85

[appearance]
# previous theme below
theme = "dracula"

[terminal]
scrollback = 50000
"#;
        std::fs::write(&path, original).unwrap();
        super::persist_theme_to_config(&path, "nord").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# user-authored header comment that must survive"));
        assert!(after.contains("# inline comment"));
        assert!(after.contains("# Window section — keep me too."));
        assert!(after.contains("# previous theme below"));
        assert!(after.contains("scrollback = 50000"));
        // The value MUST be updated.
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.appearance.theme, "nord");
        std::fs::remove_file(&path).ok();
    }
}
