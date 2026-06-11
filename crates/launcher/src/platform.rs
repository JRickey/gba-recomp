//! Platform integration: native file selection, config storage, launch.
//!
//! Desktop (Linux/macOS/Windows) uses the system file dialog via `rfd`
//! (XDG portal / NSOpenPanel / IFileOpenDialog). The Android path uses the
//! system document picker and lives behind `cfg(target_os = "android")`.

use std::path::{Path, PathBuf};

/// Open the system file picker for a cartridge image.
#[cfg(not(target_os = "android"))]
pub fn pick_rom() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Select cartridge image")
        .add_filter("GBA cartridge image", &["gba", "agb"])
        .add_filter("All files", &["*"])
        .pick_file()
}

/// Per-user config/state directory (created on first use).
pub fn config_dir() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("gba-recomp");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Locate the `recomp` binary: $GBA_RECOMP_BIN, next to this executable,
/// then $PATH.
fn recomp_bin() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("GBA_RECOMP_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("GBA_RECOMP_BIN points at nothing: {}", p.display()));
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let sib = dir.join(if cfg!(windows) { "recomp.exe" } else { "recomp" });
            if sib.is_file() {
                return Ok(sib);
            }
        }
    }
    Ok(PathBuf::from("recomp")) // resolved via PATH at spawn time
}

/// Launch a cartridge in the play runtime. Returns the child pid.
#[cfg(not(target_os = "android"))]
pub fn launch(rom: &Path) -> Result<u32, String> {
    let bin = recomp_bin()?;
    std::process::Command::new(&bin)
        .arg("play")
        .arg(rom)
        .spawn()
        .map(|c| c.id())
        .map_err(|e| format!("failed to start {}: {e}", bin.display()))
}

/// Android: the sanctioned interface is the system document picker (SAF,
/// ACTION_OPEN_DOCUMENT) — scoped storage forbids raw filesystem browsing.
/// Delivering the picked URI back to Rust needs onActivityResult, which
/// the plain NativeActivity glue does not forward; the plan (see
/// docs/launcher.md) is a small Java activity subclass that launches the
/// picker and hands the URI across JNI. Until that shim lands, selection
/// is unavailable on this target.
#[cfg(target_os = "android")]
pub fn pick_rom() -> Option<PathBuf> {
    None
}

/// Android: no play runtime is built for this target yet.
#[cfg(target_os = "android")]
pub fn launch(_rom: &Path) -> Result<u32, String> {
    Err("the play runtime is not available on this platform yet".into())
}
