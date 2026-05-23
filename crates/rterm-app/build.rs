// Build-time asset pipeline for the window icon.
//
// Reads `assets/rterm.png`, trims its transparent / solid-colour
// borders so the visible logo fills the icon edge-to-edge (the
// source PNG ships with ~20% padding on every side, which made the
// taskbar icon visibly smaller than every neighbouring app), then
// downscales to 256×256 RGBA and dumps the raw pixel buffer to
// `$OUT_DIR/icon.rgba`. The runtime binary `include_bytes!`'s that
// file and hands it to winit's `Icon::from_rgba`, so the PNG
// decoder isn't a runtime dependency (only a build dep — `image`).
//
// On Windows, additionally assembles a multi-resolution .ico file
// from the same trimmed source and embeds it as the .exe's icon
// resource via `winresource`. Without that step Explorer / taskbar
// / Alt-Tab keep showing the generic Rust gear instead of the
// rterm logo.

use std::env;
use std::fs;
use std::path::PathBuf;

const ICON_SIDE: u32 = 256;
/// Breathing margin around the trimmed bbox, in PER-MILLE of the
/// shorter trimmed side. `30/1000` = 3% padding. The icon-host
/// shells (Explorer / taskbar / Alt-Tab on Windows; the macOS
/// Dock; GNOME / KDE app launchers) all add their own padding
/// around the icon image, so we don't need much — the goal is
/// just to avoid having ink touch the icon's hard pixel edge.
const PAD_PER_MILLE: u32 = 30;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let png_path = manifest_dir.join("assets").join("rterm.png");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", png_path.display());
    println!("cargo:rerun-if-changed=build.rs");

    let img = image::open(&png_path).unwrap_or_else(|e| {
        panic!("failed to open {}: {}", png_path.display(), e);
    });
    let trimmed = trim_borders(&img);

    // Downscale to a single 256×256 RGBA buffer for the runtime
    // window icon. `Lanczos3` gives the cleanest result for a
    // photographic-style logo at small sizes.
    let icon = trimmed
        .resize_to_fill(ICON_SIDE, ICON_SIDE, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    fs::write(out_dir.join("icon.rgba"), icon.as_raw())
        .expect("write icon.rgba");

    #[cfg(windows)]
    {
        build_windows_resources(&trimmed, &out_dir);
    }
    #[cfg(not(windows))]
    {
        let _ = &trimmed; // silence "unused" outside windows
    }
}

/// Detect the bounding box of "non-background" pixels and crop the
/// image to it (plus a small breathing margin). When the source has
/// alpha variation we trust the alpha channel; otherwise we treat
/// the top-left corner pixel as the background colour and trim
/// everything that's perceptually close to it.
///
/// Falls back to the untouched image when the bbox detection finds
/// nothing usable (e.g. fully-transparent input, or solid-colour).
fn trim_borders(img: &image::DynamicImage) -> image::DynamicImage {
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    if w == 0 || h == 0 {
        return img.clone();
    }
    // Decide which strategy to use by scanning min alpha.
    let alpha_min = rgba.pixels().map(|p| p[3]).min().unwrap_or(255);
    let has_alpha = alpha_min < 240;
    // Background-matching predicate. Keeps the iterator pure on
    // the hot path — closure dispatch is cheap relative to the
    // pixel walk that dominates a 1.5M-pixel scan.
    let bg = *rgba.get_pixel(0, 0);
    let is_bg = |p: image::Rgba<u8>| -> bool {
        if has_alpha {
            p[3] < 16
        } else {
            // Sum of per-channel abs differences. < 24 catches
            // anti-aliased fringe of a solid border without
            // eating into the logo body.
            let d = (p[0] as i32 - bg[0] as i32).abs()
                + (p[1] as i32 - bg[1] as i32).abs()
                + (p[2] as i32 - bg[2] as i32).abs();
            d < 24
        }
    };
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            if !is_bg(*rgba.get_pixel(x, y)) {
                any = true;
                if x < min_x {
                    min_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if x > max_x {
                    max_x = x;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }
    }
    if !any || min_x > max_x || min_y > max_y {
        return img.clone();
    }
    let trimmed_w = max_x - min_x + 1;
    let trimmed_h = max_y - min_y + 1;
    let pad = (trimmed_w.min(trimmed_h) * PAD_PER_MILLE / 1000).max(4);
    let crop_x = min_x.saturating_sub(pad);
    let crop_y = min_y.saturating_sub(pad);
    let crop_w = (trimmed_w + 2 * pad).min(w - crop_x);
    let crop_h = (trimmed_h + 2 * pad).min(h - crop_y);
    let cropped = image::imageops::crop_imm(&rgba, crop_x, crop_y, crop_w, crop_h)
        .to_image();
    image::DynamicImage::ImageRgba8(cropped)
}

#[cfg(windows)]
fn build_windows_resources(src: &image::DynamicImage, out_dir: &std::path::Path) {
    // Resolutions Windows actually uses across Explorer thumbnails,
    // taskbar, Alt-Tab, and Start tile. Skipping any of them tends
    // to show as either a pixelated upscale or the generic icon
    // depending on the shell context.
    const SIZES: &[u32] = &[16, 24, 32, 48, 64, 128, 256];
    let mut icondir = ico::IconDir::new(ico::ResourceType::Icon);
    for &side in SIZES {
        let frame = src
            .resize_to_fill(side, side, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        let img = ico::IconImage::from_rgba_data(side, side, frame.into_raw());
        let entry = ico::IconDirEntry::encode(&img).expect("encode .ico entry");
        icondir.add_entry(entry);
    }
    let ico_path = out_dir.join("rterm.ico");
    let f = std::fs::File::create(&ico_path).expect("create rterm.ico");
    icondir.write(f).expect("write rterm.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_string_lossy().as_ref());
    if let Err(e) = res.compile() {
        // Don't fail the whole build on a resource-compile glitch
        // (the .exe still runs, just without the embedded icon).
        // Warn so a maintainer notices.
        println!("cargo:warning=winresource compile failed: {e}");
    }
}
