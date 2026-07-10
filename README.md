# rterm

Cross-platform GPU-accelerated terminal emulator written in Rust, with a Lua
plugin host.

## Status

Working MVP. The full pipeline runs end-to-end: PTY → ANSI/VT state machine →
GPU renderer → window. Tabs and BSP-split panes are wired. Lua plugins observe
~70 events and call ~50 actions. Workspace ships **400+ unit tests**,
`cargo clippy -- -D warnings` is clean.

## Target platforms

- **Linux** (X11 + Wayland) via winit + wgpu (Vulkan/GL)
- **macOS** via winit + wgpu (Metal)
- **Windows** (ConPTY) via winit + wgpu (DX12)
- **WSL2** — autodetected; defaults to GL backend + Fifo present mode to avoid
  Mesa Vulkan stalls. Override with `WGPU_BACKEND` / `WGPU_PRESENT_MODE`.

## Features

- Full VT/ANSI parser: SGR (16/256/truecolor + underline styles + reverse +
  blink + strike/overline), cursor motion (CUU/CUD/CUF/CUB/CUP/HVP/CHA/VPA
  /CNL/CPL), erase (ED/EL/ICH/DCH/IL/DL/ECH), scrolling (SU/SD/RI/IND/NEL),
  save/restore (DECSC/DECRC + CSI s/u + ?1048), DECSET/DECRST including
  alt-screen, bracketed paste, mouse tracking (X10 / button-event /
  any-event / SGR / SGR-Pixels ?1016), alternate scroll (?1007),
  focus reporting, synchronized output (DECSET ?2026), reverse screen
  (DECSCNM / ?5).
- **Kitty keyboard protocol** (progressive enhancement): `CSI > flags u`
  push / `CSI < u` pop / `CSI = u` set / `CSI ? u` query, per-screen flag
  stack. When an app enables it (neovim / helix / fish), keys are encoded
  in the unambiguous `CSI … u` form — Ctrl+letter, Escape, Shift+Tab and
  friends stop colliding with legacy control bytes. Functional keys reuse
  the xterm modifier form; text/event/repeat fields honoured.
- **OSC**: 0/1/2 (title) · 4 (palette query/set) · 7 (cwd) · 8 (hyperlinks
  with auto-detect) · 9 (single-arg notifications + 9;4 progress + 9;9 cwd) ·
  10/11/12 (default fg/bg/cursor color + query) · 52 (clipboard write) ·
  99 (kitty notification) · 104/110/111/112 (palette reset) · 133 (shell
  integration A/C/D) · 633 (VS Code shell integration) · 777 (urxvt notify) ·
  1337 (iTerm2: CurrentDir / ClearScrollback / notify=).
- **Renderer**: winit + wgpu + glyphon (cosmic-text). Background quad pass for
  per-cell colour + cursor block + hyperlink underline + search-match
  highlight. Truecolor everywhere; bold-brightens-base-16 follows xterm.
- **Tabs + BSP-split panes**: horizontal and vertical splits, zoom (tmux-style),
  swap, spatial focus, drag-reorder of tabs.
- **Selection**: drag / double-click word / triple-click line, clipboard
  copy via `Ctrl+Shift+C` (no auto-copy).
- **Search overlay**: literal + regex, forward / backward, `Esc` exits.
- **Broadcast input**: `toggle_broadcast` sends every keystroke to all panes
  in the active tab at once (iTerm2 / tmux synchronize-panes); a
  `⇉ BROADCAST` marker shows in the status bar while active.
- **Syntax highlighting**: WindTerm-style client-side regex rules recolour
  terminal output (URLs, IPs, UUIDs, hex, `ERROR`/`WARN`/`INFO`, quoted
  strings, numbers) — applied only to default-coloured text, so it never
  fights `ls --color` / `bat` / TUIs. Built-ins + custom rules via
  `[highlight]` in `config.toml`; hot-reloadable.
- **Scrollback**: bounded ring per pane, viewport navigation with
  `Shift+PgUp/PgDn/Home/End`, single-line and half-page actions, programmatic
  scroll-to-line via plugins, scrollback save.
- **Hot-reload**: `config.toml`, `init.lua`, `plugins/*.lua` all watched.
  Changes are picked up without restart; plugins receive a `reload` event.
- **Session restore**: optional `restore_session = true` writes
  `$XDG_CACHE_HOME/rterm/session.toml` on exit and re-opens the same
  tabs / cwds next start.

## Building

```bash
cargo build --workspace                 # full workspace
cargo run -p rterm-app                  # GUI (needs a display)
cargo run -p rterm-app --release        # optimized GUI
cargo run -p rterm-app -- --smoke       # headless PTY + parser pipeline
cargo test --workspace                  # 400+ unit tests
cargo clippy --workspace --all-targets -- -D warnings
```

Default-feature build pulls in winit / wgpu / glyphon / cosmic-text / mlua
(LuaJIT vendored). Cold release build is ~1 min on a typical laptop.

## CLI

```
rterm [OPTIONS]
  --config <path>                     Load this config instead of the default
  --smoke [--json]                    Headless PTY+parser sanity run
  --render-test                       Open a window, present one clear-only frame
  --list-actions [prefix] [--labels|--json]
  --list-events  [prefix] [--json]
  --list-keybindings [substr] [--json]
  --list-fonts   [substr] [--json]
  --print-config                      Resolved config as TOML
  --print-default-config              Bundled default.toml template
  --print-paths [--json]              Config / plugins / cache paths
  --check                             Validate config + lua, exit non-zero on error
  --font-size <pt>                    Override font size for this run
  --font-family <s>                   Override font family for this run
  --version [--json]
  --help
```

