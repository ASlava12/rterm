//! Lazy CPU-side decoder for inline-image payloads.
//!
//! The terminal's image store ([`rterm_core::Image`]) keeps payloads
//! in their on-the-wire form (PNG / JPEG / GIF bytes for iTerm2 and
//! Kitty `f=100`; raw RGBA8 / RGB8 for Kitty `f=32` / `f=24`). The
//! GPU image pass calls into this module the first time it needs to
//! upload a given image id to a `wgpu::Texture`, and the result is
//! cached so subsequent frames just rebind.
//!
//! Failures (truncated header, unsupported variant, decoder OOM)
//! degrade to `None` — the caller renders a placeholder rect
//! rather than panicking. Real-world ingestion of malformed image
//! bytes is non-zero (network glitches, half-finished writes), and
//! one bad frame must not be allowed to take the whole renderer
//! down.
//!
use rterm_core::{Image, ImageFormat};

/// Pre-decoded RGBA8 buffer ready for `Queue::write_texture`. Always
/// `width * height * 4` bytes long with row-major top-to-bottom
/// ordering — matches wgpu's default texture layout when stride is
/// implied.
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Animation frames for an animated GIF. Empty for a still image.
    /// When non-empty it has ≥ 2 entries and `frames[0].rgba == rgba`
    /// (the first frame is duplicated into `rgba` so the still upload
    /// path stays untouched). Each frame is already fully composited
    /// (the `image` crate applies GIF disposal), so the renderer just
    /// swaps the whole texture per frame.
    pub frames: Vec<AnimFrame>,
}

/// One frame of an animated image: an RGBA8 buffer plus the delay to
/// hold it before advancing to the next frame.
pub struct AnimFrame {
    pub rgba: Vec<u8>,
    pub delay_ms: u32,
}

/// Cap on decoded animation frames and their cumulative RGBA bytes — a
/// crafted GIF could otherwise pin unbounded RAM (thousands of frames).
const GIF_MAX_FRAMES: usize = 256;
const GIF_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Floor for per-frame delay. GIFs frequently encode 0 (or a few ms)
/// meaning "as fast as possible"; browsers clamp such frames, and a
/// literal 0 would busy-spin the render loop. 0 → 100 ms (the de-facto
/// browser default), anything else is floored at 20 ms.
const GIF_MIN_DELAY_MS: u32 = 20;

/// Decode `image` into a CPU-side RGBA8 buffer. `None` when the
/// payload is unsupported / corrupt / too large to allocate. The
/// caller's image cache should remember that result so we don't
/// retry every frame on a bad input.
pub fn decode(image: &Image) -> Option<DecodedImage> {
    match image.format {
        ImageFormat::Rgba8 => {
            // Validate the payload size matches the declared
            // dimensions. A mismatch is almost always a protocol
            // bug on the sender side; rather than feed wgpu a
            // mis-sized buffer (which causes a frame-level
            // validation error), refuse the upload here.
            let expected = (image.width_px as usize)
                .checked_mul(image.height_px as usize)?
                .checked_mul(4)?;
            if image.data.len() != expected {
                tracing::debug!(
                    expected,
                    actual = image.data.len(),
                    "RGBA8 payload length mismatches declared dimensions",
                );
                return None;
            }
            Some(DecodedImage {
                width: image.width_px,
                height: image.height_px,
                rgba: image.data.clone(),
                frames: Vec::new(),
            })
        }
        ImageFormat::Rgb8 => {
            let expected = (image.width_px as usize)
                .checked_mul(image.height_px as usize)?
                .checked_mul(3)?;
            if image.data.len() != expected {
                tracing::debug!(
                    expected,
                    actual = image.data.len(),
                    "RGB8 payload length mismatches declared dimensions",
                );
                return None;
            }
            // Expand to RGBA by inserting an opaque alpha byte
            // after every triplet. Same row order, just wider per
            // pixel. Single allocation up-front to avoid the
            // grow-from-zero churn of repeated `push`es.
            let pixels = (image.width_px as usize) * (image.height_px as usize);
            let mut rgba = Vec::with_capacity(pixels * 4);
            for triplet in image.data.chunks_exact(3) {
                rgba.push(triplet[0]);
                rgba.push(triplet[1]);
                rgba.push(triplet[2]);
                rgba.push(0xFF);
            }
            Some(DecodedImage {
                width: image.width_px,
                height: image.height_px,
                rgba,
                frames: Vec::new(),
            })
        }
        ImageFormat::Gif => {
            // Try the animated path first. On success with ≥ 2 frames we
            // return them for playback; a single-frame GIF (or a decode
            // hiccup) falls through to the still `ImageReader` path below.
            if let Some((width, height, frames)) = decode_gif_animation(&image.data) {
                if frames.len() > 1 {
                    return Some(DecodedImage {
                        width,
                        height,
                        rgba: frames[0].rgba.clone(),
                        frames,
                    });
                }
            }
            decode_still(image)
        }
        ImageFormat::Png | ImageFormat::Jpeg => decode_still(image),
    }
}

