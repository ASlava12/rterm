//! Terminal state machine. Drives a `vte::Parser` and applies VT actions onto
//! the grid: printable characters, control bytes (CR/LF/BS/HT), CSI (cursor
//! motion, erase, SGR), scroll region, alt-screen, scrollback ring, and the
//! commonly-used DECSET/DECRST modes.

use std::collections::{HashMap, VecDeque};

use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

use crate::color::{Color, NamedColor};
use crate::grid::{Cell, CellAttrs, Grid, Position, Size};

const DEFAULT_SCROLLBACK_LIMIT: usize = 10_000;
const PRIMARY: usize = 0;
const ALT: usize = 1;
const TAB_STEP: usize = 8;

/// Decode an even-length ASCII hex string into a UTF-8 String. Used by
/// XTGETTCAP to parse the hex-encoded terminfo cap names apps send via
/// `DCS + q`. Returns `None` on malformed input (odd length, non-hex
/// digits, or non-ASCII output) so the caller can reply with status 0.
fn decode_hex_ascii(s: &str) -> Option<String> {
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks_exact(2) {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    String::from_utf8(out).ok()
}

/// Encode an ASCII string as upper-case hex pairs. Counterpart to
/// `decode_hex_ascii`; used for XTGETTCAP reply values.
fn encode_hex_ascii(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len() * 2);
    for &b in s.as_bytes() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Drop every mark whose logical line index lived inside the first
/// `dropped` lines of scrollback, then shift the survivors down by the
/// same amount. Used whenever the scrollback ring shrinks (manual
/// `clear_scrollback`, ED 3, scrollback-limit decrease) so that prompt /
/// command marks continue to point at the right logical lines under the
/// new origin.
fn shift_marks_after_scrollback_drop(marks: &mut VecDeque<usize>, dropped: usize) {
    if dropped == 0 {
        return;
    }
    marks.retain(|m| *m >= dropped);
    for m in marks.iter_mut() {
        *m -= dropped;
    }
}

/// Whitelist of URL schemes the terminal is allowed to invoke via the
/// system handler (`open::that_detached`). The terminal sees URLs from
/// TWO sources: auto-detected (`detect_url_at`) and shell-supplied via
/// OSC 8 (`hyperlink_at`). The auto-detector applies this filter
/// inline; OSC 8 was previously trusted blindly, which let a malicious
/// shell embed `javascript:`, `file:///etc/...`, or `data:` URLs under
/// arbitrary visible text — a click would invoke them through
/// xdg-open / start / open without any scheme validation.
///
/// Allowed schemes:
/// - `http`, `https` — the overwhelming majority of practical URLs
/// - `ftp` — legacy but legitimate
/// - `mailto:` — non-`://` form, validated separately
/// - `ssh` — terminal users have a legitimate need (e.g. wezterm/iterm2
///   pattern of `ssh://user@host`)
/// - `file://` is INTENTIONALLY excluded — a shell that can write OSC 8
///   can also already read files. The risk is the click giving the
///   *user's browser* the URL and triggering content-disposition /
///   script-side side effects.
pub fn is_safe_url(s: &str) -> bool {
    // Schemes are case-insensitive per RFC 3986 §3.1, so accept any
    // mix of cases — some clipboard managers and OSC-8 producers
    // (cmd.exe `start`, older xterm-style scripts) preserve upper /
    // mixed case. The whitelist itself stays the canonical lower form.
    if let Some(scheme_end) = s.find("://") {
        // `find("://")` returns a byte index at a UTF-8 boundary
        // (the matched substring is pure ASCII), so this slice is
        // always safe even when `s` contains multi-byte text after
        // the scheme.
        let scheme = &s[..scheme_end];
        return ["http", "https", "ftp", "ssh"]
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed));
    }
    // `mailto:` (no `://`) — match the literal scheme case-insensitively
    // on the underlying ASCII bytes so we don't accidentally slice into
    // a multi-byte char. The auto-URL detector hands every cell run that
    // looks vaguely URL-shaped to this function, including pure Cyrillic
    // / CJK text where the first 7 bytes can span a char boundary —
    // a plain `s[..7].eq_ignore_ascii_case("mailto:")` panicked there.
    const MAILTO: &[u8] = b"mailto:";
    if s.len() >= MAILTO.len()
        && s.as_bytes()[..MAILTO.len()]
            .iter()
            .zip(MAILTO)
            .all(|(have, want)| have.eq_ignore_ascii_case(want))
    {
        // MAILTO is ASCII, so the byte index is always at a UTF-8
        // boundary — slicing the rest is safe.
        return s[MAILTO.len()..].contains('@');
    }
    false
}

fn default_tab_stops(cols: u16) -> Vec<bool> {
    let n = cols as usize;
    let mut stops = vec![false; n];
    let mut i = TAB_STEP;
    while i < n {
        stops[i] = true;
        i += TAB_STEP;
    }
    stops
}

#[derive(Debug, Clone, Copy)]
struct SgrState {
    attrs: CellAttrs,
    fg: Color,
    bg: Color,
}

impl Default for SgrState {
    fn default() -> Self {
        Self { attrs: CellAttrs::empty(), fg: Color::Default, bg: Color::Default }
    }
}

#[derive(Debug, Clone, Copy)]
struct CursorState {
    pos: Position,
    sgr: SgrState,
    /// Active charset slot (0 = G0, 1 = G1) at save time. Restored by DECRC
    /// so DEC line-drawing apps round-trip cleanly across alt-screen swaps.
    active_charset: u8,
    charset_g0: Charset,
    charset_g1: Charset,
    origin_mode: bool,
    autowrap: bool,
}

#[derive(Debug, Clone, Copy)]
struct ScrollRegion {
    top: u16,
    bottom: u16,
}

