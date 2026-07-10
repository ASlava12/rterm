# rterm — context for Claude sessions

Cross-platform GPU-accelerated terminal emulator written in Rust with Lua plugins. This file is project-level context so any Claude session spawning here (e.g. via `/loop`, cron, or fresh open) can pick up where the last one left off.

## Architecture

Cargo workspace at the repo root. Crates under `crates/`:

- **rterm-core** — VT/ANSI state machine. Cell grid, scrollback ring, alt-screen, scroll region, full SGR (16/256/RGB), cursor motion (CUU/CUD/CUF/CUB/CUP/HVP/CHA/VPA/CNL/CPL), erase (ED/EL/ICH/DCH/IL/DL/ECH), scroll (SU/SD/RI/IND/NEL), save/restore (DECSC/DECRC + CSI s/u), DECSET/DECRST for ?25/?47/?1047/?1049/?1000/?1002/?1003/?1006/?1007/?1016/?1048/?2004/?2026/?5, many OSC (0/1/2 title, 4/104 palette, 7 cwd, 8 hyperlinks, 9/99/777 notify, 10/11/12 colors, 52 clipboard, 133/633 shell-integration, 1337 iTerm2), Kitty keyboard protocol (CSI u, per-screen flag stack, `kitty_kbd_stack`), inline images (iTerm2 `OSC 1337 File=`, Kitty graphics `APC G` — see `image.rs`; Sixel `DCS q` — see `sixel.rs`, with decode caps), BEL flag, URL auto-detect. Pure data; no I/O.
- **rterm-pty** — `portable-pty` wrapper (vendored as `portable-pty-rterm`). Exposes `Pty` (owner) and `PtyControl` (Send+Sync clonable handle for write/resize).
- **rterm-history** — bundled SQLite (`rusqlite`) command-history store. `record(text, context)` / `suggest(prefix, limit, context)`; composite `PRIMARY KEY (text, context)` isolates per shell-integration context; in-place migration of old single-`text`-PK DBs.
- **rterm-config** — TOML config: `font.{family,size,bold_is_bright}`, `window.{width,height,opacity,os_decorations}`, `shell.{program,args,env}`, `keybindings`, `[highlight]`, `[history]` (popup + `redact_pasted`), `[[profiles]]` (SSH/launch presets, `Config::profile(name)`). Hot-reload of `init.lua` and `plugins/*.lua` is watched by `rterm-app`.
- **rterm-plugin** — Lua 5.4 host (`mlua` vendored). `rterm.log`, `rterm.on(event, fn)`, `rterm.add_match(name, pattern, opts)` (substring or regex → fires `match` event). Events: `startup`, `ready`, `key`, `resize`, `reload`, `bell`, `tab.new`, `tab.close`, `tab.switch`, `pane.split`, `pane.close`, `pane.focus`, `pane.exit`, `pane.swap`, `scroll`, `paste`, `copy`, `link.open`, `search.start`, `search.end`, `match`.
- **rterm-render** — winit + wgpu + glyphon (cosmic-text). Tabs with a BSP tree of horizontal **and** vertical split panes (`Tab { tree: Tree<Pane>, focus_path }`), background-quad pass (alpha-blended cell backgrounds + cursor block + hyperlink underline + search match highlight), per-cell colour rich text, mouse reporting (X10/SGR/SGR-pixel + alternate scroll), bracketed paste detection, scrollback view via `Terminal::visible_row(offset, r)`, drag/word/line selection, clipboard via `arboard`, URL open via `open` crate, search overlay, command palette (built-ins + custom actions + profiles), fish-style history-suggestion popup (`command_capture.rs` → rterm-history), inline image pass (`image_pass.rs`/`image_decode.rs`, animated GIF playback on an event-driven timer), session save/restore (merge-on-append), WindTerm-style client-side syntax highlighting (`highlight.rs` — global regex rule set applied in `build_spans` to default-fg cells only; config `[highlight]`). `lib.rs` is split into `input.rs` (key/mouse/paste + encoders), `event_loop.rs` (`ApplicationHandler` + redraw), `gpu.rs` (`GpuState`).
- **rterm-app** — `rterm` binary. Glues the above. `--smoke` runs a headless PTY+parser pipeline; `--render-test` presents one clear frame; `--profile <name>` / `--list-profiles`, `--check-update`; default is GUI.

## Keybindings

- **Ctrl+Shift+T/W** — new / close tab
- **Ctrl+Shift+←/→** — switch tab
- **Ctrl+Shift+D/X** — split / close pane
- **Ctrl+Shift+{/}** — swap focused pane with previous / next (DFS order)
- **Alt+←/→** — focus pane (spatial)
- **Alt+1..9** — focus Nth pane (DFS order)
- **Ctrl+Shift+V/C** — paste / copy
- **Ctrl+Shift+F** — search in scrollback (Enter/↓ next, Shift+Enter/↑ prev, Esc exit)
- **Shift+PgUp/PgDn/Home/End** — scrollback navigation
- Mouse: wheel scroll, drag select, double-click word, triple-click line, click focus, **Ctrl+click** open URL/hyperlink

## Build & test

```bash
cargo build                       # full workspace
cargo test --workspace            # ~647 unit tests as of writing
cargo run -p rterm-app -- --smoke # headless CI/sanity
cargo run -p rterm-app            # GUI (needs a display)
```

When `target/` exceeds ~5 GB run `cargo clean` at an iteration boundary. The cache rebuilds in ~3 min from cold (wgpu + winit + glyphon + cosmic-text are heavy).

## WSL2 / Wayland notes

- `WGPU_BACKEND` defaults to `gl` on WSL2 — Mesa's Vulkan path frequently stalls in instance init. Override to `vulkan|metal|dx12|primary|secondary` if needed.
- `WGPU_PRESENT_MODE` defaults to `fifo` on WSL2 (vs. `autovsync` elsewhere) — llvmpipe can deadlock waiting for `AutoVsync`. Force-override with `WGPU_PRESENT_MODE=mailbox|immediate|...`.
- The first `RedrawRequested` issues a clear-only frame via `GpuState::render_clear_only` before falling through to the full render in the same handler — needed to wake the Wayland compositor's `configure` chain on WSL2 GL.
- Initial cursor icon is explicitly `Text` (I-beam) — Wayland surfaces stay "no cursor" until the client commits one.
- `rterm --render-test` opens a window, presents one clear-only frame, prints `render test: OK`/`FAIL`, and exits. Useful to verify the GPU pipeline without spinning up panes/plugins.

## How to continue iterations

If a session opens with just "продолжай" (continue), read this file plus `ROADMAP.md` plus the recent `git log`, then pick the next pending item from **ROADMAP.md** — it is the single source of truth for the work plan (prioritized P0→P3, each item carries code anchors and a definition-of-done). Move top-down within a priority band; mark items `[~]`/`[x]` as you go and keep the file honest.

## Conventions

- Tests live in `#[cfg(test)] mod tests` at the bottom of each module.
- Public types re-exported through each crate's `lib.rs`.
- ANSI sequences in tests use `\r\n` (PTY ONLCR is what real shells produce).
- New crates go under `crates/` and to `[workspace.members]`.
- `Cargo.toml` has `[workspace.dependencies]` — pin versions there, depend with `{ workspace = true }`.
