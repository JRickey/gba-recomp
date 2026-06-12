//! Renders the canonical app-icon master PNG procedurally and writes it
//! to `assets/icon/app-icon.png`. The committed PNG is a build artifact of
//! the renderer in `gba_launcher::assets` — regenerate it with:
//!
//!     cargo run -p gba-launcher --bin gba-icon
//!
//! Optional arg: output path (default: the repo's assets/icon/app-icon.png).
//! Optional second arg: size in pixels (default 1024).

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .map(PathBuf::from)
        // CARGO_MANIFEST_DIR is crates/launcher; the master lives two up.
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/icon/app-icon.png")
        });
    let size: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1024);

    let rgba = gba_launcher::assets::render_icon_rgba(size);
    let png = gba_launcher::assets::encode_png_rgba(&rgba, size as u32, size as u32);

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("create assets/icon");
    }
    std::fs::write(&out, &png).expect("write master PNG");
    println!(
        "wrote {} ({}×{}, {} bytes)",
        out.display(),
        size,
        size,
        png.len()
    );
}
