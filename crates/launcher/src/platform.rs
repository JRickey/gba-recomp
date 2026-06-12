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

/// Open the system file picker for a BIOS image (first-launch setup).
#[cfg(not(target_os = "android"))]
pub fn pick_bios() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Select BIOS image (16 KB)")
        .add_filter("BIOS image", &["bin", "rom"])
        .add_filter("All files", &["*"])
        .pick_file()
}

/// Per-user config/state directory (created on first use).
pub fn config_dir() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("gba-recomp");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Suppress the console window any child process would otherwise flash
/// on Windows (CREATE_NO_WINDOW). No-op elsewhere.
fn no_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Probe `recomp --version` (prints "recomp <semver>"). `None` = unknown:
/// binary missing, the flag predates this handshake, or odd output.
pub fn recomp_version() -> Option<String> {
    let bin = recomp_bin().ok()?;
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    no_console(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let ver = text.lines().next()?.trim().strip_prefix("recomp ")?.trim();
    (!ver.is_empty()).then(|| ver.to_string())
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
            let sib = dir.join(if cfg!(windows) {
                "recomp.exe"
            } else {
                "recomp"
            });
            if sib.is_file() {
                return Ok(sib);
            }
        }
    }
    Ok(PathBuf::from("recomp")) // resolved via PATH at spawn time
}

/// Launch a cartridge in the play runtime. The caller owns the child:
/// the launcher tracks it and tears it down when the launcher exits.
/// stdout carries the `--status` lifecycle protocol (building/playing)
/// and stderr the runtime's diagnostics; both are piped so the launcher
/// can reflect the session's real state instead of guessing.
#[cfg(not(target_os = "android"))]
pub fn launch(rom: &Path, bios: Option<&Path>) -> Result<std::process::Child, String> {
    let bin = recomp_bin()?;
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("play").arg("--status");
    // Pass the resolved BIOS explicitly: play would find it again via the
    // same shared lookup, but the launcher validated *this* file — the
    // session must boot what the user was told it would boot.
    if let Some(b) = bios {
        cmd.arg("--bios").arg(b);
    }
    cmd.arg(rom)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    no_console(&mut cmd);
    cmd.spawn()
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

#[cfg(target_os = "android")]
pub fn pick_bios() -> Option<PathBuf> {
    None
}

/// Android: no play runtime is built for this target yet.
#[cfg(target_os = "android")]
pub fn launch(_rom: &Path, _bios: Option<&Path>) -> Result<std::process::Child, String> {
    Err("the play runtime is not available on this platform yet".into())
}
