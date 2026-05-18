// Build-time asset pipeline for the window icon.
//
// Reads `assets/rterm.png`, downscales it to 256×256 RGBA, and dumps
// the raw pixel buffer to `$OUT_DIR/icon.rgba`. The runtime binary
// then `include_bytes!`'s that file and hands it to winit's
// `Icon::from_rgba`, so the PNG decoder isn't a runtime dependency
// (only a build dep — `image`).
//
// On Windows, additionally assembles a multi-resolution .ico file
// from the same source and embeds it as the .exe's icon resource via
// `winresource`. Without that step Explorer / taskbar / Alt-Tab keep
// showing the generic Rust gear instead of the rterm logo.

use std::env;
use std::fs;
use std::path::PathBuf;

const ICON_SIDE: u32 = 256;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let png_path = manifest_dir.join("assets").join("rterm.png");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", png_path.display());
    println!("cargo:rerun-if-changed=build.rs");

    let img = image::open(&png_path).unwrap_or_else(|e| {
        panic!("failed to open {}: {}", png_path.display(), e);
    });

    // Downscale to a single 256×256 RGBA buffer for the runtime
    // window icon. `Lanczos3` gives the cleanest result for a
    // photographic-style logo at small sizes.
    let icon = img
        .resize_to_fill(ICON_SIDE, ICON_SIDE, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    fs::write(out_dir.join("icon.rgba"), icon.as_raw())
        .expect("write icon.rgba");

    #[cfg(windows)]
    {
        build_windows_resources(&img, &out_dir);
    }
    #[cfg(not(windows))]
    {
        let _ = &img; // silence "unused" outside windows
    }
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