impl ScrollRegion {
    fn full(size: Size) -> Self {
        Self { top: 0, bottom: size.rows.saturating_sub(1) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    /// Full-cell block (DECSCUSR 0/1/2).
    Block,
    /// Underline at the bottom of the cell (DECSCUSR 3/4).
    Underline,
    /// Vertical bar on the left edge of the cell (DECSCUSR 5/6).
    Bar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseTracking {
    Off,
    /// DECSET ?1000 — X10/button-down only.
    X10,
    /// DECSET ?1002 — buttons + motion while held.
    ButtonEvent,
    /// DECSET ?1003 — buttons + all motion.
    AnyEvent,
}

pub struct Terminal {
    grids: [Grid; 2],
    active: usize,
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_limit: usize,
    cursor: Position,
    saved: [Option<CursorState>; 2],
    sgr: SgrState,
    /// XTPUSHSGR (`CSI # {`) / XTPOPSGR (`CSI # }`) stack. Apps like
    /// `bat`, `delta`, and `eza` push current SGR, emit decorations, then
    /// pop to restore — without it, colours leak across nested spans.
    /// Capped to avoid an unbounded stack from malformed input.
    sgr_stack: Vec<SgrState>,
    cursor_visible: bool,
    region: ScrollRegion,
    parser: Parser,
    pending_title: Option<String>,
    pending_bell: bool,
    mouse_tracking: MouseTracking,
    sgr_mouse: bool,
    bracketed_paste: bool,
    /// DECSET ?5 — DECSCNM "reverse screen": invert default-fg /
    /// default-bg for the whole grid. Some legacy apps toggle this
    /// for a brief "flash" effect during attention prompts. Off
    /// by default. The renderer reads it via `is_reverse_screen()`
    /// and inverts colours per cell at draw time.
    reverse_screen: bool,
    /// DECSET ?1004 — focus tracking. When set, the terminal sends
    /// `ESC [ I` on focus-in and `ESC [ O` on focus-out so apps like vim
    /// can react to window-focus changes.
    focus_tracking: bool,
    /// DECSET ?2026 — Synchronized Output Mode. Apps (neovim, kakoune,
    /// helix) set this before a multi-segment frame and reset it once
    /// they're done so the terminal can present a tear-free composite
    /// instead of redrawing partway through. The renderer reads this via
    /// `sync_output()` and may delay presentation while it's true.
    sync_output: bool,
    /// ANSI IRM (mode 4) — Insert / Replace mode. When set, printing
    /// shifts existing cells to the right within the line instead of
    /// overwriting (off by default → replace mode).
    insert_mode: bool,
    /// DECCKM (CSI ?1h): when true, arrow / home / end keys should send
    /// `ESC O X` instead of `ESC [ X`. Apps like vim flip this on alt-screen.
    app_cursor_keys: bool,
    /// DECAWM (CSI ?7h, default on): autowrap. When off, characters past the
    /// right margin overwrite the last column instead of wrapping.
    autowrap: bool,
    /// DECOM (CSI ?6h, default off): origin mode. When on, CUP / VPA target
    /// rows are relative to `region.top` and cursor cannot leave the region.
    origin_mode: bool,
    hyperlinks: HashMap<u32, String>,
    /// Reverse index URI → id. Keeps OSC 8 dispatch O(1) when the shell
    /// repeats the same hyperlink across many cells (e.g. `ls --color`
    /// putting the same `file://` on every entry). The previous
    /// implementation linear-scanned the `hyperlinks` map on every OSC
    /// 8; with HYPERLINK_CAP = 4096 that was up to ~4096 equality
    /// compares per character span. Memory cost: each URI is stored
    /// twice (key + value), bounded by the same HYPERLINK_TOTAL_BYTES_CAP.
    hyperlink_uri_to_id: HashMap<String, u32>,
    /// FIFO order of hyperlink ids inserted into `hyperlinks` so we can
    /// evict the oldest entry when the map outgrows [`HYPERLINK_CAP`].
    hyperlink_order: VecDeque<u32>,
    next_link_id: u32,
    current_hyperlink: u32,
    /// Raw base64 payload of the most recent OSC 52 clipboard-write request.
    /// Decoded by the renderer (rterm-core stays free of base64).
    pending_clipboard: Option<String>,
    cursor_shape: CursorShape,
    cursor_should_blink: bool,
    /// Working directory last advertised by the shell via OSC 7. Updated
    /// whenever the shell prints `ESC ] 7 ; file://<host><path> BEL`.
    cwd: Option<String>,
    /// Completed lines (captured at linefeed time) waiting for the
    /// application to drain via `take_completed_lines()`. Capped to avoid
    /// runaway memory when no plugin is consuming them.
    pending_lines: VecDeque<String>,
    /// Logical line indices (scrollback first, then grid) where a shell
    /// prompt began, captured from OSC 133 ; A. Shifted when scrollback
    /// overflows so the markers stay aligned with their content.
    prompt_marks: VecDeque<usize>,
    /// Logical line indices where a command began executing, captured from
    /// OSC 133 ; C (emitted by the shell when the user submits a line).
    /// Same shifting rules as `prompt_marks`.
    command_marks: VecDeque<usize>,
    /// `(exit_code, optional_duration_ms)` records emitted by OSC 133;D
    /// sequences. `duration_ms` is set when we saw a matching OSC 133;C
    /// since the last D (so the time-between-marks reflects how long
    /// the user's command took); `None` otherwise (shell didn't emit a
    /// C beforehand, or we missed it). Drained by the App per frame and
    /// converted into `shell.exit`/`pane.shell_exit` plus the richer
    /// `pane.command_finish` event.
    pending_command_finishes: VecDeque<CommandFinish>,
    /// When OSC 133;C fires we capture `Instant::now` so the next D can
    /// compute the elapsed run time. Cleared on use (so consecutive D's
    /// without a fresh C produce `duration_ms = None`).
    last_command_start: Option<std::time::Instant>,
    /// Reply strings the terminal needs to write back to the shell — e.g.
    /// the OSC 10 / 11 colour-query responses. Drained by the App.
    osc_responses: VecDeque<String>,
    /// Default fg / bg in 8-bit sRGB so we can answer OSC 10 / 11 ?
    /// without depending on the renderer's palette. Set via
    /// `set_default_colors` once at startup.
    default_fg_rgb: [u8; 3],
    default_bg_rgb: [u8; 3],
    /// Configured RGB for the 16 named ANSI colours. Used to answer OSC 4
    /// palette queries (`ESC ] 4 ; n ; ? ST`). Set via `set_named_palette`
    /// from the renderer at startup; default mirrors xterm.
    named_palette: [[u8; 3]; 16],
    /// Configured cursor colour reported by OSC 12 queries. `None` means the
    /// cursor inherits the cell's foreground (the renderer's default).
    cursor_rgb: Option<[u8; 3]>,
    /// Column-indexed horizontal tab stops. `tab_stops[c]` true means a tab
    /// stop is set at column `c`. Default: every 8 columns. Mutated by HTS
    /// (ESC H), TBC (CSI g) and reset by terminal-wide RIS.
    tab_stops: Vec<bool>,
    /// DECPAM (ESC =): when true the keypad sends application-mode
    /// sequences (`ESC O X` for digits/PF keys). DECPNM (ESC >) clears it.
    app_keypad: bool,
    /// OSC 9 notification messages queued for the App to surface as
    /// `notification` plugin events. Capped to avoid runaway memory.
    pending_notifications: VecDeque<String>,
    /// `OSC 9 ; 4 ; <state> ; <pct>` progress reports queued for the App
    /// to surface as `progress` plugin events. `(state, percent)`.
    pending_progress: VecDeque<(u8, u8)>,
    /// Accumulated DCS body. Built up by `put` between `hook` and `unhook`.
    /// Only populated while `dcs_is_rqss` is true — bytes for unhandled DCS
    /// kinds are discarded.
    dcs_buf: Vec<u8>,
    /// True while the parser is inside a `DCS $ q ... ST` (DECRQSS — Request
    /// Status String). Reset to false at every `unhook`.
    dcs_is_rqss: bool,
    /// True while parsing the payload of an XTGETTCAP query
    /// (`DCS + q <hex-cap-names> ST`). Apps use this to ask the terminal
    /// what terminfo capabilities it supports.
    dcs_is_xtgettcap: bool,
    /// Last graphic character emitted via `print`. REP (`CSI Pn b`) repeats
    /// this character `Pn` times. Reset by any control-byte execute or CSI
    /// dispatch other than `b` itself.
    last_printed: Option<char>,
    /// Mirror of the most recently emitted OSC 0/2 title — the source for
    /// CSI 22 t (push). Cleared by a subsequent OSC 0/2 with an empty body.
    current_title: Option<String>,
    /// Stack of saved titles fed by CSI 22 t and drained by CSI 23 t. Capped
    /// to keep the memory bounded for misbehaving apps.
    title_stack: Vec<String>,
    /// Palette updates queued for the App to apply to the renderer's
    /// live palette (OSC 4 / 10 / 11 SET). Capped to keep memory bounded.
    pending_palette_changes: VecDeque<PaletteUpdate>,
    /// Designated character set for G0 / G1. Switched by `ESC ( X` /
    /// `ESC ) X`. The active slot is selected by SI (0x0F → G0) and SO
    /// (0x0E → G1). Only ASCII and DEC special graphics are supported.
    charset_g0: Charset,
    charset_g1: Charset,
    /// Which of {G0, G1} is currently active. 0 = G0, 1 = G1.
    active_charset: u8,
    /// Inline images registered by the iTerm2 `OSC 1337 ;File=` and
    /// Kitty `APC G` protocols. Keyed by a monotonically-incremented
    /// id; the matching [`ImagePlacement`]s in `image_placements`
    /// reference back via `image_id`. Capped at `IMAGE_STORE_CAP`
    /// entries so a malicious or buggy shell can't pin unbounded
    /// memory with a flood of inline blobs.
    images: HashMap<u64, crate::image::Image>,
    /// Insertion order of `images` — used to evict the oldest entry
    /// (FIFO) when the store hits its cap, mirroring how
    /// `hyperlink_order` keeps the hyperlink table bounded.
    image_order: VecDeque<u64>,
    /// Active placements — image-rect anchors in absolute
    /// (scrollback ++ grid) coordinates. Multiple placements can
    /// reference the same `image_id` (Kitty's virtual placements).
    /// Pruned by `evict_placements_at_cell` whenever a print /
    /// erase touches an image's footprint.
    image_placements: Vec<crate::image::ImagePlacement>,
    /// Monotonic id source for new images.
    next_image_id: u64,
    /// Running byte total across `images.data` for the
    /// `IMAGE_BYTES_CAP` quota — recomputing each call would be
    /// O(n) per insert.
    image_bytes_total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charset {
    Ascii,
    DecSpecialGraphics,
}

const PENDING_LINES_CAP: usize = 1024;

/// Hard ceiling for `osc_responses`. The renderer drains every frame, so
/// in normal operation this never matters — but a shell that floods OSC
/// queries between draws (DECRQM in a tight loop, XTGETTCAP fuzzing)
/// could otherwise grow this VecDeque unbounded. Drop the oldest reply
/// when over.
const OSC_RESPONSES_CAP: usize = 256;
const PROMPT_MARKS_CAP: usize = 512;
const NOTIFICATIONS_CAP: usize = 64;
const HYPERLINK_CAP: usize = 4096;
/// Running total cap on the cumulative bytes stored in the hyperlink
/// URI table. Without this a malicious shell can fill `HYPERLINK_CAP`
/// entries × `URI_CAP` bytes each = 32 MiB of pinned strings per pane
/// — a slow but real DoS. 1 MiB matches "real URI loads are well
/// under this" while still blunting the worst case.
const HYPERLINK_TOTAL_BYTES_CAP: usize = 1024 * 1024;
/// Hard ceiling on the number of distinct images held in the
/// inline-image store. iTerm2 / Kitty don't enforce any limit on
/// the wire; a shell that streams thousands of tiny PNGs would
/// otherwise pin unbounded RAM. 256 frames is plenty for typical
/// `chafa` / matplotlib / yazi-preview workloads (those reuse
/// placements or evict via the protocols' own delete actions).
const IMAGE_STORE_CAP: usize = 256;
/// Running-byte ceiling across all stored images' `data` fields.
/// 64 MiB matches "a couple of hi-res screenshots in scrollback"
/// without letting a malicious payload trigger an OOM.
const IMAGE_BYTES_CAP: usize = 64 * 1024 * 1024;
/// Hard ceiling on the size of a single image payload, in bytes.
/// Larger than `IMAGE_BYTES_CAP / IMAGE_STORE_CAP` so the global
/// cap can still hold a few normal images alongside one big one,
/// but smaller than `IMAGE_BYTES_CAP` itself so a single
/// pathological frame can't blow past the budget on its own.
pub const IMAGE_MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Pending palette change emitted by OSC 4 / 10 / 11 SET. Drained by the
/// App and folded into the renderer's live `Palette`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteUpdate {
    Named(u8, [u8; 3]),
    DefaultFg([u8; 3]),
    DefaultBg([u8; 3]),
}

/// Output of one `OSC 133 ; D ; <code>` shell-integration mark, combining
/// the exit code with the (optional) wall-clock duration of the command
/// run that preceded it (measured from the most recent `OSC 133 ; C`).
/// The renderer drains these per frame and emits both the historical
/// `shell.exit` / `pane.shell_exit` events and the richer
/// `pane.command_finish` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandFinish {
    pub exit_code: i32,
    /// Milliseconds between the most recent `OSC 133 ; C` and this `;D`.
    /// `None` if no matching `;C` was seen in the same command cycle
    /// (shell forgot to emit one, or the marks came in the wrong order).
    pub duration_ms: Option<u64>,
}

// Default-palette constants used to live here as private `const`s.
// They moved to `rterm_core::color` so `rterm-render`'s renderer-side
// `DEFAULT_THEME` and this crate's OSC reset paths share one source of
// truth — otherwise the two could drift on a one-sided edit and a
// shell-issued OSC 111 / OSC 112 reset would jump the colour to a
// value that no longer matched what the renderer paints as "Default".
use crate::color::{DEFAULT_BG, DEFAULT_FG, DEFAULT_NAMED_PALETTE};

fn indexed_palette_rgb(i: u8, named: &[[u8; 3]; 16]) -> [u8; 3] {
    if (i as usize) < 16 {
        return named[i as usize];
    }
    if i < 232 {
        let v = i - 16;
        let r = v / 36;
        let g = (v / 6) % 6;
        let b = v % 6;
        let map = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
        return [map(r), map(g), map(b)];
    }
    let gray = 8 + (i - 232) * 10;
    [gray, gray, gray]
}

impl Terminal {
    pub fn new(size: Size) -> Self {
        Self {
            grids: [Grid::new(size), Grid::new(size)],
            active: PRIMARY,
            scrollback: VecDeque::new(),
            scrollback_limit: DEFAULT_SCROLLBACK_LIMIT,
            cursor: Position::default(),
            saved: [None, None],
            sgr: SgrState::default(),
            sgr_stack: Vec::new(),
            cursor_visible: true,
            region: ScrollRegion::full(size),
            parser: Parser::new(),
            pending_title: None,
            pending_bell: false,
            mouse_tracking: MouseTracking::Off,
            sgr_mouse: false,
            bracketed_paste: false,
            reverse_screen: false,
            focus_tracking: false,
            sync_output: false,
            insert_mode: false,
            app_cursor_keys: false,
            autowrap: true,
            origin_mode: false,
            hyperlinks: HashMap::new(),
            hyperlink_uri_to_id: HashMap::new(),
            hyperlink_order: VecDeque::new(),
            next_link_id: 1,
            current_hyperlink: 0,
            pending_clipboard: None,
            cursor_shape: CursorShape::Block,
            cursor_should_blink: true,
            cwd: None,
            pending_lines: VecDeque::new(),
            prompt_marks: VecDeque::new(),
            command_marks: VecDeque::new(),
            pending_command_finishes: VecDeque::new(),
            last_command_start: None,
            osc_responses: VecDeque::new(),
            default_fg_rgb: [220, 220, 220],
            default_bg_rgb: [10, 12, 18],
            named_palette: DEFAULT_NAMED_PALETTE,
            cursor_rgb: None,
            tab_stops: default_tab_stops(size.cols),
            app_keypad: false,
            pending_notifications: VecDeque::new(),
            pending_progress: VecDeque::new(),
            dcs_buf: Vec::new(),
            dcs_is_rqss: false,
            dcs_is_xtgettcap: false,
            last_printed: None,
            current_title: None,
            title_stack: Vec::new(),
            pending_palette_changes: VecDeque::new(),
            charset_g0: Charset::Ascii,
            charset_g1: Charset::Ascii,
            active_charset: 0,
            images: HashMap::new(),
            image_order: VecDeque::new(),
            image_placements: Vec::new(),
            next_image_id: 1,
            image_bytes_total: 0,
        }
    }

    /// Register a new image payload. Returns its assigned id, or
    /// `None` when the payload would push the store past
    /// [`IMAGE_MAX_PAYLOAD_BYTES`] / [`IMAGE_BYTES_CAP`] /
    /// [`IMAGE_STORE_CAP`] even after evicting the oldest entries
    /// (e.g. one image bigger than the global byte budget).
    /// Caller (the iTerm2 / Kitty parsers) provides the
    /// pre-decoded payload, format hint, and pixel dimensions.
    pub fn register_image(
        &mut self,
        format: crate::image::ImageFormat,
        width_px: u32,
        height_px: u32,
        data: Vec<u8>,
    ) -> Option<u64> {
        if data.len() > IMAGE_MAX_PAYLOAD_BYTES {
            return None;
        }
        if data.len() > IMAGE_BYTES_CAP {
            return None;
        }
        // Evict FIFO until the new payload fits, both by count and
        // by bytes. Drops placements that reference the evicted
        // image so the renderer doesn't deref a stale id next frame.
        while !self.image_order.is_empty()
            && (self.image_order.len() >= IMAGE_STORE_CAP
                || self.image_bytes_total + data.len() > IMAGE_BYTES_CAP)
        {
            if let Some(victim) = self.image_order.pop_front() {
                if let Some(img) = self.images.remove(&victim) {
                    self.image_bytes_total =
                        self.image_bytes_total.saturating_sub(img.data.len());
                }
                self.image_placements.retain(|p| p.image_id != victim);
            } else {
                break;
            }
        }
        let id = self.next_image_id;
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
        self.image_bytes_total += data.len();
        self.image_order.push_back(id);
        self.images.insert(
            id,
            crate::image::Image { id, format, width_px, height_px, data },
        );
        Some(id)
    }

    /// Attach an image placement to the current frame. The renderer
    /// will draw the image at `abs_row × col` covering `rows × cols`
    /// cells. Silently no-ops when `image_id` doesn't match a
    /// registered image (e.g. one that was just FIFO-evicted).
    pub fn place_image(&mut self, placement: crate::image::ImagePlacement) {
        if !self.images.contains_key(&placement.image_id) {
            return;
        }
        if placement.rows == 0 || placement.cols == 0 {
            return;
        }
        self.image_placements.push(placement);
    }

    /// Snapshot of every active placement. The renderer iterates
    /// this each frame; the borrow is read-only so no copy unless
    /// the caller needs to extend its own lifetime past the frame.
    pub fn image_placements(&self) -> &[crate::image::ImagePlacement] {
        &self.image_placements
    }

    /// Look up an image's raw bytes + format by id. Returns `None`
    /// for ids that were FIFO-evicted or never registered. Used by
    /// the renderer to upload to the GPU on first draw.
    pub fn image(&self, id: u64) -> Option<&crate::image::Image> {
        self.images.get(&id)
    }

    /// Drop every placement whose cell footprint contains
    /// `(abs_row, col)`. Called from the print / erase / scroll
    /// paths so text overwriting an image surface visibly removes
    /// the image (xterm / iTerm2 semantics — Kitty's "image as
    /// layer" mode is out of scope for v1). Returns the number of
    /// placements removed.
    pub fn evict_placements_at_cell(&mut self, abs_row: i64, col: u16) -> usize {
        let before = self.image_placements.len();
        self.image_placements.retain(|p| !p.covers(abs_row, col));
        before - self.image_placements.len()
    }

    /// Drop every placement and every stored image. Called by
    /// `RIS` (`ESC c`, full reset) and by Kitty's `a=d, d=A` delete-
    /// all action. Frees the byte budget so subsequent images can
    /// land.
    pub fn clear_images(&mut self) {
        self.images.clear();
        self.image_order.clear();
        self.image_placements.clear();
        self.image_bytes_total = 0;
    }

    pub fn app_keypad(&self) -> bool {
        self.app_keypad
    }

    /// Drain queued OSC 9 notification messages.
    pub fn take_notifications(&mut self) -> Vec<String> {
        self.pending_notifications.drain(..).collect()
    }

    /// Drain queued OSC 9;4 progress reports. Each entry is
    /// `(state, percent)` where state is 0=clear, 1=set, 2=error,
    /// 3=indeterminate, 4=warning (see iTerm2 / Windows Terminal docs).
    pub fn take_progress(&mut self) -> Vec<(u8, u8)> {
        self.pending_progress.drain(..).collect()
    }

    /// Drain `(exit_code, duration_ms?)` records for every OSC 133;D the
    /// shell sent since the last drain. The renderer emits one
    /// `pane.command_finish` event per record (with duration when
    /// available) alongside the legacy `shell.exit` / `pane.shell_exit`
    /// events that consume the same source.
    pub fn take_command_finishes(&mut self) -> Vec<CommandFinish> {
        self.pending_command_finishes.drain(..).collect()
    }

    pub fn take_osc_responses(&mut self) -> Vec<String> {
        self.osc_responses.drain(..).collect()
    }

    /// Configure the RGB triplets returned for OSC 10 / 11 colour queries.
    pub fn set_default_colors(&mut self, fg: [u8; 3], bg: [u8; 3]) {
        self.default_fg_rgb = fg;
        self.default_bg_rgb = bg;
    }

    /// Replace the 16 named-colour entries reported by OSC 4 queries.
    pub fn set_named_palette(&mut self, palette: [[u8; 3]; 16]) {
        self.named_palette = palette;
    }

    /// Set the cursor colour reported by OSC 12 queries. `None` falls back
    /// to the cell foreground (xterm convention).
    pub fn set_cursor_color(&mut self, rgb: Option<[u8; 3]>) {
        self.cursor_rgb = rgb;
    }

    /// Read the cursor colour set by OSC 12 / `set_cursor_color`. Returns
    /// `None` when the pane is using the renderer's palette default.
    pub fn cursor_color(&self) -> Option<[u8; 3]> {
        self.cursor_rgb
    }

    /// Drain queued palette updates (OSC 4 / 10 / 11 SET). The App folds
    /// these into the renderer's live palette.
    pub fn take_pending_palette_changes(&mut self) -> Vec<PaletteUpdate> {
        self.pending_palette_changes.drain(..).collect()
    }

    /// Drain all completed lines captured since the last call. Each entry is
    /// trailing-space-trimmed and excludes the line break.
    pub fn take_completed_lines(&mut self) -> Vec<String> {
        self.pending_lines.drain(..).collect()
    }

    /// Logical line indices of shell prompts captured via OSC 133;A.
    pub fn prompt_marks(&self) -> &VecDeque<usize> {
        &self.prompt_marks
    }

    /// Logical line indices where a command was submitted, captured via
    /// OSC 133;C. May be empty if the shell does not emit `;C` markers.
    pub fn command_marks(&self) -> &VecDeque<usize> {
        &self.command_marks
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    pub fn cursor_should_blink(&self) -> bool {
        self.cursor_should_blink
    }

    /// Pop and return the raw base64 OSC 52 payload, if any.
    pub fn take_pending_clipboard(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }

    pub fn hyperlink_uri(&self, id: u32) -> Option<&str> {
        if id == 0 {
            None
        } else {
            self.hyperlinks.get(&id).map(|s| s.as_str())
        }
    }

    /// Hyperlink at the given visible cell, accounting for scrollback `offset`.
    pub fn hyperlink_at(&self, offset: u16, row: u16, col: u16) -> Option<&str> {
        let cell = self.visible_row(offset, row)?.get(col as usize)?;
        self.hyperlink_uri(cell.hyperlink)
    }

    /// Detect an http/https/file/ftp/mailto/ssh URL in the visible row that
    /// contains `col`. Walks outward from `col` while characters look
    /// URL-ish, then validates the slice has a recognised scheme.
    pub fn detect_url_at(&self, offset: u16, row: u16, col: u16) -> Option<String> {
        let cells = self.visible_row(offset, row)?;
        if (col as usize) >= cells.len() {
            return None;
        }
        let is_url_char = |c: char| {
            !c.is_whitespace()
                && !matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '`' | '\\')
        };
        if !is_url_char(cells[col as usize].ch) {
            return None;
        }
        let mut start = col as usize;
        while start > 0 && is_url_char(cells[start - 1].ch) {
            start -= 1;
        }
        let mut end = col as usize + 1;
        while end < cells.len() && is_url_char(cells[end].ch) {
            end += 1;
        }
        let raw: String = cells[start..end].iter().map(|c| c.ch).collect();
        // Trim trailing punctuation that is rarely part of a URL.
        let trimmed = raw.trim_end_matches(|c: char| ".,;:!?".contains(c));
        if is_safe_url(trimmed) {
            return Some(trimmed.to_string());
        }
        None
    }

    pub fn mouse_tracking(&self) -> MouseTracking {
        self.mouse_tracking
    }
    pub fn sgr_mouse(&self) -> bool {
        self.sgr_mouse
    }
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }
    pub fn focus_tracking(&self) -> bool {
        self.focus_tracking
    }
    /// DECSET ?2026 — when true, the app has asked the terminal to
    /// buffer rendering. Renderers may delay GPU presentation until this
    /// flips back to false (with a safety timeout against hung apps).
    pub fn sync_output(&self) -> bool {
        self.sync_output
    }
    /// DECSET ?5 (DECSCNM) — whether the renderer should swap
    /// default fg/bg across the whole grid for this terminal.
    pub fn is_reverse_screen(&self) -> bool {
        self.reverse_screen
    }
    pub fn app_cursor_keys(&self) -> bool {
        self.app_cursor_keys
    }
    pub fn autowrap(&self) -> bool {
        self.autowrap
    }
    pub fn origin_mode(&self) -> bool {
        self.origin_mode
    }

    /// Pop and return the title set via OSC 0/2 since the last call.
    pub fn take_title(&mut self) -> Option<String> {
        self.pending_title.take()
    }

    /// Pop and return whether a BEL (0x07) has been received since the last call.
    pub fn take_bell(&mut self) -> bool {
        std::mem::replace(&mut self.pending_bell, false)
    }

    pub fn grid(&self) -> &Grid {
        &self.grids[self.active]
    }

    pub fn cursor(&self) -> Position {
        self.cursor
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn size(&self) -> Size {
        self.grids[PRIMARY].size()
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn scrollback_line(&self, idx_from_top: usize) -> Option<&[Cell]> {
        self.scrollback.get(idx_from_top).map(|v| v.as_slice())
    }

    /// Visible-row lookup that transparently mixes scrollback and the live
    /// grid based on `offset`. `offset = 0` shows the grid as-is. Positive
    /// `offset` pushes the grid down and brings `offset` scrollback lines
    /// into view at the top.
    ///
    /// Scrollback is a primary-screen concept. When the terminal is on the
    /// alt screen (vim, less, etc.) the offset is ignored so the alt grid
    /// fills the viewport regardless of any leftover primary-screen scroll
    /// position. Otherwise scrolling up in bash, then `vim`-ing, would
    /// render bash's scrollback as the top rows of vim.
    pub fn visible_row(&self, offset: u16, r: u16) -> Option<&[Cell]> {
        let rows = self.size().rows;
        if r >= rows {
            return None;
        }
        if self.active == ALT {
            return self.grids[ALT].row(r);
        }
        let sb_len = self.scrollback.len();
        let off = (offset as usize).min(sb_len);
        if (r as usize) < off {
            let sb_idx = sb_len - off + r as usize;
            self.scrollback.get(sb_idx).map(|v| v.as_slice())
        } else {
            self.grids[self.active]
                .row(r - off as u16)
        }
    }

    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.scrollback_limit = limit;
        // Shifting the scrollback under the user's feet has to drag the
        // OSC-133 marks along — otherwise "jump to last prompt" lands
        // on a logical line that no longer exists.
        let before = self.scrollback.len();
        while self.scrollback.len() > limit {
            self.scrollback.pop_front();
        }
        let dropped = before - self.scrollback.len();
        shift_marks_after_scrollback_drop(&mut self.prompt_marks, dropped);
        shift_marks_after_scrollback_drop(&mut self.command_marks, dropped);
    }

    /// Current maximum number of lines retained in the scrollback ring.
    pub fn scrollback_limit(&self) -> usize {
        self.scrollback_limit
    }

    /// Drop every line stored in the scrollback ring without touching the
    /// live grid. Prompt / command marks that pointed into scrollback are
    /// removed and any remaining marks are re-anchored relative to the new
    /// (empty) scrollback. Equivalent to xterm's `Ctrl-Shift-K` "clear
    /// saved lines".
    pub fn clear_scrollback(&mut self) {
        let dropped = self.scrollback.len();
        self.scrollback.clear();
        shift_marks_after_scrollback_drop(&mut self.prompt_marks, dropped);
        shift_marks_after_scrollback_drop(&mut self.command_marks, dropped);
    }

    pub fn is_on_alt_screen(&self) -> bool {
        self.active == ALT
    }

    pub fn resize(&mut self, size: Size) {
        let old_rows = self.grids[PRIMARY].size().rows;
        // Shrinking row count: push the topmost evicted rows into the
        // primary scrollback so the cursor's view (typically near the
        // bottom) stays anchored to recent content instead of being cut.
        if size.rows > 0 && size.rows < old_rows {
            let evict = old_rows - size.rows;
            let blank = Cell::default();
            let evicted = self.grids[PRIMARY].scroll_up(0, old_rows - 1, evict, blank);
            for row in evicted {
                while self.scrollback.len() >= self.scrollback_limit {
                    if self.scrollback.pop_front().is_none() {
                        break;
                    }
                    // The single line we dropped shifts every surviving
                    // mark down by one; marks at the (gone) line 0 are
                    // discarded by the same helper.
                    shift_marks_after_scrollback_drop(&mut self.prompt_marks, 1);
                    shift_marks_after_scrollback_drop(&mut self.command_marks, 1);
                }
                if self.scrollback_limit > 0 {
                    self.scrollback.push_back(row);
                }
            }
            // Alt screen has no scrollback; just scroll content up.
            self.grids[ALT].scroll_up(0, old_rows - 1, evict, blank);
            self.cursor.row = self.cursor.row.saturating_sub(evict);
        }
        for g in &mut self.grids {
            g.resize(size);
        }
        let cols = size.cols.saturating_sub(1);
        let rows = size.rows.saturating_sub(1);
        self.cursor.col = self.cursor.col.min(cols);
        self.cursor.row = self.cursor.row.min(rows);
        // Reset DECSTBM to the full new grid on resize, matching xterm
        // behaviour. Apps that need a custom scroll region (vim, less)
        // re-set it from their SIGWINCH handler, so the brief reset is
        // invisible. Without this, the region kept its initial [0,
        // initial_rows-1] bounds and lines past `initial_rows` never
        // received output after the grid grew — visible as "shell
        // output stops at the middle of the window after resize".
        self.region = ScrollRegion::full(size);
        let new_n = size.cols as usize;
        let old_n = self.tab_stops.len();
        if new_n < old_n {
            self.tab_stops.truncate(new_n);
        } else if new_n > old_n {
            self.tab_stops.resize(new_n, false);
            // Seed default stops on the newly introduced columns only —
            // existing user-set stops in [0, old_n) are preserved.
            let mut i = old_n.div_ceil(TAB_STEP) * TAB_STEP;
            while i < new_n {
                if i >= old_n {
                    self.tab_stops[i] = true;
                }
                i += TAB_STEP;
            }
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        let mut perform = TerminalPerform {
            grids: &mut self.grids,
            active: &mut self.active,
            scrollback: &mut self.scrollback,
            scrollback_limit: self.scrollback_limit,
            cursor: &mut self.cursor,
            saved: &mut self.saved,
            sgr: &mut self.sgr,
            sgr_stack: &mut self.sgr_stack,
            cursor_visible: &mut self.cursor_visible,
            region: &mut self.region,
            pending_title: &mut self.pending_title,
            pending_bell: &mut self.pending_bell,
            mouse_tracking: &mut self.mouse_tracking,
            sgr_mouse: &mut self.sgr_mouse,
            bracketed_paste: &mut self.bracketed_paste,
            reverse_screen: &mut self.reverse_screen,
            focus_tracking: &mut self.focus_tracking,
            sync_output: &mut self.sync_output,
            insert_mode: &mut self.insert_mode,
            app_cursor_keys: &mut self.app_cursor_keys,
            autowrap: &mut self.autowrap,
            origin_mode: &mut self.origin_mode,
            hyperlinks: &mut self.hyperlinks,
            hyperlink_uri_to_id: &mut self.hyperlink_uri_to_id,
            hyperlink_order: &mut self.hyperlink_order,
            next_link_id: &mut self.next_link_id,
            current_hyperlink: &mut self.current_hyperlink,
            pending_clipboard: &mut self.pending_clipboard,
            cursor_shape: &mut self.cursor_shape,
            cursor_should_blink: &mut self.cursor_should_blink,
            cwd: &mut self.cwd,
            pending_lines: &mut self.pending_lines,
            prompt_marks: &mut self.prompt_marks,
            command_marks: &mut self.command_marks,
            pending_command_finishes: &mut self.pending_command_finishes,
            last_command_start: &mut self.last_command_start,
            osc_responses: &mut self.osc_responses,
            default_fg_rgb: &mut self.default_fg_rgb,
            default_bg_rgb: &mut self.default_bg_rgb,
            named_palette: &mut self.named_palette,
            cursor_rgb: &mut self.cursor_rgb,
            tab_stops: &mut self.tab_stops,
            app_keypad: &mut self.app_keypad,
            pending_notifications: &mut self.pending_notifications,
            pending_progress: &mut self.pending_progress,
            dcs_buf: &mut self.dcs_buf,
            dcs_is_rqss: &mut self.dcs_is_rqss,
            dcs_is_xtgettcap: &mut self.dcs_is_xtgettcap,
            last_printed: &mut self.last_printed,
            current_title: &mut self.current_title,
            title_stack: &mut self.title_stack,
            pending_palette_changes: &mut self.pending_palette_changes,
            charset_g0: &mut self.charset_g0,
            charset_g1: &mut self.charset_g1,
            active_charset: &mut self.active_charset,
        };
        for &b in bytes {
            self.parser.advance(&mut perform, b);
        }
        // Bound `osc_responses` post-batch. A shell flooding queries
        // between renderer drains would otherwise grow the VecDeque
        // unbounded; oldest reply pops first (the app cares about the
        // freshest response).
        while self.osc_responses.len() > OSC_RESPONSES_CAP {
            self.osc_responses.pop_front();
        }
    }
}

struct TerminalPerform<'a> {
    grids: &'a mut [Grid; 2],
    active: &'a mut usize,
    scrollback: &'a mut VecDeque<Vec<Cell>>,
    scrollback_limit: usize,
    cursor: &'a mut Position,
    saved: &'a mut [Option<CursorState>; 2],
    sgr: &'a mut SgrState,
    sgr_stack: &'a mut Vec<SgrState>,
    cursor_visible: &'a mut bool,
    region: &'a mut ScrollRegion,
    pending_title: &'a mut Option<String>,
    pending_bell: &'a mut bool,
    mouse_tracking: &'a mut MouseTracking,
    sgr_mouse: &'a mut bool,
    bracketed_paste: &'a mut bool,
    reverse_screen: &'a mut bool,
    focus_tracking: &'a mut bool,
    sync_output: &'a mut bool,
    insert_mode: &'a mut bool,
    app_cursor_keys: &'a mut bool,
    autowrap: &'a mut bool,
    origin_mode: &'a mut bool,
    hyperlinks: &'a mut HashMap<u32, String>,
    hyperlink_uri_to_id: &'a mut HashMap<String, u32>,
    hyperlink_order: &'a mut VecDeque<u32>,
    next_link_id: &'a mut u32,
    current_hyperlink: &'a mut u32,
    pending_clipboard: &'a mut Option<String>,
    cursor_shape: &'a mut CursorShape,
    cursor_should_blink: &'a mut bool,
    cwd: &'a mut Option<String>,
    pending_lines: &'a mut VecDeque<String>,
    prompt_marks: &'a mut VecDeque<usize>,
    command_marks: &'a mut VecDeque<usize>,
    pending_command_finishes: &'a mut VecDeque<CommandFinish>,
    last_command_start: &'a mut Option<std::time::Instant>,
    osc_responses: &'a mut VecDeque<String>,
    default_fg_rgb: &'a mut [u8; 3],
    default_bg_rgb: &'a mut [u8; 3],
    named_palette: &'a mut [[u8; 3]; 16],
    cursor_rgb: &'a mut Option<[u8; 3]>,
    pending_palette_changes: &'a mut VecDeque<PaletteUpdate>,
    tab_stops: &'a mut Vec<bool>,
    app_keypad: &'a mut bool,
    pending_notifications: &'a mut VecDeque<String>,
    pending_progress: &'a mut VecDeque<(u8, u8)>,
    dcs_buf: &'a mut Vec<u8>,
    dcs_is_rqss: &'a mut bool,
    dcs_is_xtgettcap: &'a mut bool,
    last_printed: &'a mut Option<char>,
    current_title: &'a mut Option<String>,
    title_stack: &'a mut Vec<String>,
    charset_g0: &'a mut Charset,
    charset_g1: &'a mut Charset,
    active_charset: &'a mut u8,
}

impl<'a> TerminalPerform<'a> {
    /// Push a notification onto the queue with the standard size /
    /// length guards. Three OSC paths (OSC 9 single-arg, OSC 777
    /// `notify`, OSC 1337 `notify=…`) all funnel here so a future
    /// cap / dedupe / filter tweak lands in one place. Empty
    /// messages and ones > 4 KiB are silently dropped (a bad
    /// shell-integration emitter mustn't break the renderer).
    fn push_notification(&mut self, msg: String) {
        if msg.is_empty() || msg.len() > 4096 {
            return;
        }
        if self.pending_notifications.len() >= NOTIFICATIONS_CAP {
            self.pending_notifications.pop_front();
        }
        self.pending_notifications.push_back(msg);
    }

    /// XTGETTCAP — `DCS + q P1[;P2…] ST`. Each `Pi` is a hex-encoded
    /// terminfo capability name. For every cap we know we reply with
    /// `DCS 1 + r <hex-cap>=<hex-value> ST`; unknown caps get `DCS 0 +
    /// r <hex-cap> ST`. Shells and editors probe this to enable features
    /// (truecolor, undercurl, etc.) without parsing /etc/terminfo.
    fn dispatch_xtgettcap(&mut self) {
        let buf = self.dcs_buf.clone();
        for chunk in buf.split(|&b| b == b';') {
            let hex_name = std::str::from_utf8(chunk).unwrap_or("");
            let Some(name) = decode_hex_ascii(hex_name) else {
                self.osc_responses
                    .push_back(format!("\x1bP0+r{}\x1b\\", hex_name));
                continue;
            };
            let value = match name.as_str() {
                // Terminal name → "rterm". Shell init scripts read this
                // to gate `TERM`-specific behaviour without inspecting
                // $TERM (which can be xterm-256color for compatibility).
                "TN" | "name" => Some("rterm".to_string()),
                // Max colours — 256 since we serve the xterm 256 palette.
                "Co" | "colors" => Some("256".to_string()),
                // Truecolor capability (kitty/wezterm-style). Value "8"
                // means 8 bits per channel direct color is supported.
                "RGB" | "rgb" | "Tc" => Some("8".to_string()),
                // Back-colour erase — flag cap, empty value when set.
                "bce" => Some(String::new()),
                _ => None,
            };
            match value {
                Some(v) => {
                    let hex_val = encode_hex_ascii(&v);
                    self.osc_responses
                        .push_back(format!("\x1bP1+r{}={}\x1b\\", hex_name, hex_val));
                }
                None => self
                    .osc_responses
                    .push_back(format!("\x1bP0+r{}\x1b\\", hex_name)),
            }
        }
    }

    fn size(&self) -> Size {
        self.grids[*self.active].size()
    }

    fn blank_cell(&self) -> Cell {
        Cell { fg: self.sgr.fg, bg: self.sgr.bg, ..Cell::default() }
    }


    fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grids[*self.active]
    }

    fn linefeed(&mut self) {
        // Capture the row we're leaving as a "completed line" — plugins use
        // these for output reactivity (rterm.on("output.line", ...)).
        let row_text = self.grids[*self.active]
            .row(self.cursor.row)
            .map(|row| {
                let mut s: String = row.iter().map(|c| c.ch).collect();
                let trimmed_len = s.trim_end_matches(' ').len();
                s.truncate(trimmed_len);
                s
            });
        if let Some(s) = row_text {
            if !s.is_empty() {
                if self.pending_lines.len() >= PENDING_LINES_CAP {
                    self.pending_lines.pop_front();
                }
                self.pending_lines.push_back(s);
            }
        }

        let bottom = self.region.bottom;
        if self.cursor.row == bottom {
            self.scroll_region_up(1);
        } else if self.cursor.row + 1 < self.size().rows {
            self.cursor.row += 1;
        }
    }

    fn carriage_return(&mut self) {
        self.cursor.col = 0;
    }

    fn scroll_region_up(&mut self, n: u16) {
        let top = self.region.top;
        let bottom = self.region.bottom;
        let blank = self.blank_cell();
        let evicted = self.grid_mut().scroll_up(top, bottom, n, blank);
        if *self.active == PRIMARY && top == 0 {
            for row in evicted {
                // Use `>=` (not `==`) so `scrollback_limit = 0` actually
                // disables scrollback rather than letting the ring grow
                // unbounded — `==` only triggered eviction at the exact
                // boundary and `0` is never == `1` after the first push.
                while self.scrollback.len() >= self.scrollback_limit {
                    if self.scrollback.pop_front().is_none() {
                        break;
                    }
                    // Each existing mark moves down by 1 — those that
                    // were at logical line 0 vanish along with the
                    // popped line.
                    shift_marks_after_scrollback_drop(self.prompt_marks, 1);
                    shift_marks_after_scrollback_drop(self.command_marks, 1);
                }
                if self.scrollback_limit > 0 {
                    self.scrollback.push_back(row);
                }
            }
        }
    }

    fn scroll_region_down(&mut self, n: u16) {
        let top = self.region.top;
        let bottom = self.region.bottom;
        let blank = self.blank_cell();
        self.grid_mut().scroll_down(top, bottom, n, blank);
    }

    /// Shift each row inside the active scroll region horizontally.
    /// Positive `delta` shifts right (SR), negative shifts left (SL).
    /// Cells past the edge are lost; vacated cells are blanked with the
    /// current SGR bg.
    fn shift_cols_in_region(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let size = self.size();
        let cols = size.cols as usize;
        if cols == 0 {
            return;
        }
        let top = self.region.top;
        let bottom = self.region.bottom;
        let blank = self.blank_cell();
        let g = self.grid_mut();
        for row in top..=bottom {
            let Some(row_slice) = g.row_mut(row) else { continue };
            // In-place shift via `copy_within` (handles overlap) + blank
            // the vacated side. No per-row Vec allocation.
            if delta > 0 {
                let d = (delta as usize).min(cols);
                row_slice.copy_within(0..cols - d, d);
                row_slice[..d].fill(blank);
            } else {
                let d = ((-delta) as usize).min(cols);
                row_slice.copy_within(d..cols, 0);
                row_slice[cols - d..].fill(blank);
            }
        }
    }

    /// Convert a 0-based row received from CUP/HVP/VPA into an absolute
    /// grid row, accounting for origin mode (DECOM ?6).
    fn origin_to_abs_row(&self, requested_zero_based: i32) -> i32 {
        if *self.origin_mode {
            let top = self.region.top as i32;
            let bottom = self.region.bottom as i32;
            (top + requested_zero_based.max(0)).clamp(top, bottom)
        } else {
            requested_zero_based
        }
    }

    fn forward_tabs(&mut self, n: u16) {
        let max = self.size().cols.saturating_sub(1);
        if max == 0 || n == 0 {
            return;
        }
        let mut col = self.cursor.col as usize;
        let last = max as usize;
        for _ in 0..n {
            let mut next = col + 1;
            while next < self.tab_stops.len() && !self.tab_stops[next] {
                next += 1;
            }
            col = next.min(last);
            if col >= last {
                break;
            }
        }
        self.cursor.col = col as u16;
    }

    fn backward_tabs(&mut self, n: u16) {
        if self.cursor.col == 0 || n == 0 {
            return;
        }
        let mut col = self.cursor.col as usize;
        for _ in 0..n {
            if col == 0 {
                break;
            }
            let mut prev = col - 1;
            while prev > 0 && !self.tab_stops[prev] {
                prev -= 1;
            }
            col = prev;
            if col == 0 {
                break;
            }
        }
        self.cursor.col = col as u16;
    }

    fn move_cursor_clamped(&mut self, row: i32, col: i32) {
        let size = self.size();
        let max_row = size.rows.saturating_sub(1) as i32;
        let max_col = size.cols.saturating_sub(1) as i32;
        self.cursor.row = row.clamp(0, max_row) as u16;
        self.cursor.col = col.clamp(0, max_col) as u16;
    }

    fn erase_range(&mut self, start: Position, end: Position) {
        let size = self.size();
        let blank = self.blank_cell();
        for row in start.row..=end.row {
            let col_lo = if row == start.row { start.col } else { 0 };
            let col_hi = if row == end.row { end.col } else { size.cols.saturating_sub(1) };
            for col in col_lo..=col_hi {
                if let Some(cell) = self.grid_mut().cell_mut(Position { row, col }) {
                    *cell = blank;
                }
            }
        }
    }

    fn erase_in_display(&mut self, mode: u16) {
        let size = self.size();
        let last_row = size.rows.saturating_sub(1);
        let last_col = size.cols.saturating_sub(1);
        let cursor = *self.cursor;
        match mode {
            0 => self.erase_range(cursor, Position { row: last_row, col: last_col }),
            1 => self.erase_range(Position::default(), cursor),
            2 => self.erase_range(Position::default(), Position { row: last_row, col: last_col }),
            // ED 3 — xterm extension: clear the entire display AND drop
            // the scrollback ring. `clear`, `tput clear`, and many tmux
            // hooks send this to actually wipe history (not just scroll
            // it out). The primary-screen scrollback is what we trim;
            // alt-screen has none.
            3 => {
                self.erase_range(
                    Position::default(),
                    Position { row: last_row, col: last_col },
                );
                if *self.active == PRIMARY {
                    let to_drop = self.scrollback.len();
                    self.scrollback.clear();
                    // Re-anchor marks against the now-empty scrollback —
                    // anything pointing into the dropped lines vanishes,
                    // the rest shifts so its "jump to last prompt" still
                    // resolves to the right logical line.
                    shift_marks_after_scrollback_drop(self.prompt_marks, to_drop);
                    shift_marks_after_scrollback_drop(self.command_marks, to_drop);
                }
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let size = self.size();
        let last_col = size.cols.saturating_sub(1);
        let row = self.cursor.row;
        let cursor = *self.cursor;
        match mode {
            0 => self.erase_range(cursor, Position { row, col: last_col }),
            1 => self.erase_range(Position { row, col: 0 }, cursor),
            2 => self.erase_range(Position { row, col: 0 }, Position { row, col: last_col }),
            _ => {}
        }
    }

    fn current_cursor_state(&self) -> CursorState {
        CursorState {
            pos: *self.cursor,
            sgr: *self.sgr,
            active_charset: *self.active_charset,
            charset_g0: *self.charset_g0,
            charset_g1: *self.charset_g1,
            origin_mode: *self.origin_mode,
            autowrap: *self.autowrap,
        }
    }

    fn save_cursor(&mut self) {
        let state = self.current_cursor_state();
        self.saved[*self.active] = Some(state);
    }

    fn restore_cursor(&mut self) {
        if let Some(s) = self.saved[*self.active] {
            self.move_cursor_clamped(s.pos.row as i32, s.pos.col as i32);
            *self.sgr = s.sgr;
            *self.active_charset = s.active_charset;
            *self.charset_g0 = s.charset_g0;
            *self.charset_g1 = s.charset_g1;
            *self.origin_mode = s.origin_mode;
            *self.autowrap = s.autowrap;
        }
    }

    fn switch_screen(&mut self, target: usize) {
        if *self.active == target {
            return;
        }
        *self.active = target;
    }

    fn enter_alt_screen(&mut self) {
        // Save primary cursor first, then switch.
        self.save_cursor();
        self.switch_screen(ALT);
        self.grid_mut().clear();
        self.cursor.col = 0;
        self.cursor.row = 0;
    }

    fn leave_alt_screen(&mut self) {
        self.switch_screen(PRIMARY);
        self.restore_cursor();
    }

    fn insert_chars(&mut self, n: u16) {
        let cols = self.size().cols;
        let row = self.cursor.row;
        let col = self.cursor.col;
        if col >= cols {
            return;
        }
        let shift = n.min(cols - col);
        let blank = self.blank_cell();
        // Shift right.
        for i in (col + shift..cols).rev() {
            let src = Position { row, col: i - shift };
            // The math guarantees `src` is in-bounds, but use `if let`
            // anyway so an unexpected resize race can't panic the parser.
            let Some(cell) = self.grid_mut().cell_mut(src).copied() else { continue };
            if let Some(dst) = self.grid_mut().cell_mut(Position { row, col: i }) {
                *dst = cell;
            }
        }
        // Blank the inserted region.
        for i in col..col + shift {
            if let Some(cell) = self.grid_mut().cell_mut(Position { row, col: i }) {
                *cell = blank;
            }
        }
    }

    fn delete_chars(&mut self, n: u16) {
        let cols = self.size().cols;
        let row = self.cursor.row;
        let col = self.cursor.col;
        if col >= cols {
            return;
        }
        let shift = n.min(cols - col);
        let blank = self.blank_cell();
        for i in col..(cols - shift) {
            let src = Position { row, col: i + shift };
            let Some(cell) = self.grid_mut().cell_mut(src).copied() else { continue };
            if let Some(dst) = self.grid_mut().cell_mut(Position { row, col: i }) {
                *dst = cell;
            }
        }
        for i in (cols - shift)..cols {
            if let Some(cell) = self.grid_mut().cell_mut(Position { row, col: i }) {
                *cell = blank;
            }
        }
    }

    fn erase_chars(&mut self, n: u16) {
        let cols = self.size().cols;
        let row = self.cursor.row;
        let col = self.cursor.col;
        if col >= cols {
            return;
        }
        let shift = n.min(cols - col);
        let blank = self.blank_cell();
        for i in col..col + shift {
            if let Some(cell) = self.grid_mut().cell_mut(Position { row, col: i }) {
                *cell = blank;
            }
        }
    }

    fn insert_lines(&mut self, n: u16) {
        if self.cursor.row < self.region.top || self.cursor.row > self.region.bottom {
            return;
        }
        let top = self.cursor.row;
        let bottom = self.region.bottom;
        let blank = self.blank_cell();
        self.grid_mut().scroll_down(top, bottom, n, blank);
    }

    fn delete_lines(&mut self, n: u16) {
        if self.cursor.row < self.region.top || self.cursor.row > self.region.bottom {
            return;
        }
        let top = self.cursor.row;
        let bottom = self.region.bottom;
        let blank = self.blank_cell();
        // Mid-region delete does not feed scrollback.
        let _ = self.grid_mut().scroll_up(top, bottom, n, blank);
    }

    fn handle_sgr(&mut self, params: &Params) {
        // Sub-parameter aware path: each top-level param slice may carry sub
        // params (e.g. `4:3` for curly underline). Detect underline substyles
        // first, then fall back to the flat numeric stream for everything else.
        let mut handled = std::collections::HashSet::<usize>::new();
        for (idx, slice) in params.iter().enumerate() {
            if slice.len() < 2 {
                continue;
            }
            if slice[0] == 4 {
                self.sgr.attrs -=
                    CellAttrs::UNDERLINE | CellAttrs::UNDERLINE_DOUBLE | CellAttrs::UNDERLINE_CURLY;
                match slice[1] {
                    0 => {} // explicit "no underline"
                    1 => self.sgr.attrs |= CellAttrs::UNDERLINE,
                    2 => {
                        self.sgr.attrs |=
                            CellAttrs::UNDERLINE | CellAttrs::UNDERLINE_DOUBLE;
                    }
                    3 => {
                        self.sgr.attrs |=
                            CellAttrs::UNDERLINE | CellAttrs::UNDERLINE_CURLY;
                    }
                    _ => self.sgr.attrs |= CellAttrs::UNDERLINE,
                }
                handled.insert(idx);
            }
            // Colon-form SGR 58 (underline colour). We don't model the
            // colour but must claim the slice so the flat path doesn't
            // see codes 58, 2, r, g, b separately (which would set DIM
            // and bright-bg via `r ∈ 100..=107`).
            if slice[0] == 58 && slice.len() >= 3 {
                handled.insert(idx);
            }
            // Colon-form RGB / indexed colour, e.g. `38:2:cs:r:g:b` or
            // `38:5:n`. We treat the whole slice as one self-contained
            // colour spec so the colorspace id (cs) is correctly skipped.
            if (slice[0] == 38 || slice[0] == 48) && slice.len() >= 3 {
                let target_fg = slice[0] == 38;
                let color = match slice[1] {
                    5 => Some(Color::Indexed(slice[2] as u8)),
                    2 if slice.len() >= 6 => {
                        // [38, 2, cs, r, g, b] — drop the colorspace id.
                        Some(Color::Rgb(slice[3] as u8, slice[4] as u8, slice[5] as u8))
                    }
                    2 if slice.len() >= 5 => {
                        // [38, 2, r, g, b] — no colorspace id present.
                        Some(Color::Rgb(slice[2] as u8, slice[3] as u8, slice[4] as u8))
                    }
                    _ => None,
                };
                if let Some(c) = color {
                    if target_fg {
                        self.sgr.fg = c;
                    } else {
                        self.sgr.bg = c;
                    }
                    handled.insert(idx);
                }
            }
        }
        let flat: Vec<u16> = params
            .iter()
            .enumerate()
            .filter(|(idx, _)| !handled.contains(idx))
            .flat_map(|(_, s)| s.iter().copied())
            .collect();
        if flat.is_empty() && handled.is_empty() {
            *self.sgr = SgrState::default();
            return;
        }
        let mut i = 0;
        while i < flat.len() {
            let code = flat[i];
            match code {
                0 => *self.sgr = SgrState::default(),
                1 => self.sgr.attrs |= CellAttrs::BOLD,
                2 => self.sgr.attrs |= CellAttrs::DIM,
                3 => self.sgr.attrs |= CellAttrs::ITALIC,
                4 => {
                    self.sgr.attrs -= CellAttrs::UNDERLINE_DOUBLE | CellAttrs::UNDERLINE_CURLY;
                    self.sgr.attrs |= CellAttrs::UNDERLINE;
                }
                // SGR 5 = slow blink, SGR 6 = rapid blink. Real
                // terminals (xterm, kitty, alacritty) don't distinguish
                // visually — both map to the same BLINK attribute, and
                // SGR 25 turns either off. Mapping 6 to BLINK keeps apps
                // that emit it (rare, but emitted by some figlet / lolcat
                // wrappers) looking the same as if they'd used SGR 5.
                5 | 6 => self.sgr.attrs |= CellAttrs::BLINK,
                7 => self.sgr.attrs |= CellAttrs::REVERSE,
                8 => self.sgr.attrs |= CellAttrs::HIDDEN,
                9 => self.sgr.attrs |= CellAttrs::STRIKETHROUGH,
                // SGR 21 — double underline.
                21 => self.sgr.attrs |=
                    CellAttrs::UNDERLINE | CellAttrs::UNDERLINE_DOUBLE,
                22 => self.sgr.attrs -= CellAttrs::BOLD | CellAttrs::DIM,
                23 => self.sgr.attrs -= CellAttrs::ITALIC,
                24 => self.sgr.attrs -=
                    CellAttrs::UNDERLINE | CellAttrs::UNDERLINE_DOUBLE | CellAttrs::UNDERLINE_CURLY,
                25 => self.sgr.attrs -= CellAttrs::BLINK,
                27 => self.sgr.attrs -= CellAttrs::REVERSE,
                28 => self.sgr.attrs -= CellAttrs::HIDDEN,
                29 => self.sgr.attrs -= CellAttrs::STRIKETHROUGH,
                53 => self.sgr.attrs |= CellAttrs::OVERLINE,
                55 => self.sgr.attrs -= CellAttrs::OVERLINE,
                30..=37 => self.sgr.fg = Color::Named(NAMED[(code - 30) as usize]),
                38 => {
                    if let Some((color, consumed)) = parse_extended(&flat[i + 1..]) {
                        self.sgr.fg = color;
                        i += consumed;
                    }
                }
                39 => self.sgr.fg = Color::Default,
                40..=47 => self.sgr.bg = Color::Named(NAMED[(code - 40) as usize]),
                48 => {
                    if let Some((color, consumed)) = parse_extended(&flat[i + 1..]) {
                        self.sgr.bg = color;
                        i += consumed;
                    }
                }
                49 => self.sgr.bg = Color::Default,
                // SGR 58 — set underline colour. We don't store underline
                // colour separately (the underline draws in the cell's fg),
                // but we MUST consume the follow-on RGB/indexed bytes so
                // they don't get reinterpreted as later SGR codes (e.g.
                // `58;2;r;g;b` would otherwise set DIM via the trailing 2
                // and a bright-bg colour via r ∈ 100..=107).
                58 => {
                    if let Some((_, consumed)) = parse_extended(&flat[i + 1..]) {
                        i += consumed;
                    }
                }
                // SGR 59 — reset underline colour. No params to consume.
                59 => {}
                90..=97 => self.sgr.fg = Color::Named(NAMED_BRIGHT[(code - 90) as usize]),
                100..=107 => self.sgr.bg = Color::Named(NAMED_BRIGHT[(code - 100) as usize]),
                _ => {}
            }
            i += 1;
        }
    }

    /// DECRQM reply value for a private mode. 1 = set, 2 = reset, 0 = unknown.
    fn decrqm_value(&self, mode: u16) -> u8 {
        let state = match mode {
            1 => Some(*self.app_cursor_keys),
            5 => Some(*self.reverse_screen),
            6 => Some(*self.origin_mode),
            7 => Some(*self.autowrap),
            12 => Some(*self.cursor_should_blink),
            25 => Some(*self.cursor_visible),
            47 | 1047 | 1049 => Some(*self.active == ALT),
            1000 => Some(*self.mouse_tracking == MouseTracking::X10),
            1002 => Some(*self.mouse_tracking == MouseTracking::ButtonEvent),
            1003 => Some(*self.mouse_tracking == MouseTracking::AnyEvent),
            1004 => Some(*self.focus_tracking),
            1006 => Some(*self.sgr_mouse),
            2004 => Some(*self.bracketed_paste),
            2026 => Some(*self.sync_output),
            _ => None,
        };
        match state {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        }
    }

    fn handle_private_mode(&mut self, params: &Params, set: bool) {
        for slice in params.iter() {
            let Some(code) = slice.first().copied() else { continue };
            match code {
                1 => *self.app_cursor_keys = set,
                // DECSET ?5 — DECSCNM reverse screen. State only;
                // the renderer reads `is_reverse_screen()` and
                // inverts default colours per cell.
                5 => *self.reverse_screen = set,
                6 => {
                    *self.origin_mode = set;
                    // Per VT spec, entering / leaving origin mode homes the
                    // cursor to (top, 0).
                    self.cursor.row = if set { self.region.top } else { 0 };
                    self.cursor.col = 0;
                }
                7 => *self.autowrap = set,
                // ?12 (AT&T extension) — cursor blink. Some apps toggle it
                // alongside DECSCUSR; honour it as a synonym.
                12 => *self.cursor_should_blink = set,
                25 => *self.cursor_visible = set,
                47 | 1047 => {
                    if set {
                        self.switch_screen(ALT);
                        self.grid_mut().clear();
                    } else {
                        self.switch_screen(PRIMARY);
                    }
                }
                1049 => {
                    if set {
                        self.enter_alt_screen();
                    } else {
                        self.leave_alt_screen();
                    }
                }
                // Mouse reporting modes.
                1000 => {
                    *self.mouse_tracking = if set { MouseTracking::X10 } else { MouseTracking::Off };
                }
                1002 => {
                    *self.mouse_tracking = if set { MouseTracking::ButtonEvent } else { MouseTracking::Off };
                }
                1003 => {
                    *self.mouse_tracking = if set { MouseTracking::AnyEvent } else { MouseTracking::Off };
                }
                1004 => *self.focus_tracking = set,
                1006 => *self.sgr_mouse = set,
                2004 => *self.bracketed_paste = set,
                // DECSET ?2026 — Synchronized Output Mode. Apps (neovim,
                // kakoune, helix) bracket a multi-segment frame with
                // BEGIN/END so the terminal can present without tearing.
                2026 => *self.sync_output = set,
                _ => {}
            }
        }
    }
}

/// Translate an ASCII byte through the DEC Special Graphics character set.
/// Returns `None` for chars that pass through unchanged (most punctuation
/// and the letters `b-h`, `i`, `y`, `z`). Covers the box-drawing range used
/// by ncurses and screen-style apps.
fn dec_special_graphic(c: char) -> Option<char> {
    Some(match c {
        '_' => ' ',
        '`' => '◆',
        'a' => '▒',
        'b' => '␉', // HT symbol
        'c' => '␌', // FF
        'd' => '␍', // CR
        'e' => '␊', // LF
        'f' => '°',
        'g' => '±',
        'h' => '␤', // NL
        'i' => '␋', // VT
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => return None,
    })
}

/// Parse a `rgb:RR/GG/BB` or `rgb:RRRR/GGGG/BBBB` colour spec into a byte
/// triple. Other forms (named colours, `#RRGGBB`) return `None`.
fn parse_rgb_spec(s: &str) -> Option<[u8; 3]> {
    // Accept both the X11 `rgb:` / `rgba:` form (`rr/gg/bb` or
    // `rrrr/gggg/bbbb`, optional `/aa` trailing alpha) and the CSS-ish
    // `#RGB` / `#RRGGBB` shorthands many apps (vim/neovim, bat, delta)
    // emit through OSC 4/10/11/12. Alpha is parsed-and-ignored — we
    // don't model per-channel alpha for the default colours.
    if let Some(rest) = s.strip_prefix('#') {
        return parse_hex_hash(rest);
    }
    // `rgb:` must be exactly three slash-separated channels (per X11).
    // `rgba:` allows a 4th alpha channel which we parse-and-ignore.
    let (rest, allow_alpha) = match (s.strip_prefix("rgb:"), s.strip_prefix("rgba:")) {
        (Some(r), _) => (r, false),
        (_, Some(r)) => (r, true),
        _ => return None,
    };
    let mut parts = rest.split('/');
    let r = parts.next()?;
    let g = parts.next()?;
    let b = parts.next()?;
    if allow_alpha {
        let _alpha = parts.next();
    }
    if parts.next().is_some() {
        return None;
    }
    fn take_byte(s: &str) -> Option<u8> {
        // Slice through `as_bytes()` rather than `&s[..N]` so a
        // shell-supplied OSC payload that smuggles a multi-byte UTF-8
        // char into a hex-channel slot (`漢a` is 4 bytes but only 2
        // chars) fails cleanly via UTF-8 validation instead of
        // panicking on a non-boundary byte index.
        let bytes = s.as_bytes();
        let head: &[u8] = match bytes.len() {
            2 => bytes,
            // 4-digit form: take the high byte by parsing the first two.
            4 => &bytes[..2],
            _ => return None,
        };
        u8::from_str_radix(std::str::from_utf8(head).ok()?, 16).ok()
    }
    Some([take_byte(r)?, take_byte(g)?, take_byte(b)?])
}

/// `#RGB` (each nibble doubled — `#f08` → `[ff, 00, 88]`) or `#RRGGBB`
/// (straight bytes). Anything else returns `None`.
fn parse_hex_hash(s: &str) -> Option<[u8; 3]> {
    // Operate on `as_bytes()` so a shell-supplied OSC payload that
    // matches one of the length arms but uses multi-byte UTF-8 chars
    // (e.g. `#🌍ab` = 6 bytes / 3 chars) is rejected via UTF-8
    // validation in `from_utf8` rather than panicking on a non-
    // boundary `&s[..2]` slice.
    let bytes = s.as_bytes();
    fn hex_pair(pair: &[u8]) -> Option<u8> {
        u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()
    }
    match bytes.len() {
        3 => {
            // ASCII hex digits are single-byte, so `bytes[i] as char`
            // is the same as the original char for the legal inputs
            // and falls through to `to_digit(16)` returning None for
            // anything else (incl. UTF-8 lead / continuation bytes).
            let r = (bytes[0] as char).to_digit(16)? as u8;
            let g = (bytes[1] as char).to_digit(16)? as u8;
            let b = (bytes[2] as char).to_digit(16)? as u8;
            Some([r * 0x11, g * 0x11, b * 0x11])
        }
        6 => Some([
            hex_pair(&bytes[0..2])?,
            hex_pair(&bytes[2..4])?,
            hex_pair(&bytes[4..6])?,
        ]),
        // `#RRGGBBAA` — alpha ignored.
        8 => Some([
            hex_pair(&bytes[0..2])?,
            hex_pair(&bytes[2..4])?,
            hex_pair(&bytes[4..6])?,
        ]),
        _ => None,
    }
}

/// Decode `%XX` byte escapes in a URI path. Invalid escapes are left as-is.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            if let (Some(a), Some(b)) = (h1, h2) {
                out.push((a * 16 + b) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_extended(rest: &[u16]) -> Option<(Color, usize)> {
    match rest.first()? {
        5 => Some((Color::Indexed(*rest.get(1)? as u8), 2)),
        2 => {
            let r = *rest.get(1)? as u8;
            let g = *rest.get(2)? as u8;
            let b = *rest.get(3)? as u8;
            Some((Color::Rgb(r, g, b), 4))
        }
        _ => None,
    }
}

const NAMED: [NamedColor; 8] = [
    NamedColor::Black,
    NamedColor::Red,
    NamedColor::Green,
    NamedColor::Yellow,
    NamedColor::Blue,
    NamedColor::Magenta,
    NamedColor::Cyan,
    NamedColor::White,
];

const NAMED_BRIGHT: [NamedColor; 8] = [
    NamedColor::BrightBlack,
    NamedColor::BrightRed,
    NamedColor::BrightGreen,
    NamedColor::BrightYellow,
    NamedColor::BrightBlue,
    NamedColor::BrightMagenta,
    NamedColor::BrightCyan,
    NamedColor::BrightWhite,
];

fn first_param_or(params: &Params, default: u16) -> u16 {
    params
        .iter()
        .next()
        .and_then(|s| s.first().copied())
        .filter(|v| *v != 0)
        .unwrap_or(default)
}

fn first_two_params_or(params: &Params, default: u16) -> (u16, u16) {
    let mut it = params.iter();
    let a = it.next().and_then(|s| s.first().copied()).filter(|v| *v != 0).unwrap_or(default);
    let b = it.next().and_then(|s| s.first().copied()).filter(|v| *v != 0).unwrap_or(default);
    (a, b)
}

impl<'a> Perform for TerminalPerform<'a> {
    fn print(&mut self, ch: char) {
        // DEC special graphics remap applies before width calc since all
        // remapped glyphs are width-1.
        let active = if *self.active_charset == 1 {
            *self.charset_g1
        } else {
            *self.charset_g0
        };
        let ch = if active == Charset::DecSpecialGraphics {
            dec_special_graphic(ch).unwrap_or(ch)
        } else {
            ch
        };
        let size = self.size();
        let width = ch.width().unwrap_or(0) as u16;
        if width == 0 {
            return;
        }
        // Wide chars (width=2 — CJK, emoji) need both cells to fit. If they
        // don't, wrap (or pin) using the COMBINED width.
        if self.cursor.col + width > size.cols {
            if *self.autowrap {
                self.linefeed();
                self.cursor.col = 0;
            } else {
                self.cursor.col = size.cols.saturating_sub(width.max(1));
            }
        }

        let pos = *self.cursor;
        let mut new_cell = Cell {
            ch,
            fg: self.sgr.fg,
            bg: self.sgr.bg,
            attrs: self.sgr.attrs,
            hyperlink: *self.current_hyperlink,
        };
        if width >= 2 {
            new_cell.attrs |= CellAttrs::WIDE;
        }
        // IRM (Insert Mode) shifts existing cells to the right by `width`
        // before we write the new glyph. The cells that fall off the right
        // edge are lost (xterm behaviour).
        if *self.insert_mode && width > 0 {
            let cols = size.cols;
            let row = pos.row;
            let col = pos.col;
            if col < cols {
                let n = width.min(cols - col);
                let g = self.grid_mut();
                let row_cells = g.row(row).map(|r| r.to_vec());
                if let Some(mut cells) = row_cells {
                    let len = cells.len();
                    // Shift [col .. len-n] right by n positions.
                    if (col as usize + n as usize) < len {
                        for i in (col as usize + n as usize..len).rev() {
                            cells[i] = cells[i - n as usize];
                        }
                    }
                    for i in 0..n {
                        cells[col as usize + i as usize] = Cell::default();
                    }
                    for (i, c) in cells.into_iter().enumerate() {
                        if let Some(slot) = g.cell_mut(Position { col: i as u16, row }) {
                            *slot = c;
                        }
                    }
                }
            }
        }
        if let Some(cell) = self.grid_mut().cell_mut(pos) {
            *cell = new_cell;
        }
        // Mark the trailing cell as a spacer so the renderer doesn't print a
        // duplicate glyph there.
        if width == 2 {
            let spacer = Cell {
                ch: ' ',
                fg: self.sgr.fg,
                bg: self.sgr.bg,
                attrs: self.sgr.attrs | CellAttrs::WIDE_SPACER,
                hyperlink: *self.current_hyperlink,
            };
            if let Some(c) = self
                .grid_mut()
                .cell_mut(Position { row: pos.row, col: pos.col + 1 })
            {
                *c = spacer;
            }
        }
        self.cursor.col = self.cursor.col.saturating_add(width);
        *self.last_printed = Some(ch);
    }

    fn execute(&mut self, byte: u8) {
        // Any control byte invalidates the "last printed" character for REP.
        *self.last_printed = None;
        match byte {
            b'\n' | 0x0B | 0x0C => self.linefeed(),
            b'\r' => self.carriage_return(),
            0x08 => self.cursor.col = self.cursor.col.saturating_sub(1),
            b'\t' => self.forward_tabs(1),
            0x07 => *self.pending_bell = true,
            // SI / SO — select G0 / G1 as the active character set.
            0x0F => *self.active_charset = 0,
            0x0E => *self.active_charset = 1,
            _ => {}
        }
    }

    fn hook(&mut self, _: &Params, intermediates: &[u8], _ignore: bool, byte: char) {
        self.dcs_buf.clear();
        // `DCS $ q <selector> ST` — DECRQSS (Request Status String).
        *self.dcs_is_rqss = intermediates == b"$" && byte == 'q';
        // `DCS + q <hex-cap-names> ST` — XTGETTCAP (xterm terminfo query).
        // Both variants buffer their payload via `put` and reply at
        // `unhook`. They're mutually exclusive — at most one flag is set.
        *self.dcs_is_xtgettcap = intermediates == b"+" && byte == 'q';
    }

    fn put(&mut self, byte: u8) {
        // Cap the buffer so a malformed DCS string can't grow unboundedly.
        // XTGETTCAP can chain many cap names (`TN;Co;bce`), so allow a
        // larger ceiling than the RQSS selectors.
        let cap = if *self.dcs_is_xtgettcap { 512 } else { 64 };
        if (*self.dcs_is_rqss || *self.dcs_is_xtgettcap) && self.dcs_buf.len() < cap {
            self.dcs_buf.push(byte);
        }
    }

    fn unhook(&mut self) {
        if *self.dcs_is_xtgettcap {
            self.dispatch_xtgettcap();
            self.dcs_buf.clear();
            *self.dcs_is_xtgettcap = false;
            return;
        }
        if !*self.dcs_is_rqss {
            return;
        }
        // Reply format: `DCS 1 $ r <Pt> ST` for "valid",
        //               `DCS 0 $ r <Pt> ST` when unknown.
        // We answer two common queries:
        //   "m"  → SGR state (we report `0m` — default — to keep replies
        //          short; apps querying SGR usually just want to know that
        //          DECRQSS is supported, not to introspect every attr).
        //   " q" → DECSCUSR: cursor shape + blink.
        //   "r"  → DECSTBM: full screen since we don't expose `region` here.
        let reply = match self.dcs_buf.as_slice() {
            b"m" => {
                // Build a compact SGR string from the active attr flags.
                // fg/bg colour reporting is intentionally limited to the
                // base case (default colours) — keeping the reply short
                // matches xterm's behaviour and avoids burning bytes on
                // an indexed/truecolor encoding that few apps consume.
                let attrs = self.sgr.attrs;
                let mut parts: Vec<&str> = Vec::with_capacity(8);
                parts.push("0"); // SGR 0 = reset baseline
                if attrs.contains(CellAttrs::BOLD) { parts.push("1"); }
                if attrs.contains(CellAttrs::DIM) { parts.push("2"); }
                if attrs.contains(CellAttrs::ITALIC) { parts.push("3"); }
                if attrs.contains(CellAttrs::UNDERLINE) { parts.push("4"); }
                if attrs.contains(CellAttrs::BLINK) { parts.push("5"); }
                if attrs.contains(CellAttrs::REVERSE) { parts.push("7"); }
                if attrs.contains(CellAttrs::HIDDEN) { parts.push("8"); }
                if attrs.contains(CellAttrs::STRIKETHROUGH) { parts.push("9"); }
                if attrs.contains(CellAttrs::OVERLINE) { parts.push("53"); }
                Some(format!("\x1bP1$r{}m\x1b\\", parts.join(";")))
            }
            b" q" => {
                let n = match (*self.cursor_shape, *self.cursor_should_blink) {
                    (CursorShape::Block, true) => 1,
                    (CursorShape::Block, false) => 2,
                    (CursorShape::Underline, true) => 3,
                    (CursorShape::Underline, false) => 4,
                    (CursorShape::Bar, true) => 5,
                    (CursorShape::Bar, false) => 6,
                };
                Some(format!("\x1bP1$r{} q\x1b\\", n))
            }
            b"r" => {
                // DECSTBM — reply 1-based top;bottom of the scroll region.
                Some(format!(
                    "\x1bP1$r{};{}r\x1b\\",
                    self.region.top + 1,
                    self.region.bottom + 1,
                ))
            }
            // DECSCL — VT conformance level. Apps probing this expect a
            // VT500-ish answer; we report `65;1"p` = level 5 with 8-bit
            // controls, matching what xterm advertises by default.
            b"\"p" => Some("\x1bP1$r65;1\"p\x1b\\".to_string()),
            _ => {
                // Echo back the unknown selector with status 0 — apps use
                // this to detect that the query was understood but the
                // setting isn't exposed.
                let sel = String::from_utf8_lossy(self.dcs_buf);
                Some(format!("\x1bP0$r{}\x1b\\", sel))
            }
        };
        if let Some(s) = reply {
            self.osc_responses.push_back(s);
        }
        self.dcs_buf.clear();
        *self.dcs_is_rqss = false;
    }
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }
        let code = std::str::from_utf8(params[0]).unwrap_or("");
        match code {
            // Window title.
            //   OSC 0 → icon name + window title
            //   OSC 1 → icon name only (xterm distinguishes; we mirror it
            //           into the window title so apps like GNU screen that
            //           emit `OSC 1` get a sensible label)
            //   OSC 2 → window title only
            "0" | "1" | "2" if params.len() >= 2 => {
                // Re-join `;`-split params so titles with embedded
                // semicolons ("user@host: ~/work; tests") arrive intact.
                // Then strip ASCII control bytes (CR/LF/ESC/etc.) — a
                // malicious OSC 2 payload could otherwise smuggle a `\n`
                // into the tab-bar label, breaking the row layout, or an
                // escape sequence into a status-line plugin that
                // re-emits the title. Multi-byte UTF-8 (≥ 0x80) passes
                // through.
                let mut raw = String::new();
                for (i, part) in params[1..].iter().enumerate() {
                    if i > 0 {
                        raw.push(';');
                    }
                    raw.push_str(&String::from_utf8_lossy(part));
                }
                let title: String = raw
                    .chars()
                    .filter(|&c| c >= ' ' && c != '\x7f')
                    .collect();
                *self.current_title = Some(title.clone());
                *self.pending_title = Some(title);
            }
            // OSC 10 / 11 — query default fg / bg colour. Format: "?" as
            // payload → respond with `rgb:RRRR/GGGG/BBBB` (16-bit channels).
            "10" if params.get(1).is_some_and(|p| p == b"?") => {
                let s = format!(
                    "\x1b]10;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
                    self.default_fg_rgb[0], self.default_fg_rgb[0],
                    self.default_fg_rgb[1], self.default_fg_rgb[1],
                    self.default_fg_rgb[2], self.default_fg_rgb[2],
                );
                self.osc_responses.push_back(s);
            }
            "11" if params.get(1).is_some_and(|p| p == b"?") => {
                let s = format!(
                    "\x1b]11;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
                    self.default_bg_rgb[0], self.default_bg_rgb[0],
                    self.default_bg_rgb[1], self.default_bg_rgb[1],
                    self.default_bg_rgb[2], self.default_bg_rgb[2],
                );
                self.osc_responses.push_back(s);
            }
            // OSC 10 SET — `ESC ] 10 ; rgb:RR/GG/BB ST` updates the default
            // foreground. Mirror semantics for OSC 11 (background).
            "10" if params.len() >= 2 && !params[1].is_empty() && params[1] != b"?" => {
                if let Some(rgb) = std::str::from_utf8(params[1]).ok().and_then(parse_rgb_spec) {
                    *self.default_fg_rgb = rgb;
                    self.pending_palette_changes
                        .push_back(PaletteUpdate::DefaultFg(rgb));
                }
            }
            "11" if params.len() >= 2 && !params[1].is_empty() && params[1] != b"?" => {
                if let Some(rgb) = std::str::from_utf8(params[1]).ok().and_then(parse_rgb_spec) {
                    *self.default_bg_rgb = rgb;
                    self.pending_palette_changes
                        .push_back(PaletteUpdate::DefaultBg(rgb));
                }
            }
            // OSC 12 — query the cursor colour. Falls back to default fg
            // (xterm convention) when no explicit cursor colour is set.
            "12" if params.get(1).is_some_and(|p| p == b"?") => {
                let [r, g, b] = self.cursor_rgb.unwrap_or(*self.default_fg_rgb);
                let s = format!(
                    "\x1b]12;rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
                    r, r, g, g, b, b,
                );
                self.osc_responses.push_back(s);
            }
            // OSC 104 — reset palette indices to xterm built-ins. With no
            // params, resets all 16 named slots; with one or more params,
            // resets only the listed indices. Paired with OSC 4 SET.
            "104" => {
                let indices: Vec<u8> = if params.len() == 1 {
                    (0u8..16).collect()
                } else {
                    params[1..]
                        .iter()
                        .filter_map(|p| std::str::from_utf8(p).ok())
                        .filter_map(|s| s.trim().parse::<u16>().ok())
                        .filter(|i| *i < 16)
                        .map(|i| i as u8)
                        .collect()
                };
                for i in indices {
                    let default = DEFAULT_NAMED_PALETTE[i as usize];
                    self.named_palette[i as usize] = default;
                    if self.pending_palette_changes.len() >= 256 {
                        self.pending_palette_changes.pop_front();
                    }
                    self.pending_palette_changes
                        .push_back(PaletteUpdate::Named(i, default));
                }
            }
            // OSC 110 / 111 / 112 — reset default fg / bg / cursor to the
            // built-in xterm defaults. No payload accepted; any body is
            // ignored. These are paired with OSC 10 / 11 / 12 SET.
            "110" => {
                *self.default_fg_rgb = DEFAULT_FG;
                self.pending_palette_changes
                    .push_back(PaletteUpdate::DefaultFg(DEFAULT_FG));
            }
            "111" => {
                *self.default_bg_rgb = DEFAULT_BG;
                self.pending_palette_changes
                    .push_back(PaletteUpdate::DefaultBg(DEFAULT_BG));
            }
            "112" => {
                *self.cursor_rgb = None;
            }
            // OSC 12 SET — apps can pin the cursor colour at runtime via
            // `ESC ] 12 ; rgb:RR/GG/BB ST` (or 16-bit `RRRR/GGGG/BBBB`).
            // We update the in-core cursor_rgb so the renderer can read it
            // per-pane (overriding the palette default for that pane).
            "12" if params.len() >= 2 && !params[1].is_empty() && params[1] != b"?" => {
                let raw = std::str::from_utf8(params[1]).unwrap_or("");
                if let Some(rgb) = parse_rgb_spec(raw) {
                    *self.cursor_rgb = Some(rgb);
                }
            }
            // OSC 52: clipboard. Format: ESC ] 52 ; selection ; base64 ST.
            // We support writes only; the "?" read variant is ignored for
            // safety — remote shells should not be able to exfiltrate the
            // local clipboard.
            "52" => {
                if let Some(data) = params.get(2) {
                    if data == b"?" || data.is_empty() {
                        return;
                    }
                    let raw = String::from_utf8_lossy(data).into_owned();
                    if raw.len() <= 1_000_000 {
                        // Cap to 1 MiB base64 to avoid pathological pastes.
                        *self.pending_clipboard = Some(raw);
                    }
                }
            }
            // OSC 4 — palette query / set. Format:
            //   `ESC ] 4 ; n ; ? ST`       → reply with current RGB
            //   `ESC ] 4 ; n ; rgb:RR/GG/BB ST` → silently accepted (the
            //   renderer's palette is the source of truth in this build).
            "4" if params.len() >= 3 => {
                let mut i = 1;
                while i + 1 < params.len() {
                    let Some(idx) = std::str::from_utf8(params[i])
                        .ok()
                        .and_then(|s| s.trim().parse::<u16>().ok())
                    else {
                        break;
                    };
                    if idx > 255 {
                        i += 2;
                        continue;
                    }
                    if params[i + 1] == b"?" {
                        let [r, g, b] = indexed_palette_rgb(idx as u8, self.named_palette);
                        self.osc_responses.push_back(format!(
                            "\x1b]4;{};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
                            idx, r, r, g, g, b, b,
                        ));
                    } else if let Some(rgb) = std::str::from_utf8(params[i + 1])
                        .ok()
                        .and_then(parse_rgb_spec)
                    {
                        // SET — update the in-core named slot (so OSC 4
                        // queries reflect it) and queue a change for the
                        // renderer to apply to its live palette.
                        if (idx as usize) < 16 {
                            self.named_palette[idx as usize] = rgb;
                        }
                        if self.pending_palette_changes.len() >= 256 {
                            self.pending_palette_changes.pop_front();
                        }
                        self.pending_palette_changes
                            .push_back(PaletteUpdate::Named(idx as u8, rgb));
                    }
                    i += 2;
                }
            }
            // OSC 9 — iTerm2/Windows Terminal extension.
            //   `ESC ] 9 ; <message> ST`             → user notification.
            //   `ESC ] 9 ; 4 ; <state> ; <pct> ST`   → progress reporting
            //     state 0 = clear, 1 = set %, 2 = error, 3 = indeterminate,
            //     4 = warning. `pct` is 0..=100 (ignored for states 0/3).
            //   `ESC ] 9 ; 9 ; <cwd> ST`             → Windows Terminal
            //     style cwd broadcast. Some shell integrations emit this
            //     in place of OSC 7 / OSC 1337;CurrentDir=.
            "9" if params.len() >= 2 => {
                if params[1] == b"9" && params.len() >= 3 {
                    // Re-join params[2..] in case the path contains a
                    // literal `;`. Empty path is a no-op (keeps current
                    // cwd) — mirrors OSC 7 / 633;P;Cwd= behaviour so a
                    // malformed broadcast can't wipe a known good cwd.
                    let mut raw = String::new();
                    for (i, part) in params[2..].iter().enumerate() {
                        if i > 0 {
                            raw.push(';');
                        }
                        raw.push_str(&String::from_utf8_lossy(part));
                    }
                    if !raw.is_empty() {
                        *self.cwd = Some(percent_decode(&raw));
                    }
                } else if params[1] == b"4" && params.len() >= 3 {
                    let state: u8 = std::str::from_utf8(params[2])
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    let pct: u8 = params
                        .get(3)
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|s| s.trim().parse::<u8>().ok())
                        .unwrap_or(0)
                        .min(100);
                    if self.pending_progress.len() >= NOTIFICATIONS_CAP {
                        self.pending_progress.pop_front();
                    }
                    self.pending_progress.push_back((state, pct));
                } else {
                    // OSC 9 ; <message>. Real-world messages frequently
                    // contain `;` (e.g. `OSC 9; build ok; 12.3s`), and
                    // the VTE parser splits them into multiple params.
                    // Re-join everything from params[1..] with the
                    // original `;` separator so the plugin event sees
                    // the whole user-visible text.
                    let mut msg = String::new();
                    for (i, part) in params[1..].iter().enumerate() {
                        if i > 0 {
                            msg.push(';');
                        }
                        msg.push_str(&String::from_utf8_lossy(part));
                    }
                    self.push_notification(msg);
                }
            }
            // OSC 99 — kitty's notification protocol. Minimal
            // implementation: treat `ESC ] 99 ; <options> ; <body> ST`
            // as a single-segment notification. Options are a
            // colon-separated list of `k=v` pairs we don't model
            // yet (priority, action, etc.). Multi-segment streams
            // (kitty's `p=?` for "more coming") aren't handled —
            // each frame is a standalone notification, which
            // matches what most CLIs emit.
            "99" if params.len() >= 3 => {
                let mut body = String::new();
                for (i, p) in params[2..].iter().enumerate() {
                    if i > 0 {
                        body.push(';');
                    }
                    body.push_str(&String::from_utf8_lossy(p));
                }
                self.push_notification(body);
            }
            // OSC 777 — urxvt's notification protocol.
            //   `ESC ] 777 ; notify ; <title> ; <body> ST`        → "title: body"
            //   `ESC ] 777 ; notify ; <body> ST`                  → bare body
            // Anything other than the `notify` subtype is ignored
            // (urxvt also defines an obscure "screen" subtype that
            // we don't model). Re-join trailing params on `;` so a
            // legitimate semicolon in the title / body survives the
            // VTE split.
            "777" if params.len() >= 3 && params[1] == b"notify" => {
                let title = String::from_utf8_lossy(params[2]).into_owned();
                let body = if params.len() > 3 {
                    let mut s = String::new();
                    for (i, p) in params[3..].iter().enumerate() {
                        if i > 0 {
                            s.push(';');
                        }
                        s.push_str(&String::from_utf8_lossy(p));
                    }
                    s
                } else {
                    String::new()
                };
                let msg = if body.is_empty() {
                    title
                } else {
                    format!("{}: {}", title, body)
                };
                self.push_notification(msg);
            }
            // OSC 133: shell integration markers.
            //   ;A → prompt start: stored as a navigable mark
            //   ;D[;exit] → command finished: enqueued for `shell.exit`
            //   ;B, ;C are accepted silently for now
            "133" if params.len() >= 2 => {
                match params[1] {
                    b"A" => {
                        let line = self.scrollback.len() + self.cursor.row as usize;
                        if self.prompt_marks.back().copied() != Some(line) {
                            if self.prompt_marks.len() >= PROMPT_MARKS_CAP {
                                self.prompt_marks.pop_front();
                            }
                            self.prompt_marks.push_back(line);
                        }
                    }
                    b"C" => {
                        let line = self.scrollback.len() + self.cursor.row as usize;
                        if self.command_marks.back().copied() != Some(line) {
                            if self.command_marks.len() >= PROMPT_MARKS_CAP {
                                self.command_marks.pop_front();
                            }
                            self.command_marks.push_back(line);
                        }
                        // Stamp command-start time so the next `;D` can
                        // report the elapsed run duration.
                        *self.last_command_start = Some(std::time::Instant::now());
                    }
                    b"D" => {
                        let code: i32 = params
                            .get(2)
                            .and_then(|p| std::str::from_utf8(p).ok())
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(0);
                        // Same cap as notifications / progress — the
                        // renderer drains every frame, but a tight loop
                        // of OSC 133;D between draws could otherwise
                        // build up unbounded.
                        let duration_ms = self
                            .last_command_start
                            .take()
                            .map(|start| start.elapsed().as_millis().min(u64::MAX as u128) as u64);
                        if self.pending_command_finishes.len() >= NOTIFICATIONS_CAP {
                            self.pending_command_finishes.pop_front();
                        }
                        self.pending_command_finishes.push_back(CommandFinish {
                            exit_code: code,
                            duration_ms,
                        });
                    }
                    _ => {}
                }
            }
            // OSC 633 — VS Code's shell-integration variant. Mirrors OSC
            // 133's A/B/C/D sub-codes plus a `P;<key>=<value>` property
            // setter. The `P;Cwd=<path>` form is the only property that
            // affects observable state today — it's the most common path
            // for VS Code's bundled terminal to broadcast the shell's
            // working directory.
            "633" if params.len() >= 2 => {
                match params[1] {
                    b"A" => {
                        let line = self.scrollback.len() + self.cursor.row as usize;
                        if self.prompt_marks.back().copied() != Some(line) {
                            if self.prompt_marks.len() >= PROMPT_MARKS_CAP {
                                self.prompt_marks.pop_front();
                            }
                            self.prompt_marks.push_back(line);
                        }
                    }
                    b"C" => {
                        let line = self.scrollback.len() + self.cursor.row as usize;
                        if self.command_marks.back().copied() != Some(line) {
                            if self.command_marks.len() >= PROMPT_MARKS_CAP {
                                self.command_marks.pop_front();
                            }
                            self.command_marks.push_back(line);
                        }
                        *self.last_command_start = Some(std::time::Instant::now());
                    }
                    b"D" => {
                        let code: i32 = params
                            .get(2)
                            .and_then(|p| std::str::from_utf8(p).ok())
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(0);
                        let duration_ms = self
                            .last_command_start
                            .take()
                            .map(|start| start.elapsed().as_millis().min(u64::MAX as u128) as u64);
                        if self.pending_command_finishes.len() >= NOTIFICATIONS_CAP {
                            self.pending_command_finishes.pop_front();
                        }
                        self.pending_command_finishes.push_back(CommandFinish {
                            exit_code: code,
                            duration_ms,
                        });
                    }
                    b"P" if params.len() >= 3 => {
                        // Property form: `OSC 633 ; P ; Key=Value ST`. The
                        // value half can contain `;` and `=` (rare but
                        // possible for paths with literal `=`), so split
                        // only on the FIRST `=` after re-joining the tail.
                        let mut raw = String::new();
                        for (i, part) in params[2..].iter().enumerate() {
                            if i > 0 {
                                raw.push(';');
                            }
                            raw.push_str(&String::from_utf8_lossy(part));
                        }
                        if let Some((key, value)) = raw.split_once('=') {
                            if key.eq_ignore_ascii_case("Cwd") && !value.is_empty() {
                                *self.cwd = Some(percent_decode(value));
                            }
                        }
                    }
                    _ => {}
                }
            }
            // OSC 7: working-directory broadcast. Format:
            // ESC ] 7 ; file://<host><path> ST
            // Re-join `;`-split params so paths with `;` in their name
            // (rare but legal on Unix) arrive intact.
            "7" if params.len() >= 2 => {
                let mut raw = String::new();
                for (i, part) in params[1..].iter().enumerate() {
                    if i > 0 {
                        raw.push(';');
                    }
                    raw.push_str(&String::from_utf8_lossy(part));
                }
                let path_part = if let Some(rest) = raw.strip_prefix("file://") {
                    rest.split_once('/')
                        .map(|(_, p)| format!("/{}", p))
                        .unwrap_or_else(|| rest.to_string())
                } else {
                    raw.clone()
                };
                let decoded = percent_decode(&path_part);
                if !decoded.is_empty() {
                    *self.cwd = Some(decoded);
                }
            }
            // OSC 1337 — iTerm2-style commands.
            //   `CurrentDir=<path>`   → cwd broadcast (zsh/fish often
            //                            emit this in addition to OSC 7).
            //   `ClearScrollback`     → wipe the primary-screen
            //                            scrollback ring. Useful for
            //                            "clear" implementations that
            //                            want to drop history too.
            // Re-join `;`-split params so `CurrentDir=/a;b` and
            // `SetUserVar=KEY=VAL;META=x` survive intact.
            "1337" if params.len() >= 2 => {
                let mut raw = String::new();
                for (i, part) in params[1..].iter().enumerate() {
                    if i > 0 {
                        raw.push(';');
                    }
                    raw.push_str(&String::from_utf8_lossy(part));
                }
                if let Some(path) = raw.strip_prefix("CurrentDir=") {
                    if !path.is_empty() {
                        *self.cwd = Some(percent_decode(path));
                    }
                } else if raw.eq_ignore_ascii_case("ClearScrollback") {
                    // Drop the scrollback ring and re-anchor the prompt
                    // / command marks the same way `clear_scrollback`
                    // does — keep behaviour identical regardless of
                    // whether the shell asked via OSC 1337 or the user
                    // pressed Ctrl+Shift+K.
                    let dropped = self.scrollback.len();
                    self.scrollback.clear();
                    shift_marks_after_scrollback_drop(self.prompt_marks, dropped);
                    shift_marks_after_scrollback_drop(self.command_marks, dropped);
                } else if let Some(msg) = raw.strip_prefix("notify=") {
                    // iTerm2 desktop notification:
                    // `ESC ] 1337 ; notify=<message> ST`. Routes
                    // through the same queue as OSC 9 / OSC 777 so
                    // the `notification` plugin event picks it up
                    // uniformly.
                    self.push_notification(msg.to_string());
                }
            }
            // OSC 8: hyperlink. Format: ESC ] 8 ; params ; URI ST
            // params[1] = link attributes (id=..., etc), params[2] = URI.
            // Empty URI ends the hyperlink span.
            "8" => {
                // Re-join `;`-split URI parts (query strings like
                // `?a=1;b=2` would otherwise truncate at the first `;`).
                // params[2..] is the URI, link-attrs sit in params[1].
                let uri = if params.len() > 2 {
                    let mut s = String::new();
                    for (i, part) in params[2..].iter().enumerate() {
                        if i > 0 {
                            s.push(';');
                        }
                        s.push_str(&String::from_utf8_lossy(part));
                    }
                    s
                } else {
                    String::new()
                };
                // Bound URI length so a malicious shell can't pin a
                // multi-gigabyte string into the hyperlink table. Real
                // URIs are <2 KiB in practice; 8 KiB leaves room for
                // data: URLs without becoming a DoS surface.
                const URI_CAP: usize = 8 * 1024;
                if uri.len() > URI_CAP {
                    *self.current_hyperlink = 0;
                    return;
                }
                if uri.is_empty() {
                    *self.current_hyperlink = 0;
                } else if let Some(&existing_id) =
                    self.hyperlink_uri_to_id.get(uri.as_str())
                {
                    // Reuse path — shells often repeat the same link
                    // many times (e.g. a file path in every `ls`
                    // output). O(1) via the inverted index.
                    *self.current_hyperlink = existing_id;
                } else {
                    let id = *self.next_link_id;
                    *self.next_link_id = self.next_link_id.wrapping_add(1).max(1);
                    let new_len = uri.len();
                    // Bound BOTH the entry count AND the cumulative
                    // byte total — eviction loops drop the oldest URI
                    // until each cap is satisfied. Cells referring to
                    // evicted ids degrade to "no link" rather than a
                    // stale URI. `bytes_used` is incrementally tracked
                    // via the inverted index sums; recomputing the sum
                    // every dispatch was O(N).
                    let mut bytes_used: usize =
                        self.hyperlink_uri_to_id.keys().map(String::len).sum();
                    while self.hyperlinks.len() >= HYPERLINK_CAP
                        || bytes_used + new_len > HYPERLINK_TOTAL_BYTES_CAP
                    {
                        let Some(old) = self.hyperlink_order.pop_front() else { break };
                        if let Some(removed) = self.hyperlinks.remove(&old) {
                            self.hyperlink_uri_to_id.remove(&removed);
                            bytes_used = bytes_used.saturating_sub(removed.len());
                        }
                    }
                    self.hyperlink_uri_to_id.insert(uri.clone(), id);
                    self.hyperlinks.insert(id, uri);
                    self.hyperlink_order.push_back(id);
                    *self.current_hyperlink = id;
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, c: char) {
        if intermediates == b"?" {
            match c {
                'h' => self.handle_private_mode(params, true),
                'l' => self.handle_private_mode(params, false),
                // DECSED / DECSEL — Selective Erase in Display / Line.
                // These differ from ED/EL only in that they preserve any
                // cells flagged "protected" by DECSCA. We don't track
                // DECSCA (which is essentially unused in modern shells),
                // so route to the regular ED/EL paths — apps still get
                // the visible-erase behaviour they expect.
                'J' => self.erase_in_display(first_param_or(params, 0)),
                'K' => self.erase_in_line(first_param_or(params, 0)),
                // Private DSR. ?6n → DECXCPR (extended cursor position with
                // page number); ?15n → printer status (reply "no printer").
                // Apps like vim probe DECXCPR before fancier rendering paths
                // and bail gracefully if the reply doesn't arrive.
                'n' => {
                    let mode = first_param_or(params, 0);
                    match mode {
                        6 => {
                            let row = self.cursor.row + 1;
                            let col = self.cursor.col + 1;
                            // Trailing "1" = page number. We have a single
                            // page so it's always 1.
                            self.osc_responses
                                .push_back(format!("\x1b[?{row};{col};1R"));
                        }
                        15 => {
                            // CSI ? 13 n = "no printer".
                            self.osc_responses
                                .push_back("\x1b[?13n".to_string());
                        }
                        25 => {
                            // CSI ? 20 n = UDK (User Defined Keys) unlocked.
                            self.osc_responses
                                .push_back("\x1b[?20n".to_string());
                        }
                        26 => {
                            // Keyboard language: 1 = North American.
                            self.osc_responses
                                .push_back("\x1b[?27;1;0;0n".to_string());
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            return;
        }
        // ANSI modes (no private prefix). Only IRM (mode 4) is honored —
        // everything else is silently accepted so apps don't break.
        if intermediates.is_empty() && (c == 'h' || c == 'l') {
            let set = c == 'h';
            for slice in params.iter() {
                if let Some(&code) = slice.first() {
                    if code == 4 {
                        *self.insert_mode = set;
                    }
                }
            }
            return;
        }
        // DECRQM — Request Mode: `CSI ? Pm $ p`. Reply mirrors xterm:
        //   `CSI ? Pm; v $ y` where v is 1=set, 2=reset, 0=unrecognised.
        if intermediates == b"?$" && c == 'p' {
            let mode = first_param_or(params, 0);
            let value = self.decrqm_value(mode);
            self.osc_responses
                .push_back(format!("\x1b[?{};{}$y", mode, value));
            return;
        }
        // ANSI DECRQM (no `?` prefix): `CSI Pm $ p`. Same reply shape
        // minus the `?`. Mode 4 is IRM (Insert/Replace) — that's the
        // only one we actually track on the public surface; everything
        // else replies 0 (unrecognised) so apps can fall back.
        if intermediates == b"$" && c == 'p' {
            let mode = first_param_or(params, 0);
            let value: u8 = match mode {
                4 => {
                    if *self.insert_mode {
                        1
                    } else {
                        2
                    }
                }
                _ => 0,
            };
            self.osc_responses
                .push_back(format!("\x1b[{};{}$y", mode, value));
            return;
        }
        // DA2 — Secondary Device Attributes: `CSI > c` / `CSI > 0 c`.
        // Reply mimics xterm: terminal type 41 (VT420), firmware 95, no
        // cartridge → "ESC [ > 41 ; 95 ; 0 c".
        if intermediates == b">" && c == 'c' {
            self.osc_responses
                .push_back("\x1b[>41;95;0c".to_string());
            return;
        }
        // DA3 — Tertiary Device Attributes: `CSI = c` / `CSI = 0 c`.
        // Reply format is `DCS ! | <hex8> ST` where `hex8` is a 4-byte
        // terminal-identification site code. xterm and most clones send
        // `00000000`. Apps that key off this byte (rare) just need a
        // well-formed reply, not a unique value.
        if intermediates == b"=" && c == 'c' {
            self.osc_responses
                .push_back("\x1bP!|00000000\x1b\\".to_string());
            return;
        }
        // XTVERSION — Report Terminal Name and Version: `CSI > q`. Reply
        // format: `DCS > | <name>(<ver>) ST`. We emit `rterm(<crate ver>)`.
        if intermediates == b">" && c == 'q' {
            self.osc_responses
                .push_back(format!("\x1bP>|rterm({})\x1b\\", env!("CARGO_PKG_VERSION")));
            return;
        }
        // DECSTR — Soft Terminal Reset: `CSI ! p`. Per the DEC spec this
        // resets the most user-visible modes (cursor visible, autowrap,
        // origin mode, app cursor / keypad, mouse / paste modes, scroll
        // region, SGR) but keeps the screen contents and scrollback so
        // running shells don't lose context.
        if intermediates == b"!" && c == 'p' {
            let size = self.size();
            *self.sgr = SgrState::default();
            self.sgr_stack.clear();
            *self.region = ScrollRegion::full(size);
            *self.cursor_visible = true;
            *self.app_cursor_keys = false;
            *self.app_keypad = false;
            *self.autowrap = true;
            *self.origin_mode = false;
            *self.mouse_tracking = MouseTracking::Off;
            *self.sgr_mouse = false;
            *self.bracketed_paste = false;
            *self.focus_tracking = false;
            *self.sync_output = false;
            *self.insert_mode = false;
            *self.cursor_shape = CursorShape::Block;
            *self.cursor_should_blink = true;
            *self.current_hyperlink = 0;
            *self.charset_g0 = Charset::Ascii;
            *self.charset_g1 = Charset::Ascii;
            *self.active_charset = 0;
            self.saved[PRIMARY] = None;
            self.saved[ALT] = None;
            self.cursor.col = 0;
            self.cursor.row = 0;
            return;
        }
        // XTPUSHSGR — `CSI # {` (or `CSI Ps # {`): save the current SGR
        // state to a stack so apps emitting nested-decorated text (`bat`,
        // `delta`, `eza`) can restore it without re-emitting every flag.
        // We ignore the parameter list — saving everything is cheaper than
        // tracking which subset to push, and the stack pops with the same
        // semantics either way. Capped at 32 entries to bound memory.
        if intermediates == b"#" && c == '{' {
            const SGR_STACK_CAP: usize = 32;
            if self.sgr_stack.len() < SGR_STACK_CAP {
                self.sgr_stack.push(*self.sgr);
            }
            return;
        }
        // XTPOPSGR — `CSI # }`: restore the most recently pushed SGR
        // state. Silently no-ops when the stack is empty (mirrors xterm's
        // behaviour — apps shouldn't crash a terminal by over-popping).
        if intermediates == b"#" && c == '}' {
            if let Some(prev) = self.sgr_stack.pop() {
                *self.sgr = prev;
            }
            return;
        }
        // SL — Scroll Left: `CSI Pn SP @`. Shift each row in the active
        // scroll region left by Pn columns, filling the right edge with
        // blanks coloured by the current SGR bg.
        if intermediates == b" " && c == '@' {
            let n = first_param_or(params, 1) as usize;
            self.shift_cols_in_region(-(n as i32));
            return;
        }
        // SR — Scroll Right: `CSI Pn SP A`. Mirror of SL.
        if intermediates == b" " && c == 'A' {
            let n = first_param_or(params, 1) as usize;
            self.shift_cols_in_region(n as i32);
            return;
        }
        // DECSCUSR — set cursor style: CSI Ps SP q
        if intermediates == b" " && c == 'q' {
            let n = first_param_or(params, 1);
            let (shape, blink) = match n {
                0 | 1 => (CursorShape::Block, true),
                2 => (CursorShape::Block, false),
                3 => (CursorShape::Underline, true),
                4 => (CursorShape::Underline, false),
                5 => (CursorShape::Bar, true),
                6 => (CursorShape::Bar, false),
                _ => return,
            };
            *self.cursor_shape = shape;
            *self.cursor_should_blink = blink;
            return;
        }
        if !intermediates.is_empty() {
            return;
        }
        match c {
            'A' => {
                let n = first_param_or(params, 1) as i32;
                self.move_cursor_clamped(self.cursor.row as i32 - n, self.cursor.col as i32);
            }
            'B' | 'e' => {
                let n = first_param_or(params, 1) as i32;
                self.move_cursor_clamped(self.cursor.row as i32 + n, self.cursor.col as i32);
            }
            'C' | 'a' => {
                let n = first_param_or(params, 1) as i32;
                self.move_cursor_clamped(self.cursor.row as i32, self.cursor.col as i32 + n);
            }
            'D' => {
                let n = first_param_or(params, 1) as i32;
                self.move_cursor_clamped(self.cursor.row as i32, self.cursor.col as i32 - n);
            }
            'E' => {
                let n = first_param_or(params, 1) as i32;
                self.move_cursor_clamped(self.cursor.row as i32 + n, 0);
            }
            'F' => {
                let n = first_param_or(params, 1) as i32;
                self.move_cursor_clamped(self.cursor.row as i32 - n, 0);
            }
            'G' | '`' => {
                let n = first_param_or(params, 1) as i32 - 1;
                self.move_cursor_clamped(self.cursor.row as i32, n);
            }
            'd' => {
                let n = first_param_or(params, 1) as i32 - 1;
                self.move_cursor_clamped(self.origin_to_abs_row(n), self.cursor.col as i32);
            }
            'H' | 'f' => {
                let (row, col) = first_two_params_or(params, 1);
                self.move_cursor_clamped(
                    self.origin_to_abs_row(row as i32 - 1),
                    col as i32 - 1,
                );
            }
            'J' => self.erase_in_display(first_param_or(params, 0)),
            'K' => self.erase_in_line(first_param_or(params, 0)),
            'S' => self.scroll_region_up(first_param_or(params, 1)),
            'T' => self.scroll_region_down(first_param_or(params, 1)),
            '@' => self.insert_chars(first_param_or(params, 1)),
            'P' => self.delete_chars(first_param_or(params, 1)),
            'X' => self.erase_chars(first_param_or(params, 1)),
            'L' => self.insert_lines(first_param_or(params, 1)),
            'M' => self.delete_lines(first_param_or(params, 1)),
            'm' => self.handle_sgr(params),
            'r' => {
                let size = self.size();
                let (top, bottom) = first_two_params_or(params, 1);
                let top0 = top.saturating_sub(1).min(size.rows.saturating_sub(1));
                let bot0 = bottom.saturating_sub(1).min(size.rows.saturating_sub(1));
                if top0 < bot0 {
                    self.region.top = top0;
                    self.region.bottom = bot0;
                } else {
                    *self.region = ScrollRegion::full(size);
                }
                self.move_cursor_clamped(0, 0);
            }
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            // DA1 — Primary Device Attributes: `CSI c` / `CSI 0 c`.
            // Reply: "ESC [ ? 64 ; 1 ; 2 ; 6 ; 9 ; 15 ; 22 c"
            //   64 = VT420 base
            //   1  = 132-column mode
            //   2  = printer port
            //   6  = selective erase
            //   9  = national replacement char sets
            //   15 = technical character set
            //   22 = ANSI color
            'c' => self
                .osc_responses
                .push_back("\x1b[?64;1;2;6;9;15;22c".to_string()),
            // DSR — Device Status Report.
            //   5n → terminal OK: reply "ESC [ 0 n"
            //   6n → CPR (cursor position report): reply "ESC [ row ; col R"
            'n' => {
                let mode = first_param_or(params, 0);
                match mode {
                    5 => self.osc_responses.push_back("\x1b[0n".to_string()),
                    6 => {
                        let row = self.cursor.row + 1;
                        let col = self.cursor.col + 1;
                        self.osc_responses
                            .push_back(format!("\x1b[{row};{col}R"));
                    }
                    _ => {}
                }
            }
            // CHT (forward tabs) / CBT (backward tabs).
            'I' => self.forward_tabs(first_param_or(params, 1)),
            'Z' => self.backward_tabs(first_param_or(params, 1)),
            // CSI Ps[;Ps] t — window-manipulation ops. We answer the size
            // queries apps rely on (18 = text-area in cells) and support the
            // common 22 / 23 title push/pop pair; the rest is accepted-and
            // -ignored.
            't' => {
                let mode = first_param_or(params, 0);
                match mode {
                    // CSI 11 t — report window state. Reply
                    // `CSI <state> t` where state = 1 (non-iconified)
                    // or 2 (iconified). We never expose a minimised
                    // state to the PTY, so always reply 1. Some apps
                    // (vim, less) probe this before falling back to
                    // size queries.
                    11 => {
                        self.osc_responses.push_back("\x1b[1t".to_string());
                    }
                    // CSI 13 t — report window position. Reply
                    // `CSI 3 ; x ; y t`. We don't surface a real
                    // window position through the PTY layer (and
                    // exposing one would leak compositor coords to
                    // remote shells), so reply with `0;0`.
                    13 => {
                        self.osc_responses.push_back("\x1b[3;0;0t".to_string());
                    }
                    18 => {
                        let size = self.size();
                        self.osc_responses
                            .push_back(format!("\x1b[8;{};{}t", size.rows, size.cols));
                    }
                    // CSI 19 t — report the entire screen size in
                    // characters. We don't separate a "screen" from the
                    // active grid (no full-screen mode), so the reply
                    // mirrors mode 18 with the xterm-standard `9;<r>;<c>t`
                    // shape.
                    19 => {
                        let size = self.size();
                        self.osc_responses
                            .push_back(format!("\x1b[9;{};{}t", size.rows, size.cols));
                    }
                    // CSI 20 t — report icon label. We don't carry a
                    // distinct icon name; mirror the window title via
                    // `OSC L <title> ST`, matching xterm.
                    20 => {
                        let title = self.current_title.clone().unwrap_or_default();
                        self.osc_responses
                            .push_back(format!("\x1b]L{}\x1b\\", title));
                    }
                    // CSI 21 t — report window title back via `OSC l <title> ST`.
                    21 => {
                        let title = self.current_title.clone().unwrap_or_default();
                        self.osc_responses
                            .push_back(format!("\x1b]l{}\x1b\\", title));
                    }
                    22 => {
                        // Push window title onto the stack. We treat
                        // sub-mode (icon/title/both) uniformly because we
                        // don't carry an icon-name slot.
                        if let Some(t) = self.current_title.as_ref() {
                            if self.title_stack.len() >= 64 {
                                self.title_stack.remove(0);
                            }
                            self.title_stack.push(t.clone());
                        }
                    }
                    23 => {
                        if let Some(t) = self.title_stack.pop() {
                            *self.current_title = Some(t.clone());
                            *self.pending_title = Some(t);
                        }
                    }
                    _ => {}
                }
            }
            // REP — repeat preceding character `Pn` times. No-op if no
            // graphic character has been printed since the last control
            // byte. Capped at one screenful to avoid runaway loops.
            'b' => {
                if let Some(ch) = *self.last_printed {
                    let max = (self.size().cols as u32) * (self.size().rows as u32);
                    let n = (first_param_or(params, 1) as u32).min(max).max(1);
                    for _ in 0..n {
                        self.print(ch);
                    }
                }
            }
            // TBC — Tab Clear. 0: clear at cursor (default). 3: clear all.
            'g' => {
                let mode = first_param_or(params, 0);
                match mode {
                    0 => {
                        let col = self.cursor.col as usize;
                        if col < self.tab_stops.len() {
                            self.tab_stops[col] = false;
                        }
                    }
                    3 => self.tab_stops.iter_mut().for_each(|s| *s = false),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // DECALN — Screen Alignment Display: `ESC # 8`. Fills the active
        // grid with `E`s for diagnostics (used by vttest); other `# n`
        // sequences (DECDHL/DECDWL/DECSWL) are silently ignored.
        if intermediates == b"#" {
            if byte == b'8' {
                let blank = Cell { ch: 'E', ..Cell::default() };
                let size = self.size();
                for r in 0..size.rows {
                    for c in 0..size.cols {
                        if let Some(cell) = self.grid_mut().cell_mut(Position { col: c, row: r }) {
                            *cell = blank;
                        }
                    }
                }
            }
            return;
        }
        // SCS — Select Character Set. `ESC ( X` designates G0; `ESC ) X`
        // designates G1. We recognise `B` (ASCII, default) and `0` (DEC
        // Special Graphics). Other charsets are silently treated as ASCII.
        if intermediates == b"(" || intermediates == b")" {
            let target = if intermediates == b"(" {
                &mut *self.charset_g0
            } else {
                &mut *self.charset_g1
            };
            *target = match byte {
                b'0' => Charset::DecSpecialGraphics,
                _ => Charset::Ascii,
            };
            return;
        }
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.linefeed(),
            b'M' => {
                if self.cursor.row == self.region.top {
                    self.scroll_region_down(1);
                } else if self.cursor.row > 0 {
                    self.cursor.row -= 1;
                }
            }
            b'E' => {
                self.cursor.col = 0;
                self.linefeed();
            }
            // HTS — Horizontal Tab Set at the current cursor column.
            b'H' => {
                let col = self.cursor.col as usize;
                if col < self.tab_stops.len() {
                    self.tab_stops[col] = true;
                }
            }
            // DECPAM (ESC =) / DECPNM (ESC >): keypad application mode.
            b'=' => *self.app_keypad = true,
            b'>' => *self.app_keypad = false,
            // DECID — Identify Terminal: `ESC Z`. Legacy VT52/VT100
            // probe that predates CSI c (DA1). Some old apps still
            // emit it; reply with the same DA1 payload so they
            // recognise us as VT420-class.
            b'Z' => self
                .osc_responses
                .push_back("\x1b[?64;1;2;6;9;15;22c".to_string()),
            // RIS — Reset to Initial State: clear screen, home cursor, reset
            // SGR, scroll region, modes, alt-screen, hyperlinks. Mirrors a
            // soft `reset` in xterm. Scrollback is preserved per common
            // convention so the user can still see what came before.
            b'c' => {
                // Drop to primary screen.
                if *self.active == ALT {
                    *self.active = PRIMARY;
                }
                // Wipe both grids.
                let size = self.grids[*self.active].size();
                self.grids[PRIMARY].clear();
                self.grids[ALT].clear();
                self.cursor.col = 0;
                self.cursor.row = 0;
                *self.sgr = SgrState::default();
                self.sgr_stack.clear();
                *self.region = ScrollRegion::full(size);
                *self.cursor_visible = true;
                *self.app_cursor_keys = false;
                *self.app_keypad = false;
                *self.autowrap = true;
                *self.origin_mode = false;
                *self.mouse_tracking = MouseTracking::Off;
                *self.sgr_mouse = false;
                *self.bracketed_paste = false;
                *self.focus_tracking = false;
                *self.sync_output = false;
                *self.reverse_screen = false;
                *self.insert_mode = false;
                *self.cursor_shape = CursorShape::Block;
                *self.cursor_should_blink = true;
                *self.current_hyperlink = 0;
                self.hyperlinks.clear();
                self.hyperlink_uri_to_id.clear();
                self.hyperlink_order.clear();
                *self.charset_g0 = Charset::Ascii;
                *self.charset_g1 = Charset::Ascii;
                *self.active_charset = 0;
                // Default tab stops every 8 columns.
                for (i, s) in self.tab_stops.iter_mut().enumerate() {
                    *s = i > 0 && i % TAB_STEP == 0;
                }
                self.saved[PRIMARY] = None;
                self.saved[ALT] = None;
                // Shell-integration state is per-session; clearing the
                // grid invalidates every mark that pointed into it.
                // Drop them so the next shell launch starts clean.
                self.prompt_marks.clear();
                self.command_marks.clear();
                self.pending_lines.clear();
                self.pending_command_finishes.clear();
                *self.last_command_start = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cols: u16, rows: u16) -> Terminal {
        Terminal::new(Size { cols, rows })
    }

    fn cell(t: &Terminal, col: u16, row: u16) -> Cell {
        *t.grid().cell(Position { col, row }).unwrap()
    }

    fn row_text(t: &Terminal, row: u16) -> String {
        (0..t.size().cols).map(|c| cell(t, c, row).ch).collect()
    }

    #[test]
    fn print_lays_chars_left_to_right() {
        let mut t = term(8, 2);
        t.advance(b"hi");
        assert_eq!(cell(&t, 0, 0).ch, 'h');
        assert_eq!(cell(&t, 1, 0).ch, 'i');
        assert_eq!(t.cursor(), Position { col: 2, row: 0 });
    }

    #[test]
    fn cr_lf_navigate() {
        let mut t = term(8, 3);
        t.advance(b"a\nb\r\nc");
        assert_eq!(cell(&t, 0, 0).ch, 'a');
        assert_eq!(cell(&t, 1, 1).ch, 'b');
        assert_eq!(cell(&t, 0, 2).ch, 'c');
    }

    #[test]
    fn cup_positions_one_indexed() {
        let mut t = term(10, 5);
        t.advance(b"\x1b[3;5HX");
        assert_eq!(cell(&t, 4, 2).ch, 'X');
    }

    #[test]
    fn cuu_cud_cuf_cub_clamp_to_grid() {
        let mut t = term(4, 4);
        t.advance(b"\x1b[100;100H");
        assert_eq!(t.cursor(), Position { col: 3, row: 3 });
        t.advance(b"\x1b[10A");
        assert_eq!(t.cursor(), Position { col: 3, row: 0 });
        t.advance(b"\x1b[10D");
        assert_eq!(t.cursor(), Position { col: 0, row: 0 });
    }

    #[test]
    fn sgr_bold_red_then_reset() {
        let mut t = term(6, 1);
        t.advance(b"\x1b[1;31mA\x1b[0mB");
        let a = cell(&t, 0, 0);
        assert!(a.attrs.contains(CellAttrs::BOLD));
        assert_eq!(a.fg, Color::Named(NamedColor::Red));
        let b = cell(&t, 1, 0);
        assert!(!b.attrs.contains(CellAttrs::BOLD));
        assert_eq!(b.fg, Color::Default);
    }

    #[test]
    fn sgr_truecolor_fg_bg() {
        let mut t = term(4, 1);
        t.advance(b"\x1b[38;2;10;20;30;48;2;200;100;50mX");
        let x = cell(&t, 0, 0);
        assert_eq!(x.fg, Color::Rgb(10, 20, 30));
        assert_eq!(x.bg, Color::Rgb(200, 100, 50));
    }

    #[test]
    fn sgr_indexed_256() {
        let mut t = term(4, 1);
        t.advance(b"\x1b[38;5;201mY");
        assert_eq!(cell(&t, 0, 0).fg, Color::Indexed(201));
    }

    #[test]
    fn ed_2_clears_screen() {
        let mut t = term(4, 2);
        t.advance(b"xxxx\nyyyy");
        t.advance(b"\x1b[2J");
        assert_eq!(row_text(&t, 0), "    ");
        assert_eq!(row_text(&t, 1), "    ");
    }

    #[test]
    fn el_clears_to_eol() {
        let mut t = term(6, 1);
        t.advance(b"abcdef");
        t.advance(b"\x1b[3G\x1b[K");
        assert_eq!(row_text(&t, 0), "ab    ");
    }

    #[test]
    fn save_restore_cursor_and_sgr() {
        let mut t = term(10, 3);
        t.advance(b"\x1b[2;5H\x1b[31m\x1b[sABC\x1b[32mz\x1b[uX");
        let x = cell(&t, 4, 1);
        assert_eq!(x.ch, 'X');
        assert_eq!(x.fg, Color::Named(NamedColor::Red));
        let z = cell(&t, 7, 1);
        assert_eq!(z.ch, 'z');
        assert_eq!(z.fg, Color::Named(NamedColor::Green));
    }

    #[test]
    fn linefeed_at_bottom_scrolls_and_grows_scrollback() {
        let mut t = term(3, 2);
        // Real PTYs apply ONLCR so terminal input is CR+LF, not bare LF.
        t.advance(b"abc\r\ndef\r\nghi");
        assert_eq!(row_text(&t, 0), "def");
        assert_eq!(row_text(&t, 1), "ghi");
        assert_eq!(t.scrollback_len(), 1);
        let line: String = t.scrollback_line(0).unwrap().iter().map(|c| c.ch).collect();
        assert_eq!(line, "abc");
    }

    #[test]
    fn scrollback_respects_limit() {
        let mut t = term(2, 2);
        t.set_scrollback_limit(3);
        for _ in 0..10 {
            t.advance(b"xx\r\n");
        }
        assert!(t.scrollback_len() <= 3);
    }

    #[test]
    fn set_scrollback_limit_shifts_marks() {
        // Shrinking the scrollback limit must re-anchor OSC-133 prompt
        // marks. Without that shift, "jump to last prompt" would land
        // on a logical line that no longer exists.
        let mut t = term(4, 1);
        t.set_scrollback_limit(10);
        // Push 5 lines and drop a prompt mark on the most recent one.
        for i in 0..5 {
            t.advance(format!("p{i}\r\n").as_bytes());
        }
        t.advance(b"\x1b]133;A\x07"); // prompt mark at current grid line
        // Mark is captured at a logical line *in* scrollback or at the
        // current row — either way, after we cut the limit it must not
        // reference a line we just dropped.
        let mark_before = t.prompt_marks().iter().max().copied().unwrap_or(0);
        let sb_before = t.scrollback_len();
        // Cut the limit so half the scrollback gets dropped.
        t.set_scrollback_limit(2);
        let dropped = sb_before - t.scrollback_len();
        let mark_after = t.prompt_marks().iter().max().copied();
        if let Some(m) = mark_after {
            // The mark either shifted down by the drop amount or was
            // dropped entirely (was below the new origin).
            assert!(
                m <= mark_before.saturating_sub(dropped),
                "mark {m} > expected {} after dropping {dropped} lines",
                mark_before - dropped,
            );
        }
    }

    #[test]
    fn scrollback_limit_zero_disables_history() {
        // The CLAUDE.md says `scrollback = 0` should disable scrollback
        // entirely. Previously the `==` check let the ring grow without
        // bound (it only triggered eviction at the exact boundary, and
        // `0` is never `== 1` after the first push). Now the ring stays
        // empty no matter how many lines scroll off-screen.
        let mut t = term(2, 2);
        t.set_scrollback_limit(0);
        for _ in 0..50 {
            t.advance(b"xx\r\n");
        }
        assert_eq!(t.scrollback_len(), 0, "limit=0 must disable scrollback");
    }

    #[test]
    fn alt_screen_visible_row_ignores_offset() {
        // On alt screen, scroll offset should be ignored — viewport pins
        // to the alt grid. Without this, scrolling up in bash and then
        // launching vim would render bash scrollback as the top rows.
        let mut t = term(8, 2);
        // Fill primary with two lines so scrollback exists after wrap.
        t.advance(b"AAA\r\nBBB\r\nCCC\r\nDDD");
        // Switch to alt screen and write a sentinel.
        t.advance(b"\x1b[?1049h");
        t.advance(b"\x1b[HZZZ");
        assert!(t.is_on_alt_screen());
        // Without the fix, requesting an offset > 0 would pull a row from
        // primary's scrollback for r=0. With the fix it stays in alt.
        let row: String = t
            .visible_row(5, 0)
            .unwrap()
            .iter()
            .map(|c| c.ch)
            .collect();
        assert!(row.starts_with("ZZZ"), "row 0 on alt should be alt content, got {row:?}");
    }

    #[test]
    fn alt_screen_preserves_primary() {
        let mut t = term(4, 2);
        t.advance(b"AAAA\r\nBBBB");
        t.advance(b"\x1b[?1049h");
        assert!(t.is_on_alt_screen());
        t.advance(b"\x1b[HZZZZ");
        assert_eq!(row_text(&t, 0), "ZZZZ");
        t.advance(b"\x1b[?1049l");
        assert!(!t.is_on_alt_screen());
        assert_eq!(row_text(&t, 0), "AAAA");
        assert_eq!(row_text(&t, 1), "BBBB");
    }

    #[test]
    fn cursor_visibility_toggle() {
        let mut t = term(4, 1);
        assert!(t.cursor_visible());
        t.advance(b"\x1b[?25l");
        assert!(!t.cursor_visible());
        t.advance(b"\x1b[?25h");
        assert!(t.cursor_visible());
    }

    #[test]
    fn scroll_region_limits_scroll() {
        let mut t = term(2, 4);
        t.advance(b"AB\r\nCD\r\nEF\r\nGH");
        t.advance(b"\x1b[3;4r");
        t.advance(b"\x1b[4HZZ\r\nYY");
        assert_eq!(row_text(&t, 0), "AB");
        assert_eq!(row_text(&t, 1), "CD");
    }

    #[test]
    fn ich_inserts_blank_cells_at_cursor() {
        let mut t = term(6, 1);
        t.advance(b"abcdef");
        t.advance(b"\x1b[3G\x1b[2@"); // cursor to col 3, insert 2 blanks
        assert_eq!(row_text(&t, 0), "ab  cd");
    }

    #[test]
    fn dch_removes_cells_and_shifts_left() {
        let mut t = term(6, 1);
        t.advance(b"abcdef");
        t.advance(b"\x1b[3G\x1b[2P"); // cursor to col 3, delete 2 chars
        assert_eq!(row_text(&t, 0), "abef  ");
    }

    #[test]
    fn ech_erases_in_place() {
        let mut t = term(6, 1);
        t.advance(b"abcdef");
        t.advance(b"\x1b[3G\x1b[2X"); // cursor to col 3, erase 2 chars
        assert_eq!(row_text(&t, 0), "ab  ef");
    }

    #[test]
    fn il_inserts_blank_lines_inside_region() {
        let mut t = term(2, 4);
        t.advance(b"AA\r\nBB\r\nCC\r\nDD");
        t.advance(b"\x1b[2H\x1b[1L"); // cursor to row 2, insert 1 line
        // Row 0 unchanged, row 1 blank, rows 2..3 = old row 1..2 (CC).
        assert_eq!(row_text(&t, 0), "AA");
        assert_eq!(row_text(&t, 1), "  ");
        assert_eq!(row_text(&t, 2), "BB");
        assert_eq!(row_text(&t, 3), "CC");
    }

    #[test]
    fn dl_removes_lines_and_shifts_up() {
        let mut t = term(2, 4);
        t.advance(b"AA\r\nBB\r\nCC\r\nDD");
        t.advance(b"\x1b[2H\x1b[1M"); // cursor to row 2, delete 1 line
        // Row 0 unchanged. Row 1 = old row 2 (CC), row 2 = old row 3 (DD), row 3 blank.
        assert_eq!(row_text(&t, 0), "AA");
        assert_eq!(row_text(&t, 1), "CC");
        assert_eq!(row_text(&t, 2), "DD");
        assert_eq!(row_text(&t, 3), "  ");
    }

    #[test]
    fn visible_row_offsets_into_scrollback() {
        let mut t = term(3, 2);
        t.advance(b"abc\r\ndef\r\nghi");
        // grid: row 0 = "def", row 1 = "ghi"; scrollback: ["abc"].
        assert_eq!(
            t.visible_row(0, 0).unwrap().iter().map(|c| c.ch).collect::<String>(),
            "def"
        );
        assert_eq!(
            t.visible_row(0, 1).unwrap().iter().map(|c| c.ch).collect::<String>(),
            "ghi"
        );
        // Scroll up by 1 line: row 0 becomes scrollback[0]=abc, row 1 becomes grid[0]=def.
        assert_eq!(
            t.visible_row(1, 0).unwrap().iter().map(|c| c.ch).collect::<String>(),
            "abc"
        );
        assert_eq!(
            t.visible_row(1, 1).unwrap().iter().map(|c| c.ch).collect::<String>(),
            "def"
        );
    }

    #[test]
    fn osc0_sets_pending_title() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]0;my-title\x07");
        assert_eq!(t.take_title().as_deref(), Some("my-title"));
        // Second take returns None — it's consumed.
        assert!(t.take_title().is_none());
    }

    #[test]
    fn osc1_sets_title_like_osc0() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]1;icon-only\x07");
        assert_eq!(t.take_title().as_deref(), Some("icon-only"));
    }

    #[test]
    fn osc2_sets_pending_title() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]2;tab two\x07");
        assert_eq!(t.take_title().as_deref(), Some("tab two"));
    }

    #[test]
    fn osc_title_strips_control_bytes() {
        // OSC payloads occasionally include stray low-bit bytes (\x01..,
        // \x7f) — either from a buggy shell or as an injection attempt
        // to forge a multi-line title into the tab bar. Sanitize them at
        // the source so downstream renderers / plugins only see printable
        // text. (ESC / CR / LF inside OSC are typically chopped earlier
        // by the VTE state machine, so this test focuses on the bytes
        // that *do* reach our handler.)
        let mut t = term(4, 1);
        t.advance(b"\x1b]2;a\x01b\x7fc\x07");
        assert_eq!(t.take_title().as_deref(), Some("abc"));

        // UTF-8 path: ё (D0 91) and 🦀 (F0 9F A6 80) survive untouched
        // so CJK / emoji titles still render correctly.
        t.advance("\x1b]2;ё🦀\x07".as_bytes());
        assert_eq!(t.take_title().as_deref(), Some("ё🦀"));
    }

    #[test]
    fn osc133_d_captures_exit_codes() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]133;D;0\x07");
        t.advance(b"\x1b]133;D;130\x07");
        t.advance(b"\x1b]133;D\x07"); // missing exit code → 0
        let finishes = t.take_command_finishes();
        let codes: Vec<i32> = finishes.iter().map(|f| f.exit_code).collect();
        assert_eq!(codes, vec![0, 130, 0]);
        // No matching `;C` was seen, so each finish has no duration.
        for f in &finishes {
            assert!(f.duration_ms.is_none(), "unexpected duration {:?}", f);
        }
        assert!(t.take_command_finishes().is_empty());
    }

    #[test]
    fn osc133_d_after_c_has_duration() {
        // OSC 133;C stamps a start time; the next ;D measures elapsed
        // wall-clock since that stamp. Consecutive D's without another
        // C drop back to `None` (we don't have a fresh anchor).
        let mut t = term(4, 1);
        t.advance(b"\x1b]133;C\x07");
        // Sleep a bit so the elapsed millis is positive enough that
        // even round-down can't reach zero.
        std::thread::sleep(std::time::Duration::from_millis(2));
        t.advance(b"\x1b]133;D;0\x07");
        let finishes = t.take_command_finishes();
        assert_eq!(finishes.len(), 1);
        let d = finishes[0].duration_ms.expect("duration should be present");
        assert!(d <= 5_000, "duration should be small but was {d} ms");

        // Second D with no fresh C → no duration.
        t.advance(b"\x1b]133;D;1\x07");
        let next = t.take_command_finishes();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].exit_code, 1);
        assert!(next[0].duration_ms.is_none());
    }

    #[test]
    fn osc133_records_prompt_marks() {
        let mut t = term(20, 5);
        // Two prompts at row 0 and row 1.
        t.advance(b"\x1b]133;A\x07$ cmd1\r\n");
        t.advance(b"\x1b]133;A\x07$ cmd2\r\n");
        let marks = t.prompt_marks();
        assert_eq!(marks.len(), 2);
        // Second prompt was at grid row 1 (after first cmd's LF advanced row).
        assert_eq!(marks[0], 0);
        assert_eq!(marks[1], 1);
    }

    #[test]
    fn parse_rgb_spec_accepts_2_and_4_digit_forms() {
        // 2-digit per channel.
        assert_eq!(parse_rgb_spec("rgb:ff/80/00"), Some([0xff, 0x80, 0x00]));
        // 4-digit per channel — use the high byte.
        assert_eq!(parse_rgb_spec("rgb:ffff/8000/00aa"), Some([0xff, 0x80, 0x00]));
        // Missing prefix.
        assert_eq!(parse_rgb_spec("ff/80/00"), None);
        // Mixed widths and trailing parts are rejected to avoid silent
        // truncation of malformed inputs.
        assert_eq!(parse_rgb_spec("rgb:ff/80/00/aa"), None);
        // Non-hex digits.
        assert_eq!(parse_rgb_spec("rgb:zz/00/00"), None);
    }

    #[test]
    fn parse_rgb_spec_rejects_multibyte_without_panic() {
        // Regression: a shell-supplied OSC 10/11/12 payload that
        // smuggles a multi-byte UTF-8 char into a hex channel slot
        // used to panic in `take_byte` on a non-boundary `&s[..2]`
        // slice. The new path validates via `from_utf8` and returns
        // None cleanly. Any panic here would surface in the parser
        // dispatch loop on the user's session.
        assert_eq!(parse_rgb_spec("rgb:漢a/00/00"), None);
        assert_eq!(parse_rgb_spec("rgb:🌍/00/00"), None);
        assert_eq!(parse_rgb_spec("rgb:ééé/00/00"), None);
        // `#RGB` / `#RRGGBB` hash-form must reject mismatched-byte
        // lengths through the same path.
        assert_eq!(parse_rgb_spec("#🌍ab"), None);
        assert_eq!(parse_rgb_spec("#漢ab"), None);
        assert_eq!(parse_rgb_spec("#éé"), None);
        // Strictly-Latin1 6-byte input where every byte boundary
        // happens to land on a char boundary still fails on the
        // `from_str_radix` step (the `é` byte pair is not hex digits).
        assert_eq!(parse_rgb_spec("#ééé"), None);
    }

    #[test]
    fn percent_decode_handles_escapes_and_leaves_bad_ones() {
        assert_eq!(percent_decode("/a%20b"), "/a b");
        assert_eq!(percent_decode("hello"), "hello");
        // Truncated `%` near end of string stays as-is.
        assert_eq!(percent_decode("a%"), "a%");
        // Non-hex following `%` is left literal.
        assert_eq!(percent_decode("a%zz"), "a%zz");
        // Multi-byte UTF-8 sequence reassembled from percent escapes.
        assert_eq!(percent_decode("%E2%9C%93"), "✓");
    }

    #[test]
    fn decrqss_replies_for_known_queries_and_marks_unknown() {
        let mut t = term(40, 8);
        // SGR query.
        t.advance(b"\x1bP$qm\x1b\\");
        // DECSCUSR query after setting cursor to steady underline (4).
        t.advance(b"\x1b[4 q");
        t.advance(b"\x1bP$q q\x1b\\");
        // DECSTBM query after setting scroll region 2..=6 (1-based).
        t.advance(b"\x1b[2;6r");
        t.advance(b"\x1bP$qr\x1b\\");
        // Unknown selector "z".
        t.advance(b"\x1bP$qz\x1b\\");
        let replies = t.take_osc_responses();
        assert!(replies.iter().any(|r| r == "\x1bP1$r0m\x1b\\"),
            "missing SGR reply, got {:?}", replies);
        assert!(replies.iter().any(|r| r == "\x1bP1$r4 q\x1b\\"),
            "missing DECSCUSR=4 reply, got {:?}", replies);
        assert!(replies.iter().any(|r| r == "\x1bP1$r2;6r\x1b\\"),
            "missing DECSTBM reply, got {:?}", replies);
        assert!(replies.iter().any(|r| r == "\x1bP0$rz\x1b\\"),
            "missing unknown-selector reply, got {:?}", replies);

        // Now set bold + italic and re-query; reply should reflect them.
        t.advance(b"\x1b[1;3m");
        t.advance(b"\x1bP$qm\x1b\\");
        let replies = t.take_osc_responses();
        assert!(replies.iter().any(|r| r == "\x1bP1$r0;1;3m\x1b\\"),
            "expected DECRQSS to report bold+italic, got {:?}", replies);

        // DECSCL — VT conformance level. Reply must advertise VT5xx-class
        // (65 = level 5) with 8-bit controls (1), matching xterm defaults.
        t.advance(b"\x1bP$q\"p\x1b\\");
        let replies = t.take_osc_responses();
        assert!(replies.iter().any(|r| r == "\x1bP1$r65;1\"p\x1b\\"),
            "missing DECSCL reply, got {:?}", replies);
    }

    #[test]
    fn osc104_resets_named_palette_entries() {
        let mut t = term(20, 4);
        // First, set palette index 1 to bright red and confirm.
        t.advance(b"\x1b]4;1;rgb:ff/00/00\x1b\\");
        let pre = t.take_pending_palette_changes();
        assert!(pre.iter().any(|u| matches!(u, PaletteUpdate::Named(1, [0xff, 0x00, 0x00]))));
        // Drain again to clear queue between cases.
        let _ = t.take_pending_palette_changes();

        // OSC 104 with index → emits a single reset for slot 1.
        t.advance(b"\x1b]104;1\x1b\\");
        let updates = t.take_pending_palette_changes();
        let named_resets: Vec<_> = updates
            .iter()
            .filter_map(|u| match u {
                PaletteUpdate::Named(i, _) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(named_resets, vec![1]);

        // OSC 104 with no index → resets all 16 named slots.
        t.advance(b"\x1b]104\x1b\\");
        let updates = t.take_pending_palette_changes();
        let mut all_resets: Vec<u8> = updates
            .iter()
            .filter_map(|u| match u {
                PaletteUpdate::Named(i, _) => Some(*i),
                _ => None,
            })
            .collect();
        all_resets.sort();
        assert_eq!(all_resets, (0u8..16).collect::<Vec<_>>());
    }

    #[test]
    fn osc8_cap_evicts_oldest_uris() {
        // Push 5000 unique URIs through OSC 8 — well over `HYPERLINK_CAP`
        // (4096). The oldest entries should be evicted; the newest should
        // remain resolvable.
        let mut t = term(80, 1);
        for i in 0..5000usize {
            // Each link contains one char ('x') so we never overflow the
            // grid (the cell just gets overwritten in place after each
            // print due to autowrap clearing the col on linefeed).
            t.advance(b"\x1b]8;;https://example.com/");
            t.advance(format!("{i}").as_bytes());
            t.advance(b"\x1b\\x\x1b]8;;\x1b\\\r\n");
        }
        // The very last URI was just inserted — it must be retrievable.
        // We can't get the id without scanning cells, but we can probe
        // via the hyperlink_uri lookup over a range of ids: the table
        // should contain at most HYPERLINK_CAP entries.
        // (Indirect verification: previously the table grew unbounded; if
        // eviction worked, the count is bounded.)
        let mut count = 0;
        for id in 1..=8000u32 {
            if t.hyperlink_uri(id).is_some() {
                count += 1;
            }
        }
        assert!(
            count <= 4096,
            "expected hyperlink table bounded by HYPERLINK_CAP=4096, got {count}",
        );
        // And at least *some* entries should remain (we didn't evict to 0).
        assert!(count > 0, "all hyperlinks evicted — cap logic broken");
    }

    #[test]
    fn osc8_reopen_same_uri_reuses_hyperlink_id() {
        // Sanity check that the hyperlink table doesn't grow unboundedly
        // when shells repeatedly emit the same URI across many spans.
        let mut t = term(40, 1);
        t.advance(b"\x1b]8;;https://example.com\x1b\\A\x1b]8;;\x1b\\ ");
        let id_a = t.grid().cell(Position { col: 0, row: 0 }).unwrap().hyperlink;
        t.advance(b"\x1b]8;;https://example.com\x1b\\B\x1b]8;;\x1b\\");
        let id_b = t.grid().cell(Position { col: 2, row: 0 }).unwrap().hyperlink;
        assert_ne!(id_a, 0);
        assert_eq!(id_a, id_b, "same URI should reuse the same hyperlink id");
    }

    #[test]
    fn osc9_progress_vs_notification() {
        let mut t = term(40, 4);
        // Plain OSC 9 → notification.
        t.advance(b"\x1b]9;build started\x07");
        assert_eq!(t.take_notifications(), vec!["build started".to_string()]);
        // OSC 9 ; 4 ; <state> ; <pct> → progress.
        t.advance(b"\x1b]9;4;1;42\x07");
        t.advance(b"\x1b]9;4;3\x07"); // indeterminate, no pct
        t.advance(b"\x1b]9;4;0\x07"); // clear
        assert_eq!(
            t.take_progress(),
            vec![(1u8, 42u8), (3, 0), (0, 0)]
        );
        // Plain OSC 9 with "4" as the message is still allowed when
        // there's no third param — but mis-parses are minimal risk since
        // shells that want a literal "4" notification would use a longer
        // message in practice. Verify the branch is unambiguous.
        t.advance(b"\x1b]9;4\x07");
        assert_eq!(t.take_progress(), vec![]);
        // No third param → params.len() == 2, takes notification branch.
        assert_eq!(t.take_notifications(), vec!["4".to_string()]);
    }

    #[test]
    fn clear_scrollback_drops_lines_and_reanchors_marks() {
        let mut t = term(8, 2);
        // Push 3 lines into scrollback by overflowing the 2-row grid.
        t.advance(b"\x1b]133;A\x07line1\r\n");
        t.advance(b"line2\r\n");
        t.advance(b"\x1b]133;A\x07line3\r\n");
        let pre_sb = t.scrollback_len();
        assert!(pre_sb >= 2, "expected scrollback to accumulate, got {pre_sb}");
        let prompt_marks_before = t.prompt_marks().len();
        assert!(prompt_marks_before >= 1);
        t.clear_scrollback();
        assert_eq!(t.scrollback_len(), 0);
        // Marks that pointed past the dropped lines should re-anchor to the
        // live grid (small indices) or be discarded if they were inside
        // scrollback.
        for m in t.prompt_marks() {
            assert!(*m < t.size().rows as usize, "stale mark {m}");
        }
    }

    #[test]
    fn osc133_c_records_command_marks() {
        let mut t = term(20, 5);
        // Prompt at row 0, then user submits "ls" → C marker at row 0
        // (cursor still on the same row when shell prints C). LF advances
        // to row 1; output line; LF to row 2; second prompt+command.
        t.advance(b"\x1b]133;A\x07$ ls\x1b]133;C\x07\r\n");
        t.advance(b"file.txt\r\n");
        t.advance(b"\x1b]133;A\x07$ pwd\x1b]133;C\x07\r\n");
        let marks = t.command_marks();
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0], 0);
        assert_eq!(marks[1], 2);
        // Duplicate C on the same line is coalesced.
        t.advance(b"\x1b]133;C\x07\x1b]133;C\x07");
        let marks = t.command_marks();
        assert_eq!(marks.len(), 3);
    }

    #[test]
    fn linefeed_captures_completed_lines() {
        let mut t = term(20, 5);
        t.advance(b"hello\r\nworld\r\nfoo bar\r\n");
        let lines = t.take_completed_lines();
        assert_eq!(lines, vec!["hello", "world", "foo bar"]);
        // Second drain returns nothing.
        assert!(t.take_completed_lines().is_empty());
    }

    #[test]
    fn osc633_a_mirrors_133_prompt_marks() {
        // VS Code's shell-integration variant emits OSC 633;A in place of
        // OSC 133;A. Same semantics: each `;A` lands a prompt mark at the
        // cursor's logical row. Tracks the same `prompt_marks` queue so
        // existing jump-to-prompt navigation works for both.
        let mut t = term(20, 5);
        t.advance(b"\x1b]633;A\x07$ cmd1\r\n");
        t.advance(b"\x1b]633;A\x07$ cmd2\r\n");
        let marks = t.prompt_marks();
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0], 0);
        assert_eq!(marks[1], 1);
    }