## Configuration

Two surfaces, both at `~/.config/rterm/` (`%APPDATA%\rterm\` on Windows):

- **`config.toml`** — declarative settings. Run `rterm --print-default-config
  > ~/.config/rterm/config.toml` to seed a fully-commented template.
- **`init.lua`** + **`plugins/*.lua`** — imperative behaviour, event hooks,
  custom actions.

Both surfaces hot-reload — edit the file and changes apply on save without
restarting rterm.

See [docs/README.md](docs/README.md) for the documentation index —
configuration schema, keybindings, themes, UI tour, and the Lua plugin
API are split across focused files.

## Default keybindings

| Combo                       | Action                                    |
|-----------------------------|-------------------------------------------|
| `Ctrl+Shift+T` / `W`        | New / close tab                           |
| `Ctrl+Shift+←` / `→`        | Switch tab                                |
| `Ctrl+Shift+Tab`            | Switch to last tab                        |
| `Ctrl+Shift+,` / `.`        | Move tab left / right                     |
| `Ctrl+Shift+D` / `E`        | Split horizontal / vertical               |
| `Ctrl+Shift+X`              | Close pane                                |
| `Ctrl+Shift+Z`              | Zoom / unzoom focused pane                |
| `Ctrl+Shift+{` / `}`        | Swap pane with previous / next            |
| `Alt+←/↑/→/↓`               | Focus pane spatially                      |
| `Alt+1..9`                  | Focus pane N (DFS order)                  |
| `Alt+Shift+←/↑/→/↓`         | Resize focused pane                       |
| `Ctrl+Shift+V` / `Shift+Insert` | Paste                                 |
| `Ctrl+Shift+C` / `Ctrl+Ins` | Copy selection                            |
| `Ctrl+Shift+Y`              | Copy hovered URL                          |
| `Ctrl+Shift+F`              | Search scrollback                         |
| `Ctrl+Shift+P`              | Open command palette                      |
| `Ctrl+Shift+H`              | Toggle help overlay                       |
| `Ctrl+Shift+K`              | Clear scrollback                          |
| `Ctrl+Shift+=` / `-` / `0`  | Font size: bigger / smaller / reset       |
| `Shift+PgUp/PgDn`           | Scrollback page                           |
| `Shift+Home/End`            | Scrollback top / live                     |
| `Ctrl+Alt+↑/↓`              | Jump prev / next prompt (OSC 133)         |

User bindings live in `[[keybindings]]` blocks in `config.toml` and override
defaults. List every available action with `rterm --list-actions --labels`.

## Environment variables

- `RUST_LOG` — tracing filter. Try `RUST_LOG=rterm=info,wgpu_hal=warn`.
- `WGPU_BACKEND` — `vulkan|gl|metal|dx12|primary|secondary`. Auto-defaults to
  `gl` on WSL2.
- `WGPU_PRESENT_MODE` — `fifo|mailbox|immediate|autovsync|autonovsync`.
  Auto-defaults to `fifo` on WSL2.
- `WGPU_DEBUG=1` — enable wgpu validation + debug callbacks.
- `WAYLAND_DISPLAY` — unset → winit falls back to X11.
- `SHELL` — fallback shell when `[shell] program` is unset.
- `RTERM_CONFIG_PATH` — override the default config path (used by every
  sub-flag that resolves it).
- `RTERM_SMOKE_COMMAND` — replace `echo hello rterm` in `--smoke`.

## Architecture

```
crates/
├── rterm-core     VT/ANSI parser, cell grid, scrollback ring, alt-screen,
│                  scroll region, full SGR, cursor motion, erase, scroll,
│                  save/restore, DECSET/DECRST, OSC. Pure data; no I/O.
├── rterm-pty      Cross-platform PTY via portable-pty. Owner + clonable
│                  PtyControl for write/resize across threads.
├── rterm-config   TOML schema for config.toml; loader + hot-reload helpers.
├── rterm-plugin   Lua 5.4 plugin host (mlua, LuaJIT vendored). Event
│                  dispatch, snapshot-pushed Lua API surface, custom action
│                  registration.
├── rterm-render   winit + wgpu + glyphon. Tabs with BSP tree of horizontal
│                  and vertical split panes, background-quad pass, per-cell
│                  rich text, mouse reporting, bracketed paste, scrollback
│                  viewport, selection, clipboard, URL open, search overlay.
└── rterm-app      `rterm` binary. CLI parsing, config + plugin watcher,
                   bridges PluginHost ↔ App via the EventSink trait, runs
                   the renderer.
```

### Lock discipline

- Per-pane `Arc<Mutex<Terminal>>` — held briefly by the PTY reader thread
  per chunk and by the renderer per frame.
- `std::sync::Mutex` is NOT reentrant. Snapshot code that collects guards into
  a `Vec` must read all needed state through those guards (see
  `crates/rterm-render/src/lib.rs` around the `focus_syncing` read for an
  example).

## Roadmap

The live, prioritized work plan lives in [ROADMAP.md](ROADMAP.md) —
quick wins (settings toggle for highlighting, Kitty placement offsets,
CLI/panic-hook fixes), medium items (IME input, Kitty keyboard
protocol, incremental search, broadcast input, GIF animation), and the
headline features (Sixel graphics, profiles/SSH manager, ligatures,
damage tracking). Items previously listed here (absolute-line
selection, right-click context menu, tab-drag ghost label) shipped.

## License

MIT OR Apache-2.0.
