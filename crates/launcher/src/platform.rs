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

/// Locate the `gamedb.sqlite` (function boundaries by ROM sha256):
/// $GBA_RECOMP_GAMEDB, then next to this executable (the portable release
/// layout), then the macOS app bundle Resources dir, then, for local `cargo`
/// builds, the repo root's `gamedb.sqlite`. `None` = none found, in which
/// case the runtime builds from label files instead.
#[cfg(not(target_os = "android"))]
fn gamedb_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("GBA_RECOMP_GAMEDB") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let beside = exe.with_file_name("gamedb.sqlite");
    if beside.is_file() {
        return Some(beside);
    }
    if let Some(resources) = bundle_resource_gamedb_candidate(&exe).filter(|p| p.is_file()) {
        return Some(resources);
    }
    // Local dev: <workspace>/gamedb.sqlite.
    dev_gamedb_candidate(&exe).filter(|p| p.is_file())
}

/// macOS app bundle layout:
/// `<app>.app/Contents/MacOS/gba-launcher` loads metadata from
/// `<app>.app/Contents/Resources/gamedb.sqlite`.
#[cfg(not(target_os = "android"))]
fn bundle_resource_gamedb_candidate(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    Some(contents.join("Resources").join("gamedb.sqlite"))
}

/// The dev gamedb candidate for an executable built under a cargo `target/`
/// directory: `<workspace>/gamedb.sqlite`. `None` if `exe` is not under a
/// `target/` (e.g. a shipped binary), so this never fires in production.
#[cfg(not(target_os = "android"))]
fn dev_gamedb_candidate(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "target"))
        .and_then(Path::parent)
        .map(|root| root.join("gamedb.sqlite"))
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn dev_gamedb_candidate_resolves_workspace_root() {
        let want = Some(PathBuf::from("/home/u/gba-recomp/gamedb.sqlite"));
        // Both debug and release layouts resolve to the same workspace root.
        assert_eq!(
            dev_gamedb_candidate(Path::new("/home/u/gba-recomp/target/release/gba-launcher")),
            want
        );
        assert_eq!(
            dev_gamedb_candidate(Path::new("/home/u/gba-recomp/target/debug/gba-launcher")),
            want
        );
        // A shipped binary (no `target/` component) yields no dev candidate.
        assert_eq!(
            dev_gamedb_candidate(Path::new("/opt/frogger-temple/gba-launcher")),
            None
        );
    }

    #[test]
    fn bundle_resource_gamedb_candidate_resolves_macos_app() {
        assert_eq!(
            bundle_resource_gamedb_candidate(Path::new(
                "/Applications/gba-recomp.app/Contents/MacOS/gba-launcher"
            )),
            Some(PathBuf::from(
                "/Applications/gba-recomp.app/Contents/Resources/gamedb.sqlite"
            ))
        );
        assert_eq!(
            bundle_resource_gamedb_candidate(Path::new("/opt/gba-recomp/gba-launcher")),
            None
        );
    }
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
    // Hand the runtime the gamedb so a first-launch build seeds from its
    // mapper-grade boundaries (then --ram profiling captures RAM-resident
    // code) — the 0-interpreter path. Absent, the runtime falls back to
    // label files beside the image.
    if let Some(db) = gamedb_path() {
        cmd.arg("--gamedb").arg(db);
    }
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
