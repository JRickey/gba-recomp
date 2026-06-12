//! Packaged-binary manifest: `recomp.pack.toml` next to the executable.
//!
//! A packaged recomp is this same binary shipped beside a manifest and
//! a prebuilt translation. The manifest pins the cartridge and BIOS by
//! SHA-256 — the package carries no image bytes, and play refuses to
//! start until the user supplies files hashing to the pins. With
//! `interpreter = false` the package is a *full recomp*: a dispatch
//! miss is a defect that halts loudly, never a silent interpreter.
//!
//!     [package]
//!     name = "my-recomp"
//!     [image]
//!     rom-sha256 = "<64 hex>"
//!     bios-sha256 = "<64 hex>"
//!     [runtime]
//!     interpreter = false
//!     [translation]
//!     file = "translation.dylib"

use std::path::{Path, PathBuf};

pub struct Manifest {
    pub name: String,
    pub rom_sha256: String,
    pub bios_sha256: String,
    pub interpreter: bool,
    /// Whether the press-Escape in-game menu is available. Default true;
    /// a package may pin it off, in which case Escape quits as it did
    /// before the menu existed and the binary is configured by its
    /// config file only.
    pub menu: bool,
    /// Translation library file name, relative to the manifest.
    pub translation: String,
    /// Engine-HLE pin from packaging: `None` = auto-detect, `"off"` =
    /// never arm, or an engine name (`m4a`/`gax`/`rdrv`) the packager
    /// verified — detection of other engines is skipped at runtime.
    pub engine_hle: Option<String>,
    /// The directory the manifest was loaded from.
    pub dir: PathBuf,
}

/// The directory holding the running executable (canonicalized), the
/// anchor for the portable package layout.
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Load the manifest beside the executable, if present. A present but
/// malformed manifest is a hard error — a packaged binary must never
/// silently fall back to developer-tool behavior.
pub fn load() -> Option<Result<Manifest, String>> {
    let dir = exe_dir()?;
    let path = dir.join("recomp.pack.toml");
    if !path.is_file() {
        return None;
    }
    Some(parse(&path, dir))
}

fn parse(path: &Path, dir: PathBuf) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let doc: toml::Value = text
        .parse()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let s = |keys: [&str; 2]| -> Result<String, String> {
        doc.get(keys[0])
            .and_then(|t| t.get(keys[1]))
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("{}: missing {}.{}", path.display(), keys[0], keys[1]))
    };
    let rom_sha256 = s(["image", "rom-sha256"])?.to_lowercase();
    let bios_sha256 = s(["image", "bios-sha256"])?.to_lowercase();
    for sha in [&rom_sha256, &bios_sha256] {
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{}: malformed sha pin", path.display()));
        }
    }
    Ok(Manifest {
        name: s(["package", "name"]).unwrap_or_else(|_| "recomp".into()),
        rom_sha256,
        bios_sha256,
        interpreter: doc
            .get("runtime")
            .and_then(|t| t.get("interpreter"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true),
        menu: doc
            .get("runtime")
            .and_then(|t| t.get("menu"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true),
        translation: s(["translation", "file"])?,
        engine_hle: doc
            .get("runtime")
            .and_then(|t| t.get("engine-hle"))
            .and_then(toml::Value::as_str)
            .filter(|v| !v.eq_ignore_ascii_case("auto"))
            .map(str::to_lowercase),
        dir,
    })
}

/// Locate the user's cartridge image: any `*.gba` beside the package
/// (then in the shared config dir) hashing to the pin.
pub fn find_rom(m: &Manifest) -> Result<PathBuf, String> {
    let mut dirs = vec![m.dir.clone()];
    if let Some(c) = dirs::config_dir() {
        dirs.push(c.join("gba-recomp"));
    }
    for d in &dirs {
        let Ok(entries) = std::fs::read_dir(d) else {
            continue;
        };
        let mut names: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "gba"))
            .collect();
        names.sort();
        for p in names {
            if let Ok(data) = std::fs::read(&p) {
                use sha2::{Digest, Sha256};
                let sha: String = Sha256::digest(&data)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                if sha == m.rom_sha256 {
                    return Ok(p);
                }
            }
        }
    }
    Err(format!(
        "this package plays exactly one cartridge image (sha256 {}…) and none was \
found.\nPlace your own dump (.gba) next to the executable:\n  {}",
        &m.rom_sha256[..16],
        m.dir.display()
    ))
}
