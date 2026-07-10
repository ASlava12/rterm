//! Sixel graphics decoder — turns a DCS Sixel body (`DCS P1;P2;P3 q
//! <data> ST`, minus the DCS envelope) into an RGBA8 image the renderer
//! can register through the normal inline-image store.
//!
//! Sixel packs six vertical pixels per data byte (`?`..`~`, value =
//! `byte - 0x3F`, bit 0 = top). Colour registers are selected with
//! `#Pc` and defined with `#Pc;Pu;Px;Py;Pz` (Pu=2 RGB 0–100%, Pu=1 HLS).
//! `$` returns to the left margin on the same six-row band, `-` starts
//! the next band, `!Pn` repeats the next sixel Pn times, and `"…` sets
//! raster attributes (declared width/height). Everything else (control
//! bytes, stray whitespace) is skipped, so a `cat` of Sixel garbage
//! decodes to whatever pixels it implies rather than crashing.
//!
//! This module is pure: no terminal state, no I/O. It exists so the
//! decode can be unit-tested against known byte strings independently of
//! the DCS plumbing that will feed it.

/// A decoded Sixel image: tightly-packed RGBA8, `width * height * 4`
/// bytes, top-to-bottom row-major (matches the renderer's texture
/// upload). Transparent pixels are `[0, 0, 0, 0]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SixelImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Hard caps: refuse a Sixel that would exceed these so a crafted body
/// can't pin unbounded RAM. 4096² is comfortably past any real terminal
/// image and well under the decoder's 8192² texture ceiling.
const MAX_DIM: usize = 4096;
const MAX_PIXELS: usize = 4096 * 4096;

/// Decode a Sixel data body into an RGBA image. `None` when the body is
/// empty, paints nothing, or would exceed the size caps. Never panics on
/// malformed input.
pub fn decode(data: &[u8]) -> Option<SixelImage> {
    let mut palette = default_palette();
    let mut color: usize = 0;

    // `grid[row][col]` — grown on demand. `None` = transparent.
    let mut grid: Vec<Vec<Option<[u8; 3]>>> = Vec::new();
    let mut x: usize = 0; // current column
    let mut band: usize = 0; // current six-row band (top row = band * 6)
    let mut max_x: usize = 0; // widest column touched (exclusive)
    let mut max_band: Option<usize> = None; // highest band any pixel landed in
    let mut declared: Option<(usize, usize)> = None; // raster (w, h)

    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'#' => {
                i += 1;
                let (pc, ni) = parse_uint(data, i);
                i = ni;
                if i < data.len() && data[i] == b';' {
                    // Colour definition: `;Pu;Px;Py;Pz`.
                    i += 1;
                    let (pu, ni) = parse_uint(data, i);
                    i = ni;
                    let (px, ni) = read_semi_param(data, i);
                    i = ni;
                    let (py, ni) = read_semi_param(data, i);
                    i = ni;
                    let (pz, ni) = read_semi_param(data, i);
                    i = ni;
                    let rgb = if pu == 1 {
                        hls_to_rgb(px, py, pz)
                    } else {
                        // Pu == 2 (RGB) — and treat anything else as RGB
                        // too, clamping the 0–100% channels.
                        [pct_to_255(px), pct_to_255(py), pct_to_255(pz)]
                    };
                    palette[(pc as usize) & 0xff] = rgb;
                }
                color = (pc as usize) & 0xff;
            }
            b'!' => {
                // Repeat: `!Pn` then one sixel char.
                i += 1;
                let (n, ni) = parse_uint(data, i);
                i = ni;
                if i < data.len() {
                    let sc = data[i];
                    i += 1;
                    if (0x3F..=0x7E).contains(&sc) {
                        let bits = sc - 0x3F;
                        // Clamp the run to the remaining width budget so a
                        // huge `!Pn` can't grow the grid without bound
                        // before the per-iteration guard below runs — the
                        // repeat loop would otherwise be a memory bomb.
                        let remaining = (MAX_DIM + 1).saturating_sub(x) as u32;
                        let count = n.max(1).min(remaining);
                        for _ in 0..count {
                            put_sixel(&mut grid, x, band, bits, palette[color]);
                            x += 1;
                        }
                        max_x = max_x.max(x);
                        if bits != 0 && count > 0 {
                            max_band = Some(max_band.map_or(band, |m| m.max(band)));
                        }
                    }
                }
            }
            b'"' => {
                // Raster attributes: `"Pan;Pad;Ph;Pv`.
                i += 1;
                let (_pan, ni) = parse_uint(data, i);
                i = ni;
                let (_pad, ni) = read_semi_param(data, i);
                i = ni;
                let (ph, ni) = read_semi_param(data, i);
                i = ni;
                let (pv, ni) = read_semi_param(data, i);
                i = ni;
                if ph > 0 && pv > 0 {
                    declared = Some((ph as usize, pv as usize));
                }
            }
            b'$' => {
                x = 0;
                i += 1;
            }
            b'-' => {
                x = 0;
                band += 1;
                i += 1;
            }
            sc @ 0x3F..=0x7E => {
                let bits = sc - 0x3F;
                put_sixel(&mut grid, x, band, bits, palette[color]);
                x += 1;
                max_x = max_x.max(x);
                if bits != 0 {
                    max_band = Some(max_band.map_or(band, |m| m.max(band)));
                }
                i += 1;
            }
            // Whitespace / control / unknown — ignore (robust to junk).
            _ => i += 1,
        }
        // Bounds guard: bail before allocating past the caps.
        if x > MAX_DIM || band.saturating_mul(6) > MAX_DIM {
            return None;
        }
    }

    // Each painted band is a full six rows tall, even if only the top
    // pixel of the last band is set — so height rounds up to the band.
    let content_h = max_band.map_or(0, |b| (b + 1) * 6);
    let content_w = max_x;
    // Prefer the declared raster size when present (apps like img2sixel
    // emit it); otherwise use the painted extent.
    let (width, height) = declared.unwrap_or((content_w, content_h));
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return None;
    }
    if width.saturating_mul(height) > MAX_PIXELS {
        return None;
    }

    let mut rgba = vec![0u8; width * height * 4];
    for (row, cols) in grid.iter().enumerate() {
        if row >= height {
            break;
        }
        for (col, px) in cols.iter().enumerate() {
            if col >= width {
                break;
            }
            if let Some([r, g, b]) = *px {
                let o = (row * width + col) * 4;
                rgba[o] = r;
                rgba[o + 1] = g;
                rgba[o + 2] = b;
                rgba[o + 3] = 0xFF;
            }
        }
    }
    Some(SixelImage { width: width as u32, height: height as u32, rgba })
}