/// Decode all frames of an animated GIF into RGBA8 buffers with delays.
/// `None` on a decode error or an empty/oversize animation. Bounded by
/// `GIF_MAX_FRAMES` / `GIF_MAX_TOTAL_BYTES` and the 8192² dimension cap.
fn decode_gif_animation(data: &[u8]) -> Option<(u32, u32, Vec<AnimFrame>)> {
    use image::AnimationDecoder;
    const MAX_DECODE_DIM: u32 = 8192;
    const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
    let mut decoder =
        image::codecs::gif::GifDecoder::new(std::io::Cursor::new(data)).ok()?;
    let (w, h) = image::ImageDecoder::dimensions(&decoder);
    if w == 0 || h == 0 || w > MAX_DECODE_DIM || h > MAX_DECODE_DIM {
        return None;
    }
    // Bound the PER-FRAME allocation, mirroring `decode_still`. A GIF frame
    // carries its own Image-Descriptor width/height (u16, up to 65535²)
    // that is independent of the logical screen checked above — without
    // limits, a tiny-canvas GIF declaring a 65535×65535 frame makes the
    // iterator alloc ~16 GiB (OOM-abort, or a 32-bit overflow panic) before
    // any rterm cap runs. With limits set, `reserve_buffer` rejects the
    // frame → `Err` → the `let Ok(frame) else break` below bails cleanly.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIM);
    limits.max_image_height = Some(MAX_DECODE_DIM);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    image::ImageDecoder::set_limits(&mut decoder, limits).ok()?;
    let mut frames = Vec::new();
    let mut total_bytes: u64 = 0;
    for frame in decoder.into_frames() {
        let Ok(frame) = frame else { break };
        let (num, den) = frame.delay().numer_denom_ms();
        let raw = num / den.max(1);
        let delay_ms = if raw == 0 { 100 } else { raw.max(GIF_MIN_DELAY_MS) };
        let rgba = frame.into_buffer().into_raw();
        total_bytes = total_bytes.saturating_add(rgba.len() as u64);
        if total_bytes > GIF_MAX_TOTAL_BYTES {
            break;
        }
        frames.push(AnimFrame { rgba, delay_ms });
        if frames.len() >= GIF_MAX_FRAMES {
            break;
        }
    }
    if frames.is_empty() {
        None
    } else {
        Some((w, h, frames))
    }
}

