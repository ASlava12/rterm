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
}

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
            })
        }
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif => {
            // `image::load_from_memory` auto-detects the format
            // from the magic bytes, so we don't need to dispatch
            // by `image.format`. The redundant tag is still
            // useful for the renderer-side cache (e.g. to gate
            // on enabled formats), but here we just decode.
            let dyn_img = match image::load_from_memory(&image.data) {
                Ok(d) => d,
                Err(e) => {
                    tracing::debug!("image decode failed: {e}");
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
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(format: ImageFormat, w: u32, h: u32, data: Vec<u8>) -> Image {
        Image { id: 1, format, width_px: w, height_px: h, data }
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
}