/// Paint one sixel char (`bits`, low 6 bits) at column `x` in six-row
/// `band`, growing the grid to fit. Bit `k` sets row `band*6 + k`.
fn put_sixel(grid: &mut Vec<Vec<Option<[u8; 3]>>>, x: usize, band: usize, bits: u8, rgb: [u8; 3]) {
    let base = band * 6;
    for k in 0..6 {
        if bits & (1 << k) == 0 {
            continue;
        }
        let row = base + k;
        if row >= grid.len() {
            grid.resize(row + 1, Vec::new());
        }
        let cols = &mut grid[row];
        if x >= cols.len() {
            cols.resize(x + 1, None);
        }
        cols[x] = Some(rgb);
    }
}

/// Parse a run of ASCII digits starting at `i`, returning `(value, next
/// index)`. `(0, i)` when there's no digit.
fn parse_uint(data: &[u8], mut i: usize) -> (u32, usize) {
    let mut v: u32 = 0;
    while i < data.len() && data[i].is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add((data[i] - b'0') as u32);
        i += 1;
    }
    (v, i)
}

/// Read an optional leading `;` then a uint — the shape of each trailing
/// colour / raster parameter. `(0, i)` when the `;` (or number) is absent.
fn read_semi_param(data: &[u8], mut i: usize) -> (u32, usize) {
    if i < data.len() && data[i] == b';' {
        i += 1;
        parse_uint(data, i)
    } else {
        (0, i)
    }
}

/// Sixel colour percentage (0–100) → 8-bit channel.
fn pct_to_255(p: u32) -> u8 {
    ((p.min(100) * 255 + 50) / 100) as u8
}

/// HLS → RGB per the Sixel spec: H 0–360 (with 0 = blue, offset by 120°
/// from the usual convention), L 0–100, S 0–100.
fn hls_to_rgb(h: u32, l: u32, s: u32) -> [u8; 3] {
    let h = (h % 360) as f32;
    let l = (l.min(100) as f32) / 100.0;
    let s = (s.min(100) as f32) / 100.0;
    // Sixel's hue is shifted: 0° is blue. Rotate so the maths below use
    // the standard "0° = red" wheel.
    let h = (h + 120.0) % 360.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let xc = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, xc, 0.0),
        1 => (xc, c, 0.0),
        2 => (0.0, c, xc),
        3 => (0.0, xc, c),
        4 => (xc, 0.0, c),
        _ => (c, 0.0, xc),
    };
    let m = l - c / 2.0;
    [
        (((r1 + m) * 255.0).round().clamp(0.0, 255.0)) as u8,
        (((g1 + m) * 255.0).round().clamp(0.0, 255.0)) as u8,
        (((b1 + m) * 255.0).round().clamp(0.0, 255.0)) as u8,
    ]
}

