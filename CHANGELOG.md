# Changelog

All notable changes to rterm are recorded here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); versions track the
workspace `Cargo.toml`. A release is cut by tagging `v<version>`, which
triggers the binary-publishing workflow.

## [0.0.13] — unreleased

The largest batch since the initial MVP: three headline graphics/input
features, a profiles/SSH manager, a macOS global hotkey, and a wave of
correctness fixes — many surfaced by successive adversarial reviews of the
new code (11 confirmed bugs across 10 subsystems, two of them
security/privacy). Every change ships with tests; CI is green on Linux,
macOS and Windows.

### Added
- **Kitty keyboard protocol** (progressive enhancement, `CSI u`): per-screen
  flag stack (push/pop/set/query), unambiguous encoding of Ctrl+letter /
  Escape / Shift+Tab, event-type and associated-text fields.
- **Inline images — Sixel** (`DCS q`): pure decoder (palette/HLS/RGB, repeat,
  raster attributes, VT340 defaults) wired into the DCS path with decode
  caps and fuzz hardening. `img2sixel` / `lsix` now display.
- **Animated GIF playback**: multi-frame decode with per-frame delays,
  event-driven timer (idle when nothing is animating), disposal handled.
- **Profiles / SSH manager**: `[[profiles]]` presets (program/args/cwd/env/
  theme), `--profile <name>` / `--list-profiles`, a "New tab with profile…"
  command-palette entry, and per-profile-context command history.
- **macOS global hotkey** for the Guake drop-down (Carbon
  `RegisterEventHotKey`), joining the existing Windows backend.
- **IME input**: composed CJK / dead keys / macOS long-press accents, with
  inline preedit rendering at the cursor.
- **Broadcast input**: send every keystroke to all panes in the active tab
  (`⇉ BROADCAST` marker in the status bar).
- **Client-side syntax highlighting** (WindTerm-style, `[highlight]` rules,
  hot-reloadable) with a Settings-overlay toggle.
- **Alternate scroll mode** (DECSET `?1007`) so the wheel drives pagers
  (`less` / `man` / `git log`) on the alt-screen.
- **SGR-Pixels mouse reporting** (DECSET `?1016`); DECSET `?1048`
  (cursor save/restore).
- **Opt-in paste redaction** (`[history] redact_pasted`) keeps pasted
  secrets out of the command-history store.
- **Plugin API**: dispatch custom actions from `run_action` / keybindings,
  the `attention` event, `add_match` per-rule `on` callback, and the missing
  bare / `_of` pane accessors.
- Panic hook writes to `<cache>/rterm/panic.log`; `--history` /
  `--shell-integration` accept any argv position; scrollback offset widened
  to `u32` (full 1M-line reach).

### Fixed
- **Pasted secrets could leak to history** despite `redact_pasted`: the taint
  was per-line, so multi-line pastes recorded lines 2..N. Now tracked across
  the whole paste span.
- **Crafted GIF could OOM the terminal**: the animation decoder set no
  per-frame allocation limit (a 65535² frame forced ~16 GiB). Now bounded
  like the still-image path.
- **Sixel HLS colors were rotated 120°** (blue rendered green); decode cap
  aligned to the image-store cap to avoid alloc amplification.
- **Two windows clobbered each other's saved session**; and one torn/badly-
  escaped block could wipe every window's session. Now merge-on-append with
  resilient per-block parsing and full control-char escaping.
- **Kitty keyboard**: RIS now resets the flag stack (so `tput reset` restores
  legacy typing); the associated-text field is omitted for ctrl/alt/super.
- **Mouse reporting**: drag-motion reports now fire (drag-select in vim
  `mouse=a` / tmux worked only on release before); one wheel notch reports
  once (was 3×); modifier bits are encoded into the button.
- **IME**: keys are suppressed while composing (no Backspace line-corruption
  / double-input); committed text honors broadcast; preedit is clamped to
  its pane.
- A pre-release tag no longer reads as newer than its release in the update
  check; paste-confirm buttons stay clickable at any font size; OSC 52 is
  drained from every pane; `--smoke` is hermetic; session restore no longer
  mis-applies a title when a pane fails to spawn.

### Changed / Internal
- Split the ~16k-line `rterm-render/src/lib.rs` into `input.rs`,
  `event_loop.rs`, `gpu.rs`, and `payload.rs` (behavior-preserving).
- Incremental search now matches off the terminal lock; tab drag requires a
  movement threshold rather than a bare click.
- Added `ROADMAP.md` as the single source of truth for the work plan; synced
  `README.md` / `CLAUDE.md` with the shipped feature set.

## [0.0.12]

Baseline for this changelog. See the git history before `0.0.13` for
earlier work (VT core, tabs + BSP split panes, search, hot-reload, Lua
plugin host, inline iTerm2/Kitty images, session restore, Guake mode,
themes, command palette, SQLite history + suggestion popup, the full audit
sweep, and the event-driven render loop).
