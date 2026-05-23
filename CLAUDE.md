# rterm — context for Claude sessions

Cross-platform GPU-accelerated terminal emulator written in Rust with Lua plugins. This file is project-level context so any Claude session spawning here (e.g. via `/loop`, cron, or fresh open) can pick up where the last one left off.

## Architecture

Cargo workspace at the repo root. Crates under `crates/`:

- **rterm-core** — VT/ANSI state machine. Cell grid, scrollback ring, alt-screen, scroll region, full SGR (16/256/RGB), cursor motion (CUU/CUD/CUF/CUB/CUP/HVP/CHA/VPA/CNL/CPL), erase (ED/EL/ICH/DCH/IL/DL/ECH), scroll (SU/SD/RI/IND/NEL), save/restore (DECSC/DECRC + CSI s/u), DECSET/DECRST for ?25/?47/?1047/?1049/?1000/?1002/?1003/?1006/?2004, OSC 0/2 (window title), OSC 8 (hyperlinks), BEL flag, URL auto-detect. Pure data; no I/O.
- **rterm-pty** — `portable-pty` wrapper. Exposes `Pty` (owner) and `PtyControl` (Send+Sync clonable handle for write/resize).
- **rterm-config** — TOML config: `font.{family,size}`, `window.{width,height,opacity}`, `shell.{program,args}`, `keybindings`. Hot-reload of `init.lua` and `plugins/*.lua` is watched by `rterm-app`.
- **rterm-plugin** — Lua 5.4 host (`mlua` vendored). `rterm.log`, `rterm.on(event, fn)`, `rterm.add_match(name, pattern, opts)` (substring or regex → fires `match` event). Events: `startup`, `ready`, `key`, `resize`, `reload`, `bell`, `tab.new`, `tab.close`, `tab.switch`, `pane.split`, `pane.close`, `pane.focus`, `pane.exit`, `pane.swap`, `scroll`, `paste`, `copy`, `link.open`, `search.start`, `search.end`, `match`.
- **rterm-render** — winit + wgpu + glyphon (cosmic-text). Tabs with a BSP tree of horizontal **and** vertical split panes (`Tab { tree: Tree<Pane>, focus_path }`), background-quad pass (alpha-blended cell backgrounds + cursor block + hyperlink underline + search match highlight), per-cell colour rich text, mouse reporting (X10/SGR), bracketed paste detection, scrollback view via `Terminal::visible_row(offset, r)`, drag/word/line selection, clipboard via `arboard`, URL open via `open` crate, search overlay.
- **rterm-app** — `rterm` binary. Glues the above. `--smoke` runs a headless PTY+parser pipeline; default is GUI.

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
cargo test --workspace            # ~251 unit tests as of writing
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

If a session opens with just "продолжай" (continue), read this file plus `MEMORY.md` plus the most recent `git log`-equivalent (the project isn't a git repo by default — check timestamps), then pick the next pending item from the in-flight TODO. Common next steps documented in commits or in `README.md` Roadmap.

Typical next chunks at this stage:
1. Selection anchored to absolute logical lines (currently viewport-relative — drifts when scrollback shifts)
2. Sixel / ReGIS graphics decoding
3. Ghost label TEXT following the cursor during tab drag-reorder (the
   chip background already follows via `tab_bar_quads`'s ghost branch,
   but the label glyphs stay at the original slot position — a separate
   ghost text-buffer rendered at `cursor.x - press_offset` would close
   the gap).

## Conventions

- Tests live in `#[cfg(test)] mod tests` at the bottom of each module.
- Public types re-exported through each crate's `lib.rs`.
- ANSI sequences in tests use `\r\n` (PTY ONLCR is what real shells produce).
- New crates go under `crates/` and to `[workspace.members]`.
- `Cargo.toml` has `[workspace.dependencies]` — pin versions there, depend with `{ workspace = true }`.