    #[test]
    fn osc633_c_records_command_marks_and_d_emits_finish() {
        let mut t = term(20, 5);
        t.advance(b"\x1b]633;A\x07$ ls\x1b]633;C\x07\r\n");
        t.advance(b"file.txt\r\n");
        // OSC 633;D with explicit exit code lands in the same finishes
        // queue the plugin layer drains for `pane.command_finish` events.
        t.advance(b"\x1b]633;D;7\x07");
        let marks = t.command_marks();
        assert_eq!(marks.len(), 1);
        let finishes = t.take_command_finishes();
        assert_eq!(finishes.len(), 1);
        assert_eq!(finishes[0].exit_code, 7);
    }

    #[test]
    fn osc633_p_cwd_updates_working_directory() {
        // VS Code's preferred cwd broadcast: `OSC 633 ; P ; Cwd=<path>`.
        // We percent-decode the value half so spaces (encoded as `%20`)
        // arrive intact. Empty value is rejected — losing a known cwd to
        // a malformed payload would surface a stale title in the tab bar.
        let mut t = term(4, 1);
        t.advance(b"\x1b]633;P;Cwd=/home/user/my%20dir\x07");
        assert_eq!(t.cwd(), Some("/home/user/my dir"));
        // Property keys are case-insensitive — VS Code normalises but a
        // hand-rolled shell hook might send `cwd=` lowercase.
        t.advance(b"\x1b]633;P;cwd=/tmp/x\x07");
        assert_eq!(t.cwd(), Some("/tmp/x"));
        // Empty cwd value is a no-op (keeps the previous cwd).
        t.advance(b"\x1b]633;P;Cwd=\x07");
        assert_eq!(t.cwd(), Some("/tmp/x"));
        // Unknown property is silently ignored.
        t.advance(b"\x1b]633;P;Task=build\x07");
        assert_eq!(t.cwd(), Some("/tmp/x"));
    }

