//! Renders the launcher runtime backdrop into the macOS DMG banner PNG.
//!
//!     cargo run -p gba-launcher --bin gba-dmg-bg
//!
//! Optional arg: output path (default: the repo's assets/macos_dmg_banner.png).

use std::path::PathBuf;

fn main() {
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/macos_dmg_banner.png")
        });
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("create asset dir");
    }
    let rgba = gba_launcher::assets::render_dmg_background_rgba(1600, 1000);
    let png = gba_launcher::assets::encode_png_rgba(&rgba, 1600, 1000);
    std::fs::write(&out, png).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
    println!("wrote {}", out.display());
}
