//! Window-icon bytes generated at build time.
//!
//! `build.rs` decodes `assets/rterm.png`, downscales it to 256×256
//! RGBA, and writes the raw pixel buffer to `$OUT_DIR/icon.rgba`.
//! Including those bytes here keeps the runtime binary free of a
//! PNG decoder while still shipping a single source-of-truth icon.

const ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon.rgba"));
const ICON_SIDE: u32 = 256;

/// `AppIcon` ready to hand to `rterm_render::RunConfig::icon`. The
/// vector is cloned per call so callers can stash it in a config
/// struct without worrying about lifetimes; the underlying buffer
/// is small (256×256×4 = 256 KiB).
pub fn app_icon() -> rterm_render::AppIcon {
    rterm_render::AppIcon {
        rgba: ICON_RGBA.to_vec(),
        width: ICON_SIDE,
        height: ICON_SIDE,
    }
}