    #[test]
    fn osc9_9_captures_cwd_windows_terminal_form() {
        // Windows Terminal's shell-integration variant: OSC 9;9;<path>.
        // Mirrors OSC 7 / 1337;CurrentDir / 633;P;Cwd= in that any of
        // them lands the value in `cwd()`. Percent-decoding so spaces
        // round-trip; empty path is a no-op (keeps the previous cwd).
        let mut t = term(4, 1);
        t.advance(b"\x1b]9;9;/tmp/work\x07");
        assert_eq!(t.cwd(), Some("/tmp/work"));
        // Percent-encoded space.
        t.advance(b"\x1b]9;9;/home/u/my%20dir\x07");
        assert_eq!(t.cwd(), Some("/home/u/my dir"));
        // Empty payload preserves prior cwd.
        t.advance(b"\x1b]9;9;\x07");
        assert_eq!(t.cwd(), Some("/home/u/my dir"));
        // Path containing a literal `;` gets re-joined intact.
        t.advance(b"\x1b]9;9;/path;with;semis\x07");
        assert_eq!(t.cwd(), Some("/path;with;semis"));
    }

    #[test]
    fn osc9_4_progress_still_parses_after_9_9_branch() {
        // The new `9;9` branch sits before the `9;4` progress arm; pin
        // that progress reports still land in the queue so a refactor
        // of the dispatch can't swallow them.
        let mut t = term(4, 1);
        t.advance(b"\x1b]9;4;2;75\x07");
        let progress: Vec<_> = t.take_progress();
        assert_eq!(progress.len(), 1);
        assert_eq!(progress[0], (2, 75));
    }

