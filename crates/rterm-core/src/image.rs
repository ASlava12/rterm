//! Inline-image data model.
//!
//! Storage layer for the bitmap content embedded by terminal apps via
//! the **iTerm2 `OSC 1337 ;File=`** and **Kitty graphics (`APC G`)**
//! protocols. The parser layer (in `terminal.rs`) decodes the
//! protocol-level bytes and calls into [`Terminal::register_image`] /
//! [`Terminal::place_image`]; the renderer pulls the placements every
//! frame via [`Terminal::image_placements`] and decodes / uploads to
//! the GPU lazily on first draw.
//!
//! Keeping the image bytes in their on-the-wire form (PNG, JPEG, raw
//! RGBA) means rterm-core stays free of an `image`-crate dep — the
//! decoder lives behind a feature gate in `rterm-render` where the
//! result lands directly in a `wgpu::Texture` without an
//! intermediate Vec<u8> round-trip.

/// On-the-wire encoding of [`Image::data`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// Pre-decoded RGBA8 — `width * height * 4` bytes of `[R, G, B, A]`.
    /// Used by Kitty `f=32` (raw RGBA) and as the cache result after
    /// the renderer has done a one-time PNG/JPEG decode.
    Rgba8,
    /// Pre-decoded RGB8 — `width * height * 3` bytes. Kitty `f=24`.
    Rgb8,
    /// PNG bytes (magic `89 50 4E 47 ...`). Used by both iTerm2 and
    /// Kitty `f=100`. Decoded on first GPU upload.
    Png,
    /// JPEG bytes (magic `FF D8 ...`). iTerm2 only.
    Jpeg,
    /// GIF bytes (magic `47 49 46 38 ...`). iTerm2 only.
    /// Treated as a still image (first frame); animation support is
    /// future work.
    Gif,
}

/// One inline image. Stored on [`Terminal`] keyed by [`Image::id`]
/// and referenced by zero-or-more [`ImagePlacement`]s — that
/// separation lets the same source bytes be displayed at multiple
/// positions (Kitty's "virtual placement" feature) without
/// re-uploading.
#[derive(Debug, Clone)]
pub struct Image {
    pub id: u64,
    pub format: ImageFormat,
    pub width_px: u32,
    pub height_px: u32,
    /// Raw bytes in the encoding `format` advertises. Owned by the
    /// terminal until [`Terminal::evict_image`] removes both the
    /// entry and any placements that point at it.
    pub data: Vec<u8>,
}

/// A single rendering of an [`Image`] anchored to a logical-row /
/// column position in the (scrollback ++ grid) stream. `abs_row` is
/// the absolute row index — same convention selection uses
/// (see `rterm-render`'s `AbsPoint`), so placements survive
/// wheel-scroll without per-frame re-anchoring.
///
/// The cell footprint (`rows × cols`) tells the renderer how many
/// terminal cells the image visually occupies. Sub-cell pixel size
/// is preserved via `width_px / height_px` so the renderer can
/// choose between "scale to cell rect" and "centre at native
/// resolution" rendering modes.
#[derive(Debug, Clone, Copy)]
pub struct ImagePlacement {
    pub image_id: u64,
    /// Top-left absolute logical row of the image's cell footprint.
    pub abs_row: i64,
    /// Top-left column.
    pub col: u16,
    /// Cell-grid footprint. `rows == 0` would be invalid; callers
    /// (the protocol parsers) compute this from the natural pixel
    /// dimensions ÷ line height.
    pub rows: u16,
    pub cols: u16,
    /// Source pixel dimensions, repeated here so the renderer can
    /// pick a scale mode without looking up the parent [`Image`]
    /// (cleaner read pattern from the render path's `&Terminal`
    /// borrow).
    pub width_px: u32,
    pub height_px: u32,
    /// Optional placement id from the Kitty `p=` parameter. Lets
    /// the shell update or delete an image in place by re-using the
    /// same `(image_id, placement_id)` pair. `0` = unscoped (iTerm2,
    /// and Kitty placements that didn't set `p=`).
    pub placement_id: u32,
}

impl ImagePlacement {
    /// `true` when the cell `(abs_row, col)` falls inside this
    /// image's rect. Used by the cell-write path to invalidate
    /// placements whose footprint just got partially overwritten by
    /// text (default behaviour — matches xterm / iTerm2; Kitty's
    /// "image-as-layer" semantics are out of scope for the first
    /// implementation).
    pub fn covers(&self, abs_row: i64, col: u16) -> bool {
        if self.rows == 0 || self.cols == 0 {
            return false;
        }
        let row_end = self.abs_row + self.rows as i64;
        let col_end = self.col.saturating_add(self.cols);
        abs_row >= self.abs_row
            && abs_row < row_end
            && col >= self.col
            && col < col_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_covers_inside_rect_and_rejects_outside() {
        let p = ImagePlacement {
            image_id: 1,
            abs_row: 10,
            col: 4,
            rows: 3,
            cols: 5,
            width_px: 100,
            height_px: 80,
            placement_id: 0,
        };
        // Inside.
        assert!(p.covers(10, 4));
        assert!(p.covers(12, 8));
        // On row-end / col-end boundary (exclusive).
        assert!(!p.covers(13, 4));
        assert!(!p.covers(10, 9));
        // Outside.
        assert!(!p.covers(9, 4));
        assert!(!p.covers(10, 3));
    }

    #[test]
    fn placement_with_zero_extent_never_covers() {
        let p = ImagePlacement {
            image_id: 1,
            abs_row: 0,
            col: 0,
            rows: 0,
            cols: 5,
            width_px: 10,
            height_px: 10,
            placement_id: 0,
        };
        assert!(!p.covers(0, 0));
    }
}