/// Decode a single still image (PNG / JPEG / single-frame GIF) via
/// `ImageReader` with decompression-bomb limits.
fn decode_still(image: &Image) -> Option<DecodedImage> {
    {
            // Decode through `ImageReader` with explicit limits — a
            // decompression bomb (a few-KB PNG declaring 50000×50000)
            // would otherwise make the decoder allocate w*h*4 bytes
            // on the CPU before we ever see the dimensions. 8192² at
            // RGBA8 is exactly the 256 MiB alloc ceiling, and larger
            // textures wouldn't fit common GPU limits anyway (the
            // upload path re-checks the actual device maximum).
            const MAX_DECODE_DIM: u32 = 8192;
            const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(MAX_DECODE_DIM);
            limits.max_image_height = Some(MAX_DECODE_DIM);
            limits.max_alloc = Some(MAX_DECODE_ALLOC);
            // Format auto-detected from the magic bytes, so no
            // dispatch on `image.format` is needed. The redundant
            // tag is still useful for the renderer-side cache.
            let mut reader = match image::ImageReader::new(std::io::Cursor::new(
                image.data.as_slice(),
            ))
            .with_guessed_format()
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        format = ?image.format,
                        data_len = image.data.len(),
                        "image format sniff failed: {e}",
                    );
                    return None;
                }
            };
            reader.limits(limits);
            let dyn_img = match reader.decode() {
                Ok(d) => d,
                Err(e) => {
                    // WARN, not debug: default log levels filter out
                    // `debug`, so a real decode failure would be
                    // invisible to a user trying to figure out why
                    // their inline image isn't drawing.
                    tracing::warn!(
                        format = ?image.format,
                        data_len = image.data.len(),
                        "image decode failed: {e}",
                    );
                    return None;
                }
            };
            let rgba_img = dyn_img.to_rgba8();
            let width = rgba_img.width();
            let height = rgba_img.height();
            Some(DecodedImage {
                width,
                height,
                rgba: rgba_img.into_raw(),
                frames: Vec::new(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(format: ImageFormat, w: u32, h: u32, data: Vec<u8>) -> Image {
        Image { id: 1, format, width_px: w, height_px: h, data }
    }

    #[test]
    fn gif_animation_decodes_multiple_frames_with_delays() {
        use image::{codecs::gif::GifEncoder, Delay, Frame, RgbaImage};
        // Encode a 2×2, 2-frame GIF in memory (delays are multiples of
        // 10 ms so they survive GIF's centisecond quantisation exactly).
        let mut buf = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut buf);
            let red = RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
            let blue = RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 255, 255]));
            enc.encode_frame(Frame::from_parts(red, 0, 0, Delay::from_numer_denom_ms(80, 1)))
                .unwrap();
            enc.encode_frame(Frame::from_parts(blue, 0, 0, Delay::from_numer_denom_ms(120, 1)))
                .unwrap();
        }
        let decoded = decode(&img(ImageFormat::Gif, 2, 2, buf)).expect("gif decodes");
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.height, 2);
        assert_eq!(decoded.frames.len(), 2, "both frames decoded");
        assert_eq!(decoded.frames[0].delay_ms, 80);
        assert_eq!(decoded.frames[1].delay_ms, 120);
        // First frame is mirrored into `rgba` so the still upload path
        // needs no special case; each frame is a full 2*2*4 RGBA buffer.
        assert_eq!(decoded.rgba, decoded.frames[0].rgba);
        assert_eq!(decoded.frames[0].rgba.len(), 2 * 2 * 4);
    }

    #[test]
    fn rgba8_passthrough_when_dimensions_match() {
        let data = vec![0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF];
        let d = decode(&img(ImageFormat::Rgba8, 2, 1, data.clone())).expect("decode");
        assert_eq!(d.width, 2);
        assert_eq!(d.height, 1);
        assert_eq!(d.rgba, data);
    }

    #[test]
    fn rgba8_rejects_size_mismatch() {
        // 2x1 RGBA = 8 bytes; pass 7 to trip the validator.
        let data = vec![0u8; 7];
        assert!(decode(&img(ImageFormat::Rgba8, 2, 1, data)).is_none());
    }

    #[test]
    fn rgb8_expands_to_rgba_with_opaque_alpha() {
        // 1×1 RGB = (red, 255, 0).
        let d = decode(&img(ImageFormat::Rgb8, 1, 1, vec![0xFF, 0xFF, 0x00]))
            .expect("decode");
        assert_eq!(d.rgba, vec![0xFF, 0xFF, 0x00, 0xFF]);
    }

    #[test]
    fn rgb8_rejects_size_mismatch() {
        // 2×1 RGB = 6 bytes; pass 5.
        assert!(decode(&img(ImageFormat::Rgb8, 2, 1, vec![0u8; 5])).is_none());
    }

    #[test]
    fn png_decoder_rejects_garbage() {
        // Random bytes that aren't a real PNG.
        let data = vec![0x89, b'P', b'N', b'G', 0x00, 0x00, 0x00, 0x00];
        assert!(decode(&img(ImageFormat::Png, 1, 1, data)).is_none());
    }

    /// CRC-32 (IEEE, reflected, poly 0xEDB88320) — just enough to
    /// hand-craft a PNG chunk the decoder will accept as well-formed.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    #[test]
    fn png_bomb_dimensions_are_rejected_by_decode_limits() {
        // A few dozen bytes declaring a 50000×50000 image. Without
        // decoder limits, `load_from_memory` would try to allocate
        // ~10 GB for the pixel buffer before failing — a remote
        // shell can print this. The limits must reject it at header
        // parse, cheaply.
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&50_000u32.to_be_bytes()); // width
        ihdr.extend_from_slice(&50_000u32.to_be_bytes()); // height
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace
        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        assert!(decode(&img(ImageFormat::Png, 50_000, 50_000, png)).is_none());
    }

    #[test]
    fn small_valid_png_still_decodes_through_the_limited_reader() {
        // Positive control for the limits change: an ordinary PNG
        // passes through `ImageReader` + `Limits` unharmed.
        let mut buf = Vec::new();
        let px = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        px.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("encode test png");
        let d = decode(&img(ImageFormat::Png, 2, 2, buf)).expect("valid PNG decodes");
        assert_eq!((d.width, d.height), (2, 2));
        assert_eq!(d.rgba.len(), 2 * 2 * 4);
        assert_eq!(&d.rgba[..4], &[10, 20, 30, 255]);
    }
}