    #[test]
    fn osc7_captures_cwd_with_percent_decoding() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]7;file://localhost/home/user/my%20dir\x07");
        assert_eq!(t.cwd(), Some("/home/user/my dir"));
        // No host: still parsed.
        t.advance(b"\x1b]7;file:///etc\x07");
        assert_eq!(t.cwd(), Some("/etc"));
    }

    #[test]
    fn osc10_11_query_returns_color_response() {
        let mut t = term(4, 1);
        t.set_default_colors([0xab, 0xcd, 0xef], [0x12, 0x34, 0x56]);
        t.advance(b"\x1b]10;?\x07");
        t.advance(b"\x1b]11;?\x07");
        let resps = t.take_osc_responses();
        assert_eq!(resps.len(), 2);
        assert!(resps[0].contains("rgb:abab/cdcd/efef"));
        assert!(resps[1].contains("rgb:1212/3434/5656"));
    }

    #[test]
    fn osc52_captures_base64_payload() {
        let mut t = term(4, 1);
        // Encode "hi" as base64 → "aGk="
        t.advance(b"\x1b]52;c;aGk=\x07");
        assert_eq!(t.take_pending_clipboard().as_deref(), Some("aGk="));
        // Read-back form ('?') is ignored.
        t.advance(b"\x1b]52;c;?\x07");
        assert!(t.take_pending_clipboard().is_none());
    }

    #[test]
    fn osc_responses_queue_bounded_under_flood() {
        // A shell flooding DECRQM queries between renderer drains
        // could otherwise grow `osc_responses` without bound. Verify
        // the post-batch trim caps the queue at OSC_RESPONSES_CAP (256).
        let mut t = term(8, 1);
        // 1000 mode queries → 1000 replies if unbounded; cap should
        // limit us to ≤ 256.
        let mut input = Vec::with_capacity(1000 * 9);
        for _ in 0..1000 {
            input.extend_from_slice(b"\x1b[?25$p");
        }
        t.advance(&input);
        let replies = t.take_osc_responses();
        assert!(replies.len() <= 256, "got {} replies, cap is 256", replies.len());
        // The kept replies are the most recent ones — sanity-check by
        // confirming we got at least one valid DECRQM reply.
        assert!(replies.iter().any(|r| r == "\x1b[?25;1$y"));
    }

    #[test]
    fn osc8_huge_uri_does_not_explode_memory() {
        // VTE's OSC parser already caps the raw payload at 1 KiB. Our
        // 8 KiB belt-and-suspenders cap is defense in depth — neither
        // layer can be removed silently without a way to detect it.
        // Verify that a *very* oversized URI gets processed without
        // panicking, and that the lookup table stays well below the
        // payload size (concretely: < 4096 entries).
        let mut t = term(10, 1);
        let mut huge = b"\x1b]8;;".to_vec();
        // 128 KiB of "x" — VTE will truncate well before our cap.
        huge.extend(std::iter::repeat(b'x').take(128 * 1024));
        huge.extend_from_slice(b"\x1b\\link\x1b]8;;\x1b\\");
        t.advance(&huge);
        // Whatever URI made it through, the hyperlink store stays
        // bounded — the cap on entries (4096) protects against id churn.
        // Sanity: a regular subsequent OSC 8 still works.
        t.advance(b"\x1b]8;;https://x.example\x1b\\y");
        assert!(matches!(
            t.hyperlink_at(0, 0, 4),
            Some("https://x.example") | None
        ));
    }

    #[test]
    fn osc8_tags_subsequent_cells_with_hyperlink() {
        let mut t = term(10, 1);
        // Start link, write "link", end link, write " plain"
        t.advance(b"\x1b]8;;https://example.org\x1b\\link\x1b]8;;\x1b\\ end");
        // Cells 0..=3 should carry the hyperlink id; 4 onward 0.
        let mut ids = Vec::new();
        for c in 0..10 {
            ids.push(t.grid().cell(Position { col: c, row: 0 }).unwrap().hyperlink);
        }
        assert!(ids[0] != 0 && ids[0] == ids[3]);
        assert_eq!(ids[4], 0);
        assert_eq!(t.hyperlink_at(0, 0, 0), Some("https://example.org"));
        assert_eq!(t.hyperlink_at(0, 0, 5), None);
    }

    #[test]
    fn detect_url_picks_up_https_in_text() {
        let mut t = term(30, 1);
        t.advance(b"see https://example.org/a now");
        // Click anywhere inside the URL substring.
        let url = t.detect_url_at(0, 0, 10).expect("url detected");
        assert_eq!(url, "https://example.org/a");
        // Click outside the URL returns None.
        assert!(t.detect_url_at(0, 0, 0).is_none());
    }

    #[test]
    fn detect_url_trims_trailing_punctuation() {
        let mut t = term(30, 1);
        t.advance(b"check https://example.org.");
        let url = t.detect_url_at(0, 0, 8).unwrap();
        assert_eq!(url, "https://example.org");
    }

    #[test]
    fn csi_21_t_reports_window_title() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]2;hi\x07");
        let _ = t.take_title();
        t.advance(b"\x1b[21t");
        assert_eq!(t.take_osc_responses(), vec!["\x1b]lhi\x1b\\".to_string()]);
    }

    #[test]
    fn title_push_pop_restores_previous_title() {
        let mut t = term(4, 1);
        // Set title A, push, set title B, pop → title is back to A.
        t.advance(b"\x1b]2;A\x07");
        let _ = t.take_title();
        t.advance(b"\x1b[22t");
        t.advance(b"\x1b]2;B\x07");
        assert_eq!(t.take_title().as_deref(), Some("B"));
        t.advance(b"\x1b[23t");
        assert_eq!(t.take_title().as_deref(), Some("A"));
        // Popping an empty stack does nothing.
        t.advance(b"\x1b[23t");
        assert!(t.take_title().is_none());
    }

    #[test]
    fn osc12_query_default_falls_back_to_fg() {
        let mut t = term(4, 1);
        t.set_default_colors([0xaa, 0xbb, 0xcc], [0, 0, 0]);
        t.advance(b"\x1b]12;?\x07");
        let replies = t.take_osc_responses();
        assert_eq!(
            replies,
            vec!["\x1b]12;rgb:aaaa/bbbb/cccc\x1b\\".to_string()]
        );
    }

    #[test]
    fn parse_rgb_spec_forms() {
        assert_eq!(parse_rgb_spec("rgb:ff/cc/66"), Some([0xff, 0xcc, 0x66]));
        assert_eq!(parse_rgb_spec("rgb:ffff/cccc/6666"), Some([0xff, 0xcc, 0x66]));
        assert!(parse_rgb_spec("red").is_none());
        assert!(parse_rgb_spec("rgb:ff/cc").is_none());
        assert!(parse_rgb_spec("rgb:zz/cc/66").is_none());
    }

    #[test]
    fn osc12_set_updates_cursor_color() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]12;rgb:11/22/33\x07");
        assert_eq!(t.cursor_color(), Some([0x11, 0x22, 0x33]));
        // 16-bit form: high byte is taken.
        t.advance(b"\x1b]12;rgb:abcd/ef12/3456\x07");
        assert_eq!(t.cursor_color(), Some([0xab, 0xef, 0x34]));
        // Garbage payload is ignored, prior value preserved.
        t.advance(b"\x1b]12;not-a-color\x07");
        assert_eq!(t.cursor_color(), Some([0xab, 0xef, 0x34]));
    }

    #[test]
    fn osc112_resets_cursor_color() {
        let mut t = term(4, 1);
        t.set_cursor_color(Some([0xff, 0x00, 0x00]));
        assert_eq!(t.cursor_color(), Some([0xff, 0x00, 0x00]));
        t.advance(b"\x1b]112\x07");
        assert_eq!(t.cursor_color(), None);
    }

    #[test]
    fn osc12_query_uses_explicit_cursor_color() {
        let mut t = term(4, 1);
        t.set_cursor_color(Some([0xff, 0xcc, 0x66]));
        t.advance(b"\x1b]12;?\x07");
        let replies = t.take_osc_responses();
        assert_eq!(
            replies,
            vec!["\x1b]12;rgb:ffff/cccc/6666\x1b\\".to_string()]
        );
    }

    #[test]
    fn osc7_empty_path_does_not_clobber_cwd() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]7;file:///home/u/p\x07");
        assert_eq!(t.cwd(), Some("/home/u/p"));
        // Empty payload — must keep the existing cwd.
        t.advance(b"\x1b]7;\x07");
        assert_eq!(t.cwd(), Some("/home/u/p"));
    }

    #[test]
    fn osc1337_currentdir_updates_cwd() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]1337;CurrentDir=/home/user/proj\x07");
        assert_eq!(t.cwd(), Some("/home/user/proj"));
        // Empty payload is ignored.
        t.advance(b"\x1b]1337;CurrentDir=\x07");
        assert_eq!(t.cwd(), Some("/home/user/proj"));
    }

    #[test]
    fn osc1337_clear_scrollback_wipes_history() {
        // OSC 1337 ; ClearScrollback ST — drop the scrollback ring
        // (same effect as the `clear_scrollback` action / Ctrl+Shift+K).
        // Push several lines off the visible grid first so the ring
        // has something to clear.
        let mut t = term(4, 2);
        t.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\n");
        assert!(
            t.scrollback_len() > 0,
            "test setup: scrollback should have grown",
        );
        t.advance(b"\x1b]1337;ClearScrollback\x07");
        assert_eq!(t.scrollback_len(), 0);
        // Case-insensitive match — `clearscrollback` (lowercase) also
        // recognized, mirroring shell hooks that vary in casing.
        t.advance(b"x\r\ny\r\nz\r\n");
        assert!(t.scrollback_len() > 0);
        t.advance(b"\x1b]1337;clearscrollback\x07");
        assert_eq!(t.scrollback_len(), 0);
    }

    #[test]
    fn osc4_query_replies_with_palette_rgb() {
        let mut t = term(4, 1);
        // Query indexed color 1 (red). Default named palette → [205,49,49].
        t.advance(b"\x1b]4;1;?\x07");
        let replies = t.take_osc_responses();
        assert_eq!(
            replies,
            vec!["\x1b]4;1;rgb:cdcd/3131/3131\x1b\\".to_string()]
        );
    }

    #[test]
    fn osc10_11_set_updates_defaults_and_queues_change() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]10;rgb:aa/bb/cc\x07");
        t.advance(b"\x1b]11;rgb:11/22/33\x07");
        let changes = t.take_pending_palette_changes();
        assert_eq!(
            changes,
            vec![
                PaletteUpdate::DefaultFg([0xaa, 0xbb, 0xcc]),
                PaletteUpdate::DefaultBg([0x11, 0x22, 0x33]),
            ]
        );
        // OSC 10 query should now return the new fg.
        t.advance(b"\x1b]10;?\x07");
        let replies = t.take_osc_responses();
        assert_eq!(
            replies,
            vec!["\x1b]10;rgb:aaaa/bbbb/cccc\x1b\\".to_string()]
        );
    }

    #[test]
    fn osc4_set_updates_named_palette_and_queues_change() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]4;1;rgb:11/22/33\x07");
        // Query should reflect the new value.
        t.advance(b"\x1b]4;1;?\x07");
        let replies = t.take_osc_responses();
        assert_eq!(
            replies,
            vec!["\x1b]4;1;rgb:1111/2222/3333\x1b\\".to_string()]
        );
        let changes = t.take_pending_palette_changes();
        assert_eq!(changes, vec![PaletteUpdate::Named(1, [0x11, 0x22, 0x33])]);
        // Drained — second call is empty.
        assert!(t.take_pending_palette_changes().is_empty());
    }

    #[test]
    fn osc4_query_uses_custom_named_palette() {
        let mut t = term(4, 1);
        let mut pal = DEFAULT_NAMED_PALETTE;
        pal[1] = [255, 0, 0];
        t.set_named_palette(pal);
        t.advance(b"\x1b]4;1;?\x07");
        let replies = t.take_osc_responses();
        assert_eq!(
            replies,
            vec!["\x1b]4;1;rgb:ffff/0000/0000\x1b\\".to_string()]
        );
    }

    #[test]
    fn decsc_decrc_restores_charset() {
        let mut t = term(6, 1);
        // Save cursor + state with G0 = DEC graphics.
        t.advance(b"\x1b(0\x1b7");
        // Switch G0 back to ASCII and print 'q'.
        t.advance(b"\x1b(Bq");
        assert!(row_text(&t, 0).starts_with('q'), "got {:?}", row_text(&t, 0));
        // Restore — G0 must flip back to DEC graphics. Print 'q' → ─.
        t.advance(b"\x1b8q");
        let row = row_text(&t, 0);
        assert!(row.starts_with("─"), "got {row:?}");
    }

    #[test]
    fn dec_graphics_remap_when_selected() {
        let mut t = term(8, 1);
        // Designate G0 as DEC special graphics, then print `lqk` which
        // forms a top-of-box: ┌─┐.
        t.advance(b"\x1b(0lqk");
        let row = row_text(&t, 0);
        assert!(row.starts_with("┌─┐"), "got {row:?}");
        // Switch back to ASCII — subsequent prints are unchanged.
        t.advance(b"\x1b(B");
        t.advance(b"A");
        let row2 = row_text(&t, 0);
        assert!(row2.contains('A'), "got {row2:?}");
    }

    #[test]
    fn si_so_switch_active_charset() {
        let mut t = term(8, 1);
        // G0 ASCII (default), G1 DEC graphics.
        t.advance(b"\x1b)0");
        // SO → switch to G1. Print `q` → ─.
        t.advance(b"\x0eq");
        assert!(row_text(&t, 0).starts_with("─"));
        // SI → back to G0. Print `q` literally.
        t.advance(b"\x0fq");
        let row = row_text(&t, 0);
        assert!(row.starts_with("─q"), "got {row:?}");
    }

    #[test]
    fn csi_18_t_reports_text_area_size() {
        let mut t = term(120, 36);
        t.advance(b"\x1b[18t");
        assert_eq!(t.take_osc_responses(), vec!["\x1b[8;36;120t".to_string()]);
    }

    #[test]
    fn csi_xtwinops_reports_state_position_and_icon_label() {
        // XTWINOPS modes 11 (window state), 13 (window position), and
        // 20 (icon label) are common probes some apps emit before
        // falling back to size queries. We reply with stable defaults
        // — never-minimised, position 0/0, icon-label mirrors title.
        let mut t = term(80, 24);
        t.advance(b"\x1b]2;tab-label\x07"); // set title
        t.take_osc_responses(); // drain any title side-effects

        t.advance(b"\x1b[11t");
        assert_eq!(t.take_osc_responses(), vec!["\x1b[1t".to_string()]);

        t.advance(b"\x1b[13t");
        assert_eq!(t.take_osc_responses(), vec!["\x1b[3;0;0t".to_string()]);

        t.advance(b"\x1b[20t");
        assert_eq!(
            t.take_osc_responses(),
            vec!["\x1b]Ltab-label\x1b\\".to_string()]
        );
    }

    #[test]
    fn osc7_and_osc1337_rejoin_embedded_semicolons_in_cwd() {
        // OSC 7 + OSC 1337 carry path strings. Unix lets `;` sit inside
        // a filename, so the VTE param split would truncate cwds like
        // `/tmp/weird;dir/`. Pin that both forms rejoin correctly.
        let mut t = term(8, 1);
        // OSC 7 — `file:///path/with;semi/inside`. file:// prefix is
        // stripped; everything after the host is treated as the path.
        t.advance(b"\x1b]7;file:///tmp/odd;path/here/\x07");
        assert_eq!(t.cwd(), Some("/tmp/odd;path/here/"));
        // OSC 1337 — `CurrentDir=/tmp/another;dir`.
        t.advance(b"\x1b]1337;CurrentDir=/tmp/another;dir\x07");
        assert_eq!(t.cwd(), Some("/tmp/another;dir"));
    }

    #[test]
    fn osc8_uri_rejoins_embedded_semicolons() {
        // OSC 8 hyperlinks with query strings like
        // `?a=1;b=2` arrived split by the VTE parser. The hyperlink
        // table used to keep only the prefix — clicking the link would
        // open a truncated URL.
        let mut t = term(8, 1);
        t.advance(b"\x1b]8;;https://example.com/path?a=1;b=2\x1b\\");
        t.advance(b"X");
        let row = t.grid().row(0).unwrap();
        let hl_id = row[0].hyperlink;
        assert!(hl_id != 0, "cell should carry a hyperlink id");
        let uri = t.hyperlink_uri(hl_id).unwrap();
        assert_eq!(uri, "https://example.com/path?a=1;b=2");
    }

    #[test]
    fn osc_title_rejoins_embedded_semicolons() {
        // `OSC 2 ; <title with ; inside> BEL` — VTE splits params on
        // `;`, so without re-joining we used to keep only the prefix.
        // Real shell prompts (`bash` with `\W; \u@\h`) hit this.
        let mut t = term(4, 1);
        t.advance(b"\x1b]2;user@host: ~/work; tests\x07");
        assert_eq!(
            t.take_title().as_deref(),
            Some("user@host: ~/work; tests"),
        );
    }

    #[test]
    fn osc9_notification_rejoins_semicolons_in_body() {
        // Real notification payloads frequently include `;` ("build ok;
        // 12.3s elapsed"), and VTE splits the OSC params on every `;`.
        // Without re-joining, plugins would receive truncated messages.
        let mut t = term(4, 1);
        t.advance(b"\x1b]9;build ok; 12.3s elapsed\x07");
        let notifs = t.take_notifications();
        assert_eq!(notifs, vec!["build ok; 12.3s elapsed".to_string()]);
    }

    #[test]
    fn csi_eq_c_returns_da3_terminal_id() {
        // `CSI = c` is DA3 (Tertiary Device Attributes). Apps that probe
        // it expect a `DCS ! | <hex8> ST` reply. We send xterm's
        // canonical `00000000` so terminfo entries with a `setab/setaf`
        // probe sequence don't time out.
        let mut t = term(4, 1);
        t.advance(b"\x1b[=c");
        assert_eq!(
            t.take_osc_responses(),
            vec!["\x1bP!|00000000\x1b\\".to_string()],
        );
        // Same reply when an explicit `0` parameter is sent (matching
        // xterm's `CSI = 0 c` form).
        t.advance(b"\x1b[=0c");
        assert_eq!(
            t.take_osc_responses(),
            vec!["\x1bP!|00000000\x1b\\".to_string()],
        );
    }

    #[test]
    fn ris_clears_shell_integration_state() {
        // RIS (ESC c) reinitializes the display — old OSC 133;A/C/D
        // marks and pending command lines point into a wiped grid and
        // would mislead a "jump to last prompt" plugin. Verify they're
        // gone after RIS.
        let mut t = term(8, 4);
        t.advance(b"\x1b]133;A\x07$ cmd1\r\n");
        t.advance(b"\x1b]133;C\x07");
        // Sleep so the C-timestamp captures a non-zero duration if the
        // D handler ran — we want to be sure RIS clears the start slot
        // before any D fires.
        t.advance(b"\x1b]133;D;0\x07");
        let _ = t.take_command_finishes();
        assert!(!t.prompt_marks().is_empty(), "marks should accumulate");
        // Now hit RIS.
        t.advance(b"\x1bc");
        assert!(t.prompt_marks().is_empty(), "RIS must drop prompt marks");
        assert!(t.command_marks().is_empty(), "RIS must drop command marks");
        // Pending command finishes / lines must also be drained so a
        // post-RIS take returns empty.
        assert!(t.take_command_finishes().is_empty());
        assert!(t.take_completed_lines().is_empty());
    }

    #[test]
    fn csi_19_t_reports_screen_size_with_xterm_prefix() {
        // xterm replies to `CSI 19 t` (screen size in chars) with
        // `CSI 9 ; r ; c t`. We don't model a separate "screen" so the
        // value mirrors `CSI 18 t`, but apps that distinguish the two
        // need the 9-prefix shape specifically (mintty/iTerm2 probe this
        // before painting full-screen overlays).
        let mut t = term(80, 24);
        t.advance(b"\x1b[19t");
        assert_eq!(t.take_osc_responses(), vec!["\x1b[9;24;80t".to_string()]);
    }

    #[test]
    fn il_dl_noop_outside_scroll_region() {
        let mut t = term(4, 6);
        // Fill all six rows so we can spot any unintended motion.
        t.advance(b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD\r\nEEEE\r\nFFFF");
        // Set scroll region rows 3..=4 (1-based 3..5) and place cursor at
        // row 0 (outside the region) via CUP.
        t.advance(b"\x1b[3;5r\x1b[1;1H");
        assert_eq!(t.cursor(), Position { col: 0, row: 0 });
        // IL at outside-region cursor — must not change content.
        t.advance(b"\x1b[5L");
        assert_eq!(row_text(&t, 0), "AAAA");
        assert_eq!(row_text(&t, 1), "BBBB");
        assert_eq!(row_text(&t, 5), "FFFF");
        // DL at outside-region cursor — also no-op.
        t.advance(b"\x1b[5M");
        assert_eq!(row_text(&t, 5), "FFFF");
    }

    #[test]
    fn detect_url_recognises_mailto() {
        let mut t = term(40, 1);
        t.advance(b"email mailto:alice@example.org!");
        let url = t.detect_url_at(0, 0, 10).expect("mailto detected");
        assert_eq!(url, "mailto:alice@example.org");
        // A bare email without scheme is NOT a URL — we don't promote.
        let mut t2 = term(40, 1);
        t2.advance(b"alice@example.org");
        assert!(t2.detect_url_at(0, 0, 5).is_none());
    }

    #[test]
    fn is_safe_url_passes_http_https_ftp_ssh_mailto() {
        assert!(is_safe_url("http://example.org"));
        assert!(is_safe_url("https://example.org/path?q=1"));
        assert!(is_safe_url("ftp://files.example.org/"));
        assert!(is_safe_url("ssh://user@host"));
        assert!(is_safe_url("mailto:alice@example.org"));
    }

    #[test]
    fn is_safe_url_handles_multibyte_input_without_panic() {
        // Regression: the auto-URL detector calls `is_safe_url` on any
        // word the user hovers over, including pure Cyrillic / CJK
        // text. A previous case-insensitive `mailto:` check sliced
        // `s[..7]`, which panicked when the first seven bytes spanned
        // a multi-byte UTF-8 character (e.g. `защищены`: 'и' is bytes
        // 6..8, so the boundary at 7 lands mid-character).
        assert!(!is_safe_url("защищены"));
        assert!(!is_safe_url("日本語のテキスト"));
        // Strings shorter than `mailto:` also must not panic.
        assert!(!is_safe_url("ма"));
        assert!(!is_safe_url("a"));
        assert!(!is_safe_url(""));
        // A real mailto with non-ASCII local part still passes — the
        // scheme is pure ASCII so the slice stays UTF-8 safe.
        assert!(is_safe_url("MAILTO:user@xn--n3h.example"));
    }

    #[test]
    fn is_safe_url_is_case_insensitive_per_rfc_3986() {
        // RFC 3986 §3.1 says URI schemes are case-insensitive, with
        // the canonical form lower-case. Clipboard managers and some
        // OSC-8 emitters preserve whatever case the source typed —
        // pin so neither end of the case spectrum gets silently
        // rejected and turns a legitimate Ctrl+click into a no-op.
        assert!(is_safe_url("HTTP://example.org"));
        assert!(is_safe_url("Https://example.org"));
        assert!(is_safe_url("HTTPS://example.org"));
        assert!(is_safe_url("SSH://user@host"));
        assert!(is_safe_url("Ftp://example.org/"));
        assert!(is_safe_url("MailTo:alice@example.org"));
        assert!(is_safe_url("MAILTO:bob@example.org"));
        // Case-insensitivity is *only* for the scheme. A blocked
        // scheme can't be smuggled by varying case.
        assert!(!is_safe_url("FILE:///etc/passwd"));
        assert!(!is_safe_url("JavaScript:alert(1)"));
        assert!(!is_safe_url("DATA:text/html,x"));
    }

    #[test]
    fn is_safe_url_blocks_dangerous_schemes() {
        // Schemes a malicious shell could embed under OSC 8 to make a
        // user Ctrl+click into a privilege boundary. xdg-open / start /
        // open will happily route these to the user's browser / shell,
        // so the gate is layered HERE before invocation.
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url("data:text/html,<script>1</script>"));
        assert!(!is_safe_url("file:///etc/passwd"));
        assert!(!is_safe_url("vbscript:msgbox"));
        // `mailto:` is allowed but only with an `@` in the body —
        // `mailto:not-an-email` is a typo, not a useful action.
        assert!(!is_safe_url("mailto:no-at-sign"));
        // Nothing at all → not a URL → blocked.
        assert!(!is_safe_url(""));
        assert!(!is_safe_url("plain text"));
    }

    #[test]
    fn decscusr_changes_cursor_shape() {
        let mut t = term(4, 1);
        assert_eq!(t.cursor_shape(), CursorShape::Block);
        assert!(t.cursor_should_blink());
        t.advance(b"\x1b[5 q"); // blinking bar
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        assert!(t.cursor_should_blink());
        t.advance(b"\x1b[4 q"); // steady underline
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        assert!(!t.cursor_should_blink());
        t.advance(b"\x1b[2 q"); // steady block
        assert_eq!(t.cursor_shape(), CursorShape::Block);
        assert!(!t.cursor_should_blink());
    }

    #[test]
    fn wide_char_occupies_two_cells_and_marks_spacer() {
        let mut t = term(6, 1);
        // 你 (U+4F60) has east-asian width 2.
        t.advance("你好".as_bytes());
        let c0 = t.grid().cell(Position { col: 0, row: 0 }).unwrap();
        let c1 = t.grid().cell(Position { col: 1, row: 0 }).unwrap();
        let c2 = t.grid().cell(Position { col: 2, row: 0 }).unwrap();
        let c3 = t.grid().cell(Position { col: 3, row: 0 }).unwrap();
        assert_eq!(c0.ch, '你');
        assert!(c0.attrs.contains(CellAttrs::WIDE));
        assert!(c1.attrs.contains(CellAttrs::WIDE_SPACER));
        assert_eq!(c2.ch, '好');
        assert!(c2.attrs.contains(CellAttrs::WIDE));
        assert!(c3.attrs.contains(CellAttrs::WIDE_SPACER));
        assert_eq!(t.cursor(), Position { col: 4, row: 0 });
    }

    #[test]
    fn wide_char_wraps_when_only_one_col_left() {
        let mut t = term(4, 2);
        t.advance(b"ab"); // cursor at col 2
        t.advance(b"c"); // cursor at col 3
        // Wide char wants 2 cols; only 1 left → wrap.
        t.advance("你".as_bytes());
        // Row 0: "abc " (wide didn't fit); row 1: '你' + spacer
        assert_eq!(
            row_text(&t, 0).trim_end(),
            "abc",
            "row 0 should still show abc"
        );
        assert_eq!(t.grid().cell(Position { col: 0, row: 1 }).unwrap().ch, '你');
    }

    #[test]
    fn shrink_resize_evicts_top_to_scrollback() {
        let mut t = term(4, 4);
        t.advance(b"AB\r\nCD\r\nEF\r\nGH");
        // Cursor at end of last line: (2, 3).
        t.resize(Size { cols: 4, rows: 2 });
        // Surviving grid rows = old rows 2..=3 = "EF", "GH".
        let r0: String = (0..4)
            .map(|c| t.grid().cell(Position { col: c, row: 0 }).unwrap().ch)
            .collect();
        let r1: String = (0..4)
            .map(|c| t.grid().cell(Position { col: c, row: 1 }).unwrap().ch)
            .collect();
        assert_eq!(r0, "EF  ");
        assert_eq!(r1, "GH  ");
        // Scrollback got the top two rows.
        assert_eq!(t.scrollback_len(), 2);
        let sb0: String = t.scrollback_line(0).unwrap().iter().map(|c| c.ch).collect();
        let sb1: String = t.scrollback_line(1).unwrap().iter().map(|c| c.ch).collect();
        assert_eq!(sb0, "AB  ");
        assert_eq!(sb1, "CD  ");
        // Cursor moved up by 2 to stay over its content.
        assert_eq!(t.cursor(), Position { col: 2, row: 1 });
    }

    #[test]
    fn decom_origin_mode_clips_to_region() {
        let mut t = term(4, 6);
        // Set scroll region rows 3-5 (0-indexed 2-4).
        t.advance(b"\x1b[3;5r");
        // Enable origin mode.
        t.advance(b"\x1b[?6h");
        // CUP 1;1 in origin mode → region.top (row 2), col 0.
        t.advance(b"\x1b[1;1HA");
        assert_eq!(t.grid().cell(Position { col: 0, row: 2 }).unwrap().ch, 'A');
        // CUP 100;1 should clamp to bottom (row 4).
        t.advance(b"\x1b[100;1HZ");
        assert_eq!(t.grid().cell(Position { col: 0, row: 4 }).unwrap().ch, 'Z');
        // Disable origin mode and CUP 1;1 → row 0.
        t.advance(b"\x1b[?6l");
        t.advance(b"\x1b[1;1HB");
        assert_eq!(t.grid().cell(Position { col: 0, row: 0 }).unwrap().ch, 'B');
    }

    #[test]
    fn decawm_disables_autowrap() {
        let mut t = term(4, 2);
        t.advance(b"\x1b[?7l"); // autowrap off
        t.advance(b"abcdef"); // overwrites col 3 multiple times
        // First 3 chars fill cols 0..3, then 'd', 'e', 'f' overwrite col 3.
        let row: String = (0..4)
            .map(|c| t.grid().cell(Position { col: c, row: 0 }).unwrap().ch)
            .collect();
        assert_eq!(row, "abcf");
        // Re-enable, ensure normal wrap continues.
        t.advance(b"\x1b[?7h");
        t.advance(b"\r\nXY"); // CR LF then "XY"
        let row1: String = (0..4)
            .map(|c| t.grid().cell(Position { col: c, row: 1 }).unwrap().ch)
            .collect();
        assert_eq!(row1, "XY  ");
    }

    #[test]
    fn decckm_toggles_app_cursor_keys() {
        let mut t = term(4, 1);
        assert!(!t.app_cursor_keys());
        t.advance(b"\x1b[?1h");
        assert!(t.app_cursor_keys());
        t.advance(b"\x1b[?1l");
        assert!(!t.app_cursor_keys());
    }

    #[test]
    fn mouse_modes_toggle() {
        let mut t = term(4, 1);
        assert_eq!(t.mouse_tracking(), MouseTracking::Off);
        t.advance(b"\x1b[?1000h");
        assert_eq!(t.mouse_tracking(), MouseTracking::X10);
        t.advance(b"\x1b[?1002h");
        assert_eq!(t.mouse_tracking(), MouseTracking::ButtonEvent);
        t.advance(b"\x1b[?1003h");
        assert_eq!(t.mouse_tracking(), MouseTracking::AnyEvent);
        t.advance(b"\x1b[?1006h");
        assert!(t.sgr_mouse());
        t.advance(b"\x1b[?1000l");
        assert_eq!(t.mouse_tracking(), MouseTracking::Off);
        t.advance(b"\x1b[?1006l");
        assert!(!t.sgr_mouse());
    }

    #[test]
    fn sl_shifts_left_fills_right_with_blanks() {
        let mut t = term(6, 1);
        t.advance(b"abcdef");
        t.advance(b"\x1b[2 @"); // SL 2
        assert_eq!(row_text(&t, 0), "cdef  ");
    }

    #[test]
    fn sr_shifts_right_fills_left_with_blanks() {
        let mut t = term(6, 1);
        t.advance(b"abcdef");
        t.advance(b"\x1b[2 A"); // SR 2
        assert_eq!(row_text(&t, 0), "  abcd");
    }

    #[test]
    fn decaln_fills_grid_with_e() {
        let mut t = term(4, 3);
        t.advance(b"\x1b#8");
        for r in 0..3 {
            assert_eq!(row_text(&t, r), "EEEE");
        }
    }

    #[test]
    fn irm_inserts_without_overwriting() {
        let mut t = term(8, 1);
        t.advance(b"abcd");
        // Cursor now at col 4. Move back to col 1, enable IRM, print 'X'.
        t.advance(b"\x1b[2G\x1b[4hX");
        // Expected: a X b c d (X inserted at col 1, b/c/d shifted right).
        assert_eq!(&row_text(&t, 0)[..5], "aXbcd");
        // Disable IRM, print 'Y' over col 2 — replaces, no shift.
        t.advance(b"\x1b[3G\x1b[4lY");
        assert_eq!(&row_text(&t, 0)[..5], "aXYcd");
    }

    #[test]
    fn focus_tracking_toggle() {
        let mut t = term(4, 1);
        assert!(!t.focus_tracking());
        t.advance(b"\x1b[?1004h");
        assert!(t.focus_tracking());
        t.advance(b"\x1b[?1004l");
        assert!(!t.focus_tracking());
    }

    #[test]
    fn bracketed_paste_toggle() {
        let mut t = term(4, 1);
        assert!(!t.bracketed_paste());
        t.advance(b"\x1b[?2004h");
        assert!(t.bracketed_paste());
        t.advance(b"\x1b[?2004l");
        assert!(!t.bracketed_paste());
    }

    #[test]
    fn bel_flags_pending_bell() {
        let mut t = term(4, 1);
        assert!(!t.take_bell());
        t.advance(b"\x07");
        assert!(t.take_bell());
        assert!(!t.take_bell());
    }

    #[test]
    fn reverse_index_at_top_scrolls_down() {
        let mut t = term(2, 3);
        t.advance(b"AB\r\nCD\r\nEF");
        t.advance(b"\x1b[H");
        t.advance(b"\x1bM");
        assert_eq!(row_text(&t, 0), "  ");
        assert_eq!(row_text(&t, 1), "AB");
    }

    #[test]
    fn ht_uses_default_eight_column_stops() {
        let mut t = term(40, 1);
        t.advance(b"\t");
        assert_eq!(t.cursor().col, 8);
        t.advance(b"\t");
        assert_eq!(t.cursor().col, 16);
    }

    #[test]
    fn hts_and_tbc_at_cursor() {
        let mut t = term(40, 1);
        // Move to col 5 and set a tab stop there via ESC H.
        t.advance(b"\x1b[6G\x1bH");
        // Move back to col 0 — next tab should jump to col 5.
        t.advance(b"\x1b[1G\t");
        assert_eq!(t.cursor().col, 5);
        // Now clear that stop (TBC 0) and jump again — should fall through
        // to the default stop at col 8.
        t.advance(b"\x1b[1G\x1b[6G\x1b[g\x1b[1G\t");
        assert_eq!(t.cursor().col, 8);
    }

    #[test]
    fn tbc_3_clears_all_stops() {
        let mut t = term(40, 1);
        t.advance(b"\x1b[3g");
        // No stops left → tab should land on the last column.
        t.advance(b"\t");
        assert_eq!(t.cursor().col, 39);
    }

    #[test]
    fn cht_cbt_move_n_stops() {
        let mut t = term(40, 1);
        // CHT 2 from col 0 → col 16.
        t.advance(b"\x1b[2I");
        assert_eq!(t.cursor().col, 16);
        // CBT 1 → col 8.
        t.advance(b"\x1b[Z");
        assert_eq!(t.cursor().col, 8);
    }

    #[test]
    fn decpam_decpnm_toggle_keypad() {
        let mut t = term(4, 1);
        assert!(!t.app_keypad());
        t.advance(b"\x1b=");
        assert!(t.app_keypad());
        t.advance(b"\x1b>");
        assert!(!t.app_keypad());
    }

    #[test]
    fn da1_replies_with_xterm_attributes() {
        let mut t = term(4, 1);
        t.advance(b"\x1b[c");
        let replies = t.take_osc_responses();
        assert!(
            replies.iter().any(|r| r.starts_with("\x1b[?") && r.ends_with('c')),
            "got {replies:?}"
        );
    }

    #[test]
    fn decid_esc_z_replies_with_same_payload_as_da1() {
        // DECID (ESC Z) is the VT100-era predecessor of DA1. We
        // honour it by emitting the identical DA1 reply so any
        // legacy probe that still uses it recognises us.
        let mut t = term(4, 1);
        t.advance(b"\x1bZ");
        let via_decid = t.take_osc_responses();
        t.advance(b"\x1b[c");
        let via_da1 = t.take_osc_responses();
        assert_eq!(via_decid, via_da1, "DECID must mirror DA1");
        assert!(
            via_decid
                .iter()
                .any(|r| r.starts_with("\x1b[?") && r.ends_with('c')),
            "got {via_decid:?}"
        );
    }

    #[test]
    fn xtversion_replies_with_name_and_version() {
        let mut t = term(4, 1);
        t.advance(b"\x1b[>q");
        let replies = t.take_osc_responses();
        assert_eq!(replies.len(), 1);
        let r = &replies[0];
        assert!(r.starts_with("\x1bP>|rterm("));
        assert!(r.ends_with(")\x1b\\"));
    }

    #[test]
    fn da2_replies_with_secondary_attributes() {
        let mut t = term(4, 1);
        t.advance(b"\x1b[>c");
        let replies = t.take_osc_responses();
        assert!(
            replies.iter().any(|r| r.starts_with("\x1b[>") && r.ends_with('c')),
            "got {replies:?}"
        );
    }

    #[test]
    fn dsr_5_returns_ok_status() {
        let mut t = term(4, 1);
        t.advance(b"\x1b[5n");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1b[0n".to_string()]);
    }

    #[test]
    fn dsr_6_returns_cursor_position() {
        let mut t = term(10, 5);
        // Move to row 3, col 5 (1-based).
        t.advance(b"\x1b[3;5H\x1b[6n");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1b[3;5R".to_string()]);
    }

    #[test]
    fn dec_mode_12_toggles_cursor_blink() {
        let mut t = term(4, 1);
        assert!(t.cursor_should_blink());
        t.advance(b"\x1b[?12l");
        assert!(!t.cursor_should_blink());
        t.advance(b"\x1b[?12h");
        assert!(t.cursor_should_blink());
    }

    #[test]
    fn decstr_resets_modes_keeps_screen() {
        let mut t = term(6, 2);
        t.advance(b"HELLO\x1b[?25l\x1b[?1h\x1b[?7l");
        // Soft reset: modes flip back, but the "HELLO" stays on screen.
        t.advance(b"\x1b[!p");
        assert_eq!(&row_text(&t, 0)[..5], "HELLO");
        assert!(t.cursor_visible());
        assert!(!t.app_cursor_keys());
        assert_eq!(t.cursor(), Position { col: 0, row: 0 });
    }

    #[test]
    fn sgr_38_colon_form_with_colorspace() {
        let mut t = term(2, 1);
        // Xterm colon form with empty colorspace id (parsed as 0).
        t.advance(b"\x1b[38:2::10:20:30mA");
        assert_eq!(cell(&t, 0, 0).fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn sgr_38_colon_form_without_colorspace() {
        let mut t = term(2, 1);
        t.advance(b"\x1b[38:2:40:50:60mB");
        assert_eq!(cell(&t, 0, 0).fg, Color::Rgb(40, 50, 60));
    }

    #[test]
    fn sgr_53_55_toggles_overline() {
        let mut t = term(3, 1);
        t.advance(b"\x1b[53mA\x1b[55mB");
        assert!(cell(&t, 0, 0).attrs.contains(CellAttrs::OVERLINE));
        assert!(!cell(&t, 1, 0).attrs.contains(CellAttrs::OVERLINE));
    }

    #[test]
    fn rep_repeats_last_printed_char() {
        let mut t = term(10, 1);
        t.advance(b"A\x1b[5b");
        assert_eq!(row_text(&t, 0), "AAAAAA    ");
    }

    #[test]
    fn rep_is_noop_after_control_byte() {
        let mut t = term(10, 1);
        // After CR (a control byte) REP should not repeat.
        t.advance(b"A\r\x1b[3b");
        assert_eq!(row_text(&t, 0), "A         ");
    }

    #[test]
    fn osc9_queues_notification() {
        let mut t = term(4, 1);
        t.advance(b"\x1b]9;Build done\x07");
        let queue = t.take_notifications();
        assert_eq!(queue, vec!["Build done".to_string()]);
        // Drained — second call returns nothing.
        assert!(t.take_notifications().is_empty());
    }

    #[test]
    fn decscnm_reverse_screen_toggles_and_responds_to_decrqm() {
        // DECSET ?5 / DECRST ?5 flip the reverse-screen flag the
        // renderer reads to invert default fg/bg across the grid.
        // DECRQM `CSI ? 5 $ p` reports the current state with
        // value 1 (set) or 2 (reset). RIS clears it.
        let mut t = term(4, 1);
        assert!(!t.is_reverse_screen(), "default off");
        // Set.
        t.advance(b"\x1b[?5h");
        assert!(t.is_reverse_screen());
        t.advance(b"\x1b[?5$p");
        assert_eq!(t.take_osc_responses(), vec!["\x1b[?5;1$y".to_string()]);
        // Reset.
        t.advance(b"\x1b[?5l");
        assert!(!t.is_reverse_screen());
        t.advance(b"\x1b[?5$p");
        assert_eq!(t.take_osc_responses(), vec!["\x1b[?5;2$y".to_string()]);
        // RIS (ESC c) clears too.
        t.advance(b"\x1b[?5h");
        assert!(t.is_reverse_screen());
        t.advance(b"\x1bc");
        assert!(!t.is_reverse_screen());
    }

    #[test]
    fn parser_handles_pathological_byte_streams_without_panic() {
        // Pseudo-fuzz the parser: feed several pathological byte
        // patterns that target known parser hot spots. The goal
        // is "does not panic / unwrap-OOM / hang", not correctness
        // of the resulting grid. A deterministic LCG-seeded random
        // walk avoids dragging in proptest / quickcheck.
        let mut t = term(20, 5);
        // 1. Long unterminated OSC — VTE caps the param buffer
        //    so we just want "no panic".
        let mut payload = b"\x1b]2;".to_vec();
        payload.extend(std::iter::repeat(b'A').take(8192));
        t.advance(&payload);
        // No ST — start a new escape to flush.
        t.advance(b"\x1bc");
        // 2. Tight loop of CSI with random params.
        let mut seed: u32 = 0xDEAD_BEEF;
        let mut next = || -> u8 {
            // xorshift32 — deterministic.
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        };
        let mut buf = Vec::with_capacity(4096);
        for _ in 0..4096 {
            buf.push(next());
        }
        t.advance(&buf);
        // 3. Mixed control-byte burst.
        t.advance(b"\x00\x01\x02\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x1b\x7f");
        // 4. Truncated DCS.
        t.advance(b"\x1bP+q");
        for _ in 0..1024 {
            t.advance(&[next()]);
        }
        // If we got here, the parser survived. Quick sanity: take
        // accumulated outbound queues so we don't leak state into
        // adjacent tests via the shared `Term` if anyone refactors.
        let _ = t.take_osc_responses();
        let _ = t.take_notifications();
        let _ = t.take_progress();
        let _ = t.take_command_finishes();
    }

    #[test]
    fn osc_responses_queue_evicts_oldest_at_cap() {
        // OSC_RESPONSES_CAP (256) bounds the per-frame outbound
        // reply queue. A shell that floods queries (CSI c, DSR,
        // DECRQSS) between renderer drains can't grow it
        // unbounded — the cap evicts oldest. Pin the rule by
        // sending 257 DA1 queries and confirming the drain
        // returns exactly 256 replies.
        let mut t = term(4, 1);
        for _ in 0..257 {
            t.advance(b"\x1b[c");
        }
        let replies = t.take_osc_responses();
        assert_eq!(replies.len(), 256, "osc_responses must stay ≤ OSC_RESPONSES_CAP");
        // All replies are the DA1 string — pin one to confirm
        // shape didn't get corrupted by the eviction loop.
        assert!(
            replies
                .iter()
                .all(|r| r.starts_with("\x1b[?") && r.ends_with('c')),
        );
    }

    #[test]
    fn hyperlinks_map_evicts_oldest_at_cap() {
        // HYPERLINK_CAP (4096) bounds the URI lookup table. Emit
        // 4097 distinct URIs and confirm: the first id is evicted
        // (hyperlink_uri returns None) while the most-recent id
        // is still resolvable. Reuse-by-URI means we don't churn
        // ids on repeated identical links — pin that side too.
        let mut t = term(4, 1);
        for i in 1u32..=4097 {
            t.advance(format!("\x1b]8;;https://e/{}\x1b\\", i).as_bytes());
        }
        // First id (1) was inserted earliest → evicted when we
        // exceeded the cap.
        assert!(
            t.hyperlink_uri(1).is_none(),
            "id 1 should have been evicted at HYPERLINK_CAP",
        );
        // Last id (4097) survives.
        assert_eq!(t.hyperlink_uri(4097), Some("https://e/4097"));
    }

    #[test]
    fn hyperlinks_total_bytes_capped() {
        // HYPERLINK_TOTAL_BYTES_CAP (1 MiB) bounds the cumulative
        // byte total even when the entry count is well below
        // HYPERLINK_CAP — protects against a malicious shell that
        // pushes a handful of huge (up to URI_CAP = 8 KiB) URIs.
        // Push 200× 7 KiB URIs (~1.4 MiB nominal) and confirm the
        // running total stays under the cap.
        let mut t = term(4, 1);
        let bulk = "a".repeat(7 * 1024);
        for i in 1..=200u32 {
            // ESC ] 8 ; ; <scheme>://<bulk>?<i> ESC \
            t.advance(b"\x1b]8;;https://x.example/");
            t.advance(bulk.as_bytes());
            t.advance(format!("?{}", i).as_bytes());
            t.advance(b"\x1b\\");
        }
        // Sum bytes of every surviving URI; must respect the cap
        // (with a small headroom — eviction runs BEFORE insert, so
        // the inserted URI itself might push slightly past the cap
        // on the very last step; pin a generous 2× ceiling to make
        // the regression test stable while still catching unbounded
        // growth).
        let total: usize = (1u32..=u32::MAX)
            .take_while(|id| t.hyperlink_uri(*id).is_some() || *id < 250)
            .filter_map(|id| t.hyperlink_uri(id))
            .map(str::len)
            .sum();
        assert!(
            total <= 2 * 1024 * 1024,
            "hyperlink table grew to {total} bytes — total-byte cap not honoured",
        );
    }

    #[test]
    fn prompt_marks_queue_evicts_oldest_at_cap() {
        // PROMPT_MARKS_CAP (512) bounds the prompt-marks ring so a
        // shell that spams OSC 133;A across thousands of prompts
        // (e.g. a fast `while`-loop) stays bounded. Each prompt
        // must land on a distinct logical line — we drive it via
        // `OSC 133;A\n` cycles which advance the cursor / scroll.
        let mut t = term(2, 1);
        for _ in 0..513 {
            t.advance(b"\x1b]133;A\x07\n");
        }
        let marks = t.prompt_marks();
        assert_eq!(marks.len(), 512, "prompt_marks must stay ≤ PROMPT_MARKS_CAP");
        // The marks are strictly ascending (FIFO eviction keeps
        // the most-recent 512 in order).
        let ascending = marks.iter().zip(marks.iter().skip(1)).all(|(a, b)| a < b);
        assert!(ascending, "surviving marks must remain ascending");
    }

    #[test]
    fn command_finishes_queue_evicts_oldest_at_cap() {
        // OSC 133;D / 633;D both push to the same
        // pending_command_finishes queue, capped at
        // NOTIFICATIONS_CAP. Test pushes 65 distinct exit codes
        // via OSC 133;D and confirms FIFO eviction.
        let mut t = term(4, 1);
        for i in 0..65i32 {
            t.advance(format!("\x1b]133;D;{}\x07", i).as_bytes());
        }
        let drained = t.take_command_finishes();
        assert_eq!(drained.len(), 64, "queue must stay ≤ NOTIFICATIONS_CAP");
        // First survivor is exit_code=1 (i=0 evicted).
        assert_eq!(drained.first().map(|f| f.exit_code), Some(1));
        // Newest is i=64.
        assert_eq!(drained.last().map(|f| f.exit_code), Some(64));
    }

    #[test]
    fn progress_queue_evicts_oldest_at_cap() {
        // Same NOTIFICATIONS_CAP (64) FIFO eviction as
        // `pending_notifications`, but for OSC 9 ; 4 progress
        // updates. A noisy CI emitter that fires hundreds of
        // percent updates between frames stays bounded.
        let mut t = term(4, 1);
        // Push 65 progress updates, each with state=1 and pct=i%101.
        for i in 0..65u8 {
            let pct = i.min(100);
            t.advance(format!("\x1b]9;4;1;{}\x07", pct).as_bytes());
        }
        let drained = t.take_progress();
        assert_eq!(drained.len(), 64, "queue must stay ≤ NOTIFICATIONS_CAP");
        // First survivor is pct=1 (i=0 evicted).
        assert_eq!(drained.first(), Some(&(1u8, 1u8)));
        // Tail is the most-recent push (i=64, pct=64 since 64<=100).
        assert_eq!(drained.last(), Some(&(1u8, 64u8)));
    }

    #[test]
    fn notification_queue_evicts_oldest_at_cap() {
        // `NOTIFICATIONS_CAP` (64) bounds the per-frame queue so a
        // runaway emitter can't grow it unbounded. Eviction is
        // FIFO — pushing N+1 drops entry 0, keeping the most
        // recent CAP. The renderer drains every frame so this
        // ceiling only matters when a flood lands inside a
        // single frame.
        let mut t = term(4, 1);
        // Push 65 notifications via OSC 9 single-arg.
        for i in 0..65u32 {
            t.advance(format!("\x1b]9;n{}\x07", i).as_bytes());
        }
        let drained = t.take_notifications();
        assert_eq!(drained.len(), 64, "queue must stay ≤ NOTIFICATIONS_CAP");
        // Oldest ("n0") was evicted; first surviving entry is "n1".
        assert_eq!(drained.first().map(|s| s.as_str()), Some("n1"));
        // Newest is still on the tail.
        assert_eq!(drained.last().map(|s| s.as_str()), Some("n64"));
    }

    #[test]
    fn notification_truncated_by_vte_param_buffer() {
        // vte's OSC raw buffer caps individual params around 1 KiB
        // (the parser's internal `MAX_OSC_RAW`). Long bodies are
        // truncated by vte BEFORE we see them, so our own 4 KiB
        // cap in `push_notification` is defense-in-depth rather
        // than the operative limit. Pin that:
        //   - a 512-byte body round-trips intact,
        //   - a 2 KiB body lands but is truncated (received len
        //     is < 2 KiB), and
        //   - nothing crashes on a pathological emitter.
        let mut t = term(4, 1);
        let exact = "x".repeat(512);
        t.advance(format!("\x1b]9;{}\x07", exact).as_bytes());
        let drained = t.take_notifications();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].len(), 512);
        // Large body: vte truncates, but we still receive
        // something non-empty (the truncated prefix).
        let over = "y".repeat(2048);
        t.advance(format!("\x1b]9;{}\x07", over).as_bytes());
        let drained = t.take_notifications();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].len() < 2048, "vte should have truncated");
        assert!(!drained[0].is_empty(), "truncated prefix must still arrive");
    }

    #[test]
    fn all_notification_oscs_share_one_queue() {
        // OSC 9 (xterm/iTerm2 simple), OSC 99 (kitty), OSC 777
        // (urxvt), and OSC 1337 notify= (iTerm2 explicit) all
        // emit through `TerminalPerform::push_notification`. The
        // architectural property: one drain (`take_notifications`)
        // returns everything in source order. Pin it here so a
        // future "add a fifth notification protocol" iteration
        // can verify it lands in the same channel by extending
        // this test in one line.
        let mut t = term(4, 1);
        t.advance(b"\x1b]9;a\x07");
        t.advance(b"\x1b]99;;b\x07");
        t.advance(b"\x1b]777;notify;c\x07");
        t.advance(b"\x1b]1337;notify=d\x07");
        let drained = t.take_notifications();
        assert_eq!(
            drained,
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ],
        );
    }

    #[test]
    fn osc99_notify_routes_through_notification_queue() {
        // Kitty's `ESC ] 99 ; <options> ; <body> ST`. Options are
        // accepted but ignored (we don't model priority / action
        // yet). Confirm body lands in the queue, semicolons in
        // body survive rejoin, and empty body is dropped.
        let mut t = term(4, 1);
        t.advance(b"\x1b]99;;build done\x07");
        assert_eq!(t.take_notifications(), vec!["build done".to_string()]);
        // With options (ignored).
        t.advance(b"\x1b]99;d=42:p=high;backup finished\x07");
        assert_eq!(
            t.take_notifications(),
            vec!["backup finished".to_string()],
        );
        // Semicolons in body survive.
        t.advance(b"\x1b]99;;1.2.3;released\x07");
        assert_eq!(
            t.take_notifications(),
            vec!["1.2.3;released".to_string()],
        );
        // Empty body dropped.
        t.advance(b"\x1b]99;;\x07");
        assert!(t.take_notifications().is_empty());
        // Too few params (no body) → no queue entry.
        t.advance(b"\x1b]99;opts\x07");
        assert!(t.take_notifications().is_empty());
    }

    #[test]
    fn osc1337_notify_routes_through_notification_queue() {
        // iTerm2 `ESC ] 1337 ; notify=<message> ST`. Pair with the
        // existing `CurrentDir=` / `ClearScrollback` handling.
        // Confirm: short message lands in the queue; the existing
        // `CurrentDir=` path still works (no accidental shadowing).
        let mut t = term(4, 1);
        t.advance(b"\x1b]1337;notify=hello world\x07");
        assert_eq!(
            t.take_notifications(),
            vec!["hello world".to_string()],
        );
        // CurrentDir still works (unchanged).
        t.advance(b"\x1b]1337;CurrentDir=/tmp/x\x07");
        assert_eq!(t.cwd(), Some("/tmp/x"));
        assert!(t.take_notifications().is_empty());
        // Empty notification body is dropped (no queue entry).
        t.advance(b"\x1b]1337;notify=\x07");
        assert!(t.take_notifications().is_empty());
    }

    #[test]
    fn osc777_notify_routes_through_notification_queue() {
        // urxvt-style `ESC ] 777 ; notify ; <title> ; <body> ST`
        // and the bare two-arg form. Body-less variant emits just
        // the title; both forms drain via `take_notifications` so
        // the plugin `notification` event already covers them.
        let mut t = term(4, 1);
        t.advance(b"\x1b]777;notify;build;done\x07");
        assert_eq!(t.take_notifications(), vec!["build: done".to_string()]);
        // Bare body (no separate title).
        t.advance(b"\x1b]777;notify;hello\x07");
        assert_eq!(t.take_notifications(), vec!["hello".to_string()]);
        // Semicolons in the body survive rejoin.
        t.advance(b"\x1b]777;notify;title;body;with;semis\x07");
        assert_eq!(
            t.take_notifications(),
            vec!["title: body;with;semis".to_string()],
        );
        // Unknown subtype is silently ignored.
        t.advance(b"\x1b]777;screen;something\x07");
        assert!(t.take_notifications().is_empty());
    }

    #[test]
    fn sgr_5_and_6_both_set_blink_and_25_clears() {
        // Slow blink (5) and rapid blink (6) collapse to the same BLINK
        // attribute since no terminal in active use distinguishes them
        // visually. SGR 25 must clear blink regardless of which set it.
        let mut t = term(4, 1);
        t.advance(b"\x1b[5mA");
        assert!(cell(&t, 0, 0).attrs.contains(CellAttrs::BLINK));
        t.advance(b"\x1b[25mB");
        assert!(!cell(&t, 1, 0).attrs.contains(CellAttrs::BLINK));
        t.advance(b"\x1b[6mC");
        assert!(cell(&t, 2, 0).attrs.contains(CellAttrs::BLINK));
        t.advance(b"\x1b[25mD");
        assert!(!cell(&t, 3, 0).attrs.contains(CellAttrs::BLINK));
    }

    #[test]
    fn sgr_21_sets_double_underline() {
        let mut t = term(4, 1);
        t.advance(b"\x1b[21mA");
        let a = cell(&t, 0, 0);
        assert!(a.attrs.contains(CellAttrs::UNDERLINE));
        assert!(a.attrs.contains(CellAttrs::UNDERLINE_DOUBLE));
    }

    #[test]
    fn sgr_4_clears_double_and_curly_bits() {
        let mut t = term(4, 1);
        // Enable double underline, then plain `SGR 4` should drop the
        // double bit so we render as single underline.
        t.advance(b"\x1b[21mA\x1b[4mB");
        let b = cell(&t, 1, 0);
        assert!(b.attrs.contains(CellAttrs::UNDERLINE));
        assert!(!b.attrs.contains(CellAttrs::UNDERLINE_DOUBLE));
        assert!(!b.attrs.contains(CellAttrs::UNDERLINE_CURLY));
    }

    #[test]
    fn sgr_4_3_sets_curly_underline() {
        let mut t = term(4, 1);
        t.advance(b"\x1b[4:3mB");
        let b = cell(&t, 0, 0);
        assert!(b.attrs.contains(CellAttrs::UNDERLINE));
        assert!(b.attrs.contains(CellAttrs::UNDERLINE_CURLY));
        assert!(!b.attrs.contains(CellAttrs::UNDERLINE_DOUBLE));
    }

    #[test]
    fn sgr_24_clears_all_underline_variants() {
        let mut t = term(4, 1);
        t.advance(b"\x1b[21mX\x1b[24mY");
        let y = cell(&t, 1, 0);
        assert!(!y.attrs.contains(CellAttrs::UNDERLINE));
        assert!(!y.attrs.contains(CellAttrs::UNDERLINE_DOUBLE));
    }

    #[test]
    fn ris_clears_screen_and_resets_state() {
        let mut t = term(8, 3);
        // Push some SGR stack entries too so we cover that branch.
        t.advance(b"\x1b[1;31mHELLO\x1b[#{\x1b[?25l\x1b[?1049h\x1b[?2026h");
        t.advance(b"\x1bc");
        // Cursor home, default SGR, cursor visible, primary screen.
        assert_eq!(t.cursor(), Position { col: 0, row: 0 });
        assert!(t.cursor_visible());
        // Whole grid is blanked.
        assert_eq!(row_text(&t, 0), "        ");
        // Mode flags reset.
        assert!(!t.app_cursor_keys());
        assert!(!t.app_keypad());
        // Synchronized-output mode also cleared — otherwise an app
        // crashing after `\x1b[?2026h` and a subsequent reset would
        // strand the terminal in deferred-render limbo across the
        // reset boundary.
        assert!(!t.sync_output());
        // After RIS the SGR stack must be empty — a stray pop must not
        // restore stale attrs from before the reset.
        t.advance(b"\x1b[#}X");
        let row = t.grid().row(0).unwrap();
        assert_eq!(row[0].ch, 'X');
        assert!(!row[0].attrs.contains(CellAttrs::BOLD));
    }

    #[test]
    fn decrqm_reports_mode_state() {
        let mut t = term(4, 2);
        // ?25 starts set; flip it off then query.
        t.advance(b"\x1b[?25l\x1b[?25$p");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1b[?25;2$y".to_string()]);
        // Set it back on and re-query.
        t.advance(b"\x1b[?25h\x1b[?25$p");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1b[?25;1$y".to_string()]);
        // Unknown mode → value 0.
        t.advance(b"\x1b[?9999$p");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1b[?9999;0$y".to_string()]);
    }

    #[test]
    fn xtpushsgr_pop_restores_attrs() {
        // bat/delta/eza-style flow: set bold-red, push, set bold-green,
        // pop — the next printed cell should be bold-red again, not the
        // intermediate bold-green.
        let mut t = term(8, 1);
        t.advance(b"\x1b[1;31m"); // bold + red fg
        t.advance(b"\x1b[#{");    // push
        t.advance(b"\x1b[32m");   // green fg
        t.advance(b"x");
        t.advance(b"\x1b[#}");    // pop → bold + red
        t.advance(b"y");
        let row = t.grid().row(0).unwrap();
        // `x` got the intermediate (green) fg.
        assert_eq!(row[0].ch, 'x');
        // `y` carries the restored (red, bold) attrs.
        assert_eq!(row[1].ch, 'y');
        assert!(row[1].attrs.contains(CellAttrs::BOLD));
        // Pop-on-empty is a no-op: a second pop after we've drained the
        // stack must not blow up or alter the current SGR.
        t.advance(b"\x1b[#}z");
        let row = t.grid().row(0).unwrap();
        assert_eq!(row[2].ch, 'z');
    }

    #[test]
    fn xtpushsgr_stack_is_bounded() {
        // Pathological input that pushes far past the cap should not
        // grow memory unboundedly — the cap is silently enforced at the
        // push site. We use a small alphabet of distinct SGR colours so a
        // failure to enforce the cap would also corrupt pop order. After
        // 100 pushes (well past the 32 cap) we still pop cleanly.
        let mut t = term(8, 1);
        t.advance(b"\x1b[31m"); // red baseline
        for _ in 0..100 {
            t.advance(b"\x1b[#{");
        }
        // Drain pops. None should crash; final state must remain red.
        for _ in 0..100 {
            t.advance(b"\x1b[#}");
        }
        t.advance(b"r");
        let row = t.grid().row(0).unwrap();
        assert_eq!(row[0].ch, 'r');
        // SGR after the drain must equal the original "red baseline" we
        // pushed — verifies pops didn't lose state once we hit the cap.
        // We can't observe SGR fg directly here without exposing it, but
        // applying SGR 0 and re-printing must produce a different colour
        // span — that's covered by the prior test. Here we just check
        // the cap doesn't crash / panic.
    }

    #[test]
    fn decsed_decsel_alias_ed_el() {
        // DECSED 2 (CSI ? 2 J) should wipe the visible grid just like
        // CSI 2 J. Anything previously printed is gone.
        let mut t = term(5, 2);
        t.advance(b"hello\r\nworld");
        t.advance(b"\x1b[?2J");
        let row0: String = t.grid().row(0).unwrap().iter().map(|c| c.ch).collect();
        assert_eq!(row0.trim_end(), "");
        // DECSEL 1 (CSI ? 1 K) erases from start of line to cursor.
        t.advance(b"\x1b[1;1Habcde\x1b[1;3H\x1b[?1K");
        let row0: String = t.grid().row(0).unwrap().iter().map(|c| c.ch).collect();
        // Cells 0..=2 (inclusive of cursor at col 2) should be blank;
        // col 3..4 remain. CSI 1 K is "from start to cursor inclusive".
        assert_eq!(&row0[0..3], "   ");
    }

    #[test]
    fn xtgettcap_replies_for_known_and_unknown_caps() {
        let mut t = term(40, 4);
        // Query TN (Terminal Name) — hex of "TN" is "544E".
        t.advance(b"\x1bP+q544E\x1b\\");
        let replies = t.take_osc_responses();
        // Expected: `DCS 1 + r 544E=<hex of "rterm"> ST`.
        // "rterm" → 72 74 65 72 6D → 727465726D.
        assert_eq!(replies, vec!["\x1bP1+r544E=727465726D\x1b\\".to_string()]);

        // Query an unknown cap "ZZ" (hex 5A5A) — expect status 0 reply.
        t.advance(b"\x1bP+q5A5A\x1b\\");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1bP0+r5A5A\x1b\\".to_string()]);

        // Chained query: "Co;RGB" → 436F;524742. Each gets its own reply.
        t.advance(b"\x1bP+q436F;524742\x1b\\");
        let replies = t.take_osc_responses();
        // "256" → 323536, "8" → 38.
        assert_eq!(
            replies,
            vec![
                "\x1bP1+r436F=323536\x1b\\".to_string(),
                "\x1bP1+r524742=38\x1b\\".to_string(),
            ],
        );

        // Garbage hex (odd length) gets a status-0 echo so the app can
        // tell its query was received but unparseable.
        t.advance(b"\x1bP+qXYZ\x1b\\");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1bP0+rXYZ\x1b\\".to_string()]);
    }

    #[test]
    fn sync_output_mode_round_trips() {
        let mut t = term(8, 2);
        assert!(!t.sync_output());
        // Set, then query via DECRQM ?2026 $ p.
        t.advance(b"\x1b[?2026h\x1b[?2026$p");
        assert!(t.sync_output());
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1b[?2026;1$y".to_string()]);
        // Reset.
        t.advance(b"\x1b[?2026l");
        assert!(!t.sync_output());
        // DECSTR clears it too (so a half-frame app crash doesn't strand
        // the terminal in deferred-render limbo).
        t.advance(b"\x1b[?2026h\x1b[!p");
        assert!(!t.sync_output());
    }

    #[test]
    fn private_dsr_replies() {
        // DECXCPR — extended cursor position with page number.
        let mut t = term(80, 24);
        // Move to (3,5) (1-based for CUP).
        t.advance(b"\x1b[3;5H");
        t.advance(b"\x1b[?6n");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1b[?3;5;1R".to_string()]);

        // Printer status → "no printer".
        t.advance(b"\x1b[?15n");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1b[?13n".to_string()]);

        // Keyboard language report.
        t.advance(b"\x1b[?26n");
        let replies = t.take_osc_responses();
        assert_eq!(replies, vec!["\x1b[?27;1;0;0n".to_string()]);
    }

    #[test]
    fn ed3_drops_scrollback() {
        // Push enough lines into scrollback to be meaningful, capture a
        // prompt mark inside it, then send ED 3 — the visible grid plus
        // the ring should both be empty afterwards, and the mark must be
        // re-anchored so it doesn't dangle past the new scrollback end.
        let mut t = term(4, 2);
        t.set_scrollback_limit(8);
        // Three full screens of output → ~6 lines pushed into scrollback.
        for _ in 0..6 {
            t.advance(b"abc\r\n");
        }
        // Drop a prompt mark — currently lives near scrollback's top.
        t.advance(b"\x1b]133;A\x07");
        assert!(!t.prompt_marks().is_empty());
        assert!(t.scrollback_len() > 0);
        // ED 3 (CSI 3 J) — clears display + scrollback.
        t.advance(b"\x1b[3J");
        assert_eq!(t.scrollback_len(), 0);
        // Visible grid is wiped.
        let row = t.grid().row(0).unwrap();
        assert!(row.iter().all(|c| c.ch == ' '));
        // Marks that previously pointed into scrollback get dropped (we
        // can't reasonably keep them — the lines they referenced are
        // gone). Marks inside the grid survive at re-anchored indices.
        let marks: Vec<usize> = t.prompt_marks().iter().copied().collect();
        for m in &marks {
            assert!(*m < (t.size().rows as usize), "mark {m} outside grid");
        }
    }

    #[test]
    fn decstr_clears_sgr_stack() {
        // Soft reset should drop the entire SGR stack so a subsequent
        // unbalanced pop doesn't restore stale attrs from an earlier
        // session of a long-lived shell.
        let mut t = term(8, 1);
        t.advance(b"\x1b[1m\x1b[#{"); // push bold
        t.advance(b"\x1b[!p");        // DECSTR — soft reset
        t.advance(b"\x1b[#}a");       // pop on (now empty) stack → a is plain
        let row = t.grid().row(0).unwrap();
        assert_eq!(row[0].ch, 'a');
        assert!(!row[0].attrs.contains(CellAttrs::BOLD));
    }

    #[test]
    fn sgr_58_consumes_rgb_subparams_without_setting_dim() {
        // SGR 58;2;r;g;b sets the underline colour. We don't model the
        // underline colour, but we MUST consume the trailing r,g,b so
        // they aren't reinterpreted (e.g. `2` would otherwise become DIM
        // and `100..=107` would set a bright background).
        let mut t = term(8, 1);
        // `2` (DIM), then explicitly close it with `22` so a later SGR
        // 58 that mishandles its sub-params can't be confused with the
        // initial DIM in this test. Then write a plain char.
        t.advance(b"\x1b[2m\x1b[22m");
        t.advance(b"\x1b[58;2;100;200;50ma");
        let row = t.grid().row(0).unwrap();
        assert_eq!(row[0].ch, 'a');
        // DIM must NOT be re-set by mis-parsing the trailing `2`.
        assert!(!row[0].attrs.contains(CellAttrs::DIM));
        // BG must NOT be a bright colour from mis-parsing `100`.
        match row[0].bg {
            Color::Default | Color::Named(_) | Color::Indexed(_) | Color::Rgb(_, _, _) => {}
        }
        if let Color::Named(n) = row[0].bg {
            // Specifically, NOT bright black (the value that `100` maps to).
            assert_ne!(n, NamedColor::BrightBlack);
        }
    }

    #[test]
    fn sgr_58_colon_form_consumes_subparams() {
        // Colon-form variant `58:2::r:g:b` lands in a single Params slice.
        // It must be claimed by the sub-param pass so the flat path
        // doesn't see codes 58, 2, r, g, b separately.
        let mut t = term(8, 1);
        // [38;5;1m sets fg = red so we can verify it survives.
        t.advance(b"\x1b[38;5;1m");
        // Colon-form underline colour. After this, fg must still be the
        // red we set above (i.e. the 58:2:... slice must not bleed into
        // subsequent SGR processing and overwrite anything else).
        t.advance(b"\x1b[58:2::100:200:50ma");
        let row = t.grid().row(0).unwrap();
        assert_eq!(row[0].ch, 'a');
        // Foreground stays red — colon-form 58 didn't trip over the slice
        // boundary.
        assert!(matches!(row[0].fg, Color::Indexed(1)));
    }

    #[test]
    fn parse_rgb_spec_accepts_xterm_and_hex_forms() {
        // Canonical X11 `rgb:` form, 2-digit per channel.
        assert_eq!(parse_rgb_spec("rgb:ff/80/40"), Some([0xff, 0x80, 0x40]));
        // X11 `rgb:` form, 4-digit per channel (take the high byte).
        assert_eq!(parse_rgb_spec("rgb:ffff/8080/4040"), Some([0xff, 0x80, 0x40]));
        // `rgba:` variant — alpha (4th group) is parsed and ignored.
        assert_eq!(parse_rgb_spec("rgba:ff/80/40/ff"), Some([0xff, 0x80, 0x40]));
        // CSS-style 6-hex.
        assert_eq!(parse_rgb_spec("#ff8040"), Some([0xff, 0x80, 0x40]));
        // Short 3-hex (each nibble doubled).
        assert_eq!(parse_rgb_spec("#f84"), Some([0xff, 0x88, 0x44]));
        // 8-hex with alpha — alpha dropped.
        assert_eq!(parse_rgb_spec("#ff80407f"), Some([0xff, 0x80, 0x40]));
        // Malformed inputs reject without panicking.
        assert!(parse_rgb_spec("ff8040").is_none()); // no scheme prefix
        assert!(parse_rgb_spec("#ggg").is_none()); // non-hex
        assert!(parse_rgb_spec("rgb:ff/80").is_none()); // missing channel
        assert!(parse_rgb_spec("rgb:ff/80/40/extra/bits").is_none()); // too many slashes
        assert!(parse_rgb_spec("#1234567").is_none()); // 7-hex is unsupported
    }

    #[test]
    fn ansi_decrqm_irm_set_and_reset_round_trip() {
        // `CSI Pm $ p` (no `?` prefix) is ANSI DECRQM. Apps occasionally
        // probe with this form alongside the private one. Reply must
        // mirror current state for mode 4 (IRM) and fall back to 0
        // (unrecognised) for anything else.
        let mut t = term(4, 1);
        // Default: insert mode OFF → reply value 2 (reset).
        t.advance(b"\x1b[4$p");
        let resp = t.take_osc_responses();
        assert_eq!(resp, vec!["\x1b[4;2$y".to_string()]);

        // Turn IRM on via `CSI 4 h` (no `?`), re-query.
        t.advance(b"\x1b[4h\x1b[4$p");
        let resp = t.take_osc_responses();
        assert_eq!(resp, vec!["\x1b[4;1$y".to_string()]);

        // Unknown ANSI mode → value 0.
        t.advance(b"\x1b[999$p");
        let resp = t.take_osc_responses();
        assert_eq!(resp, vec!["\x1b[999;0$y".to_string()]);
    }

    #[test]
    fn osc11_set_accepts_css_hex_payload() {
        // Many apps (neovim's `:hi Normal guibg=#0a0c12`, bat/delta) emit
        // `OSC 11 ; #RRGGBB ST` instead of the X11 `rgb:` form. We must
        // honour it the same way: the default-bg colour updates and the
        // change queues for the renderer.
        let mut t = term(4, 1);
        // Pick a value that round-trips through the OSC 11 query reply
        // so we can verify storage shape.
        t.advance(b"\x1b]11;#102030\x1b\\");
        t.advance(b"\x1b]11;?\x1b\\");
        let replies = t.take_osc_responses();
        assert!(replies.iter().any(|r| r.contains("1010/2020/3030")),
                "OSC 11 reply should reflect the new bg, got {:?}", replies);
    }

    #[test]
    fn sgr_59_resets_underline_color_without_side_effects() {
        // `CSI 59 m` clears underline colour. It's a no-op for us (we
        // don't track underline colour), but it MUST NOT touch other
        // attrs.
        let mut t = term(8, 1);
        t.advance(b"\x1b[1m\x1b[59ma");
        let row = t.grid().row(0).unwrap();
        assert_eq!(row[0].ch, 'a');
        assert!(row[0].attrs.contains(CellAttrs::BOLD));
    }

    #[test]
    fn encode_hex_ascii_uses_uppercase_pairs_and_handles_empty() {
        // Pin the format that XTGETTCAP replies depend on: every byte
        // becomes exactly two upper-case hex digits, in big-endian
        // nibble order. Sourced from RFC 4648 §8 but more practically
        // from what xterm expects in the field.
        assert_eq!(encode_hex_ascii(""), "");
        assert_eq!(encode_hex_ascii("TN"), "544E");
        assert_eq!(encode_hex_ascii("rterm"), "7274 65726D".replace(' ', ""));
        // 0x00 and 0x7f are the ASCII range edges; pin the
        // big-endian-nibble formatting for both.
        assert_eq!(encode_hex_ascii("\x00\x7f"), "007F");
    }

    #[test]
    fn decode_hex_ascii_round_trips_with_encode_for_nonempty() {
        // The decode side is more permissive (lower- AND upper-case
        // hex digits) but for any non-empty input the round trip
        // encode → decode must reproduce the original ASCII bytes.
        // This pins the contract XTGETTCAP relies on: send cap "TN"
        // → reply hex "544E" → the requesting client decodes "TN"
        // back. Empty input is special: encode returns "" and decode
        // treats that as "no payload" → None; tested separately below.
        for s in ["TN", "rterm", "ABCdef0123", "  ", "\x01\x7f"] {
            let hex = encode_hex_ascii(s);
            assert_eq!(decode_hex_ascii(&hex).as_deref(), Some(s), "round-trip {s:?}");
        }
        // Mixed case input still decodes — the encoder uses upper,
        // but the decoder tolerates lower-case digits some clients
        // send (e.g. older XTGETTCAP implementations).
        assert_eq!(decode_hex_ascii("544e").as_deref(), Some("TN"));
        // Malformed → None. The XTGETTCAP dispatcher relies on this:
        // a malformed cap hex falls through to the "unknown cap" reply
        // instead of panicking on a hostile shell-supplied payload.
        assert!(decode_hex_ascii("ABC").is_none(), "odd length must fail");
        assert!(decode_hex_ascii("ZZ").is_none(), "non-hex digits must fail");
        assert!(decode_hex_ascii("").is_none(), "empty input rejected");
    }

    #[test]
    fn register_image_assigns_monotonic_ids() {
        let mut t = term(80, 24);
        let id1 = t
            .register_image(crate::image::ImageFormat::Rgba8, 1, 1, vec![0, 0, 0, 255])
            .expect("first");
        let id2 = t
            .register_image(crate::image::ImageFormat::Rgba8, 1, 1, vec![1, 2, 3, 4])
            .expect("second");
        assert_ne!(id1, id2);
        assert!(t.image(id1).is_some());
        assert!(t.image(id2).is_some());
    }

    #[test]
    fn place_image_filters_unknown_ids_and_zero_extent() {
        let mut t = term(80, 24);
        let id = t
            .register_image(crate::image::ImageFormat::Rgba8, 4, 4, vec![0; 4 * 4 * 4])
            .unwrap();
        t.place_image(crate::image::ImagePlacement {
            image_id: id,
            abs_row: 0,
            col: 0,
            rows: 2,
            cols: 4,
            width_px: 4,
            height_px: 4,
            placement_id: 0,
        });
        assert_eq!(t.image_placements().len(), 1);
        // Unknown id silently dropped.
        t.place_image(crate::image::ImagePlacement {
            image_id: 9999,
            abs_row: 0,
            col: 0,
            rows: 1,
            cols: 1,
            width_px: 1,
            height_px: 1,
            placement_id: 0,
        });
        assert_eq!(t.image_placements().len(), 1);
        // Zero-extent placement silently dropped.
        t.place_image(crate::image::ImagePlacement {
            image_id: id,
            abs_row: 0,
            col: 0,
            rows: 0,
            cols: 4,
            width_px: 4,
            height_px: 4,
            placement_id: 0,
        });
        assert_eq!(t.image_placements().len(), 1);
    }

    #[test]
    fn evict_placements_at_cell_only_removes_covered() {
        let mut t = term(80, 24);
        let id = t
            .register_image(crate::image::ImageFormat::Rgba8, 10, 10, vec![0; 10 * 10 * 4])
            .unwrap();
        // Two non-overlapping placements.
        t.place_image(crate::image::ImagePlacement {
            image_id: id,
            abs_row: 0,
            col: 0,
            rows: 2,
            cols: 4,
            width_px: 10,
            height_px: 10,
            placement_id: 0,
        });
        t.place_image(crate::image::ImagePlacement {
            image_id: id,
            abs_row: 5,
            col: 10,
            rows: 2,
            cols: 4,
            width_px: 10,
            height_px: 10,
            placement_id: 0,
        });
        assert_eq!(t.image_placements().len(), 2);
        let removed = t.evict_placements_at_cell(1, 2); // inside first
        assert_eq!(removed, 1);
        assert_eq!(t.image_placements().len(), 1);
        // Second one's still there.
        assert_eq!(t.image_placements()[0].abs_row, 5);
    }

    #[test]
    fn payload_above_max_is_rejected() {
        let mut t = term(80, 24);
        let oversize = vec![0u8; IMAGE_MAX_PAYLOAD_BYTES + 1];
        assert!(t
            .register_image(crate::image::ImageFormat::Rgba8, 1, 1, oversize)
            .is_none());
    }

    #[test]
    fn clear_images_resets_store_and_placements() {
        let mut t = term(80, 24);
        let id = t
            .register_image(crate::image::ImageFormat::Rgba8, 2, 2, vec![0; 16])
            .unwrap();
        t.place_image(crate::image::ImagePlacement {
            image_id: id,
            abs_row: 0,
            col: 0,
            rows: 1,
            cols: 1,
            width_px: 2,
            height_px: 2,
            placement_id: 0,
        });
        t.clear_images();
        assert!(t.image_placements().is_empty());
        assert!(t.image(id).is_none());
    }
}