/// VT340 default 16-colour palette (registers 0–15); the rest start
/// black and are typically redefined before use. Values are the standard
/// Sixel percentages converted to 8-bit.
fn default_palette() -> [[u8; 3]; 256] {
    // (r, g, b) in 0–100% as the VT340 defines them.
    const BASE: [(u32, u32, u32); 16] = [
        (0, 0, 0),
        (20, 20, 80),
        (80, 13, 13),
        (20, 80, 20),
        (80, 20, 80),
        (20, 80, 80),
        (80, 80, 20),
        (53, 53, 53),
        (26, 26, 26),
        (33, 33, 60),
        (60, 26, 26),
        (33, 60, 33),
        (60, 33, 60),
        (33, 60, 60),
        (60, 60, 33),
        (80, 80, 80),
    ];
    let mut pal = [[0u8; 3]; 256];
    let mut i = 0;
    while i < 16 {
        let (r, g, b) = BASE[i];
        pal[i] = [pct_to_255(r), pct_to_255(g), pct_to_255(b)];
        i += 1;
    }
    pal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_at(img: &SixelImage, x: u32, y: u32) -> [u8; 4] {
        let o = ((y * img.width + x) * 4) as usize;
        [img.rgba[o], img.rgba[o + 1], img.rgba[o + 2], img.rgba[o + 3]]
    }

    #[test]
    fn single_full_column_is_six_pixels_tall() {
        // `#1` selects register 1, `~` (0x7E) = value 63 = all six bits.
        // One column, six rows.
        let img = decode(b"#1~").expect("decodes");
        assert_eq!((img.width, img.height), (1, 6));
        for y in 0..6 {
            assert_eq!(rgb_at(&img, 0, y)[3], 0xFF, "row {y} painted");
        }
    }

    #[test]
    fn top_pixel_only_and_transparency() {
        // `@` (0x40) = value 1 = only bit 0 (top row) set. The remaining
        // five rows stay transparent.
        let img = decode(b"#2@").expect("decodes");
        assert_eq!(img.height, 6);
        assert_eq!(rgb_at(&img, 0, 0)[3], 0xFF, "top row opaque");
        for y in 1..6 {
            assert_eq!(rgb_at(&img, 0, y)[3], 0x00, "row {y} transparent");
        }
    }

    #[test]
    fn rgb_color_definition_and_repeat() {
        // Define register 3 as pure red (RGB 100;0;0), then repeat the
        // full-height sixel 4 times → a 4×6 red block.
        let img = decode(b"#3;2;100;0;0!4~").expect("decodes");
        assert_eq!((img.width, img.height), (4, 6));
        for x in 0..4 {
            let px = rgb_at(&img, x, 0);
            assert_eq!(px, [255, 0, 0, 255], "column {x} red");
        }
    }

    #[test]
    fn newline_and_carriage_return_move_bands() {
        // Column A on band 0, `-` to band 1, column B → 1 wide, 12 tall.
        let img = decode(b"#1~-#1~").expect("decodes");
        assert_eq!((img.width, img.height), (1, 12));
        assert_eq!(rgb_at(&img, 0, 0)[3], 0xFF);
        assert_eq!(rgb_at(&img, 0, 11)[3], 0xFF);
    }

    #[test]
    fn raster_attributes_set_dimensions() {
        // `"1;1;10;8` declares a 10×8 canvas even though we only paint a
        // 1×6 column.
        let img = decode(b"\"1;1;10;8#1~").expect("decodes");
        assert_eq!((img.width, img.height), (10, 8));
    }

    #[test]
    fn fuzz_decode_never_panics_and_stays_bounded() {
        // Deterministic LCG (no external rng, reproducible). Feed many
        // random byte strings — biased toward Sixel-significant chars so
        // the control paths get exercised — and assert the decoder never
        // panics, never exceeds the caps, and returns a correctly-sized
        // buffer whenever it produces an image.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let alphabet: &[u8] = b"#!$-\"0123456789;?@ABYZ~\x00\xff ";
        for _ in 0..3000 {
            let len = (next() % 600) as usize;
            let data: Vec<u8> = (0..len)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            if let Some(img) = decode(&data) {
                assert!(img.width as usize <= MAX_DIM);
                assert!(img.height as usize <= MAX_DIM);
                assert_eq!(
                    img.rgba.len(),
                    img.width as usize * img.height as usize * 4,
                    "rgba length matches declared dimensions"
                );
            }
        }
        // A pathological run-length must not hang or OOM — it's clamped to
        // the width budget, then rejected for exceeding it.
        assert!(decode(b"!4294967295~").is_none());
        assert!(decode(b"#999999999999;2;100;0;0~").is_some());
    }

    #[test]
    fn junk_and_empty_are_safe() {
        // Pure control/whitespace paints nothing → None, no panic.
        assert!(decode(b"").is_none());
        assert!(decode(b"\x1b\x07 \r\n").is_none());
        // A stray `cat` of a binary file shouldn't panic; it may or may
        // not paint pixels, but must return without crashing.
        let _ = decode(&[0u8, 255, 128, 0x7E, 0x3F, b'-', b'$', b'!', 200]);
    }
}
