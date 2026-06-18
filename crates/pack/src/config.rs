//! `pack.toml` — the packager's input. One file describes one
//! distributable: which image (by SHA only, never by content), which
//! runtime modules to bundle, which platforms to target, and what to
//! emit. See docs/packaging.md for the full design.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PackConfig {
    pub package: Package,
    pub image: Image,
    #[serde(default)]
    pub labels: Labels,
    #[serde(default)]
    pub runtime: Runtime,
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    pub output: Output,
}

/// Build-time toolchain settings — how the translation is compiled on the
/// *packager's* machine. Nothing here is shipped in the package.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Build {
    /// C compiler to translate with, overriding the recompiler's default
    /// (a system `cc`/`clang`/`gcc` if present, else the bundled TinyCC).
    /// Any `cc`-style program name or absolute path — `clang`, `gcc`, a
    /// wrapper script. Set this to compile a shipped package with a real
    /// optimizing compiler instead of the TinyCC fallback. Forwarded to
    /// `recomp` as `$GBA_RECOMP_CC`; works on all platforms.
    pub compiler: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Package {
    /// Distributable name (binary stem). Keep it free of commercial
    /// title names if you intend to publish — the packager neither
    /// checks nor cares, but your lawyers might.
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Target platforms; the trio is the default and the promise.
    #[serde(default = "default_platforms")]
    pub platforms: Vec<Platform>,
    /// Optional executable icon: a square PNG master, ideally 1024×1024
    /// (no smaller than 256×256 — every platform format is derived from
    /// it). Relative paths resolve against this config file's directory,
    /// like `labels.file`. When absent the canonical launcher icon is
    /// used so the package still reads as part of the product family.
    #[serde(default)]
    pub icon: Option<PathBuf>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Macos,
    Windows,
    Linux,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Image {
    /// SHA-256 of the cartridge image this package recompiles. The
    /// produced binary refuses to run with anything else: the image
    /// is the asset store, supplied by the end user, verified by hash.
    pub rom_sha256: String,
    /// SHA-256 of the BIOS image the package boots with. Same deal:
    /// end-user supplied, hash-gated, never distributed.
    pub bios_sha256: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Labels {
    /// The function map (gba-labels v1 or v2). For a packaged release
    /// this should be a mapper-grade full map; the packager prints
    /// coverage loudly either way.
    pub file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Runtime {
    /// Bundle the press-Escape in-game menu (audio/video/input
    /// settings, ROM/BIOS selection). Without it the binary is
    /// configured by its config file only.
    pub menu: bool,
    /// Ship the enhanced audio path (per-channel sinc, soft-clip,
    /// engine HLE shadow). The end-user toggle lives in the menu.
    pub enhanced_audio: bool,
    /// Ship the panel-simulation video path (crates/screen).
    pub screen_sim: bool,
    /// Sound-engine HLE selection. `auto` = runtime discovery as in
    /// `play`; pinning skips discovery for a known title.
    pub engine_hle: EngineHle,
    /// `false` = full recomp: the packaged binary ships with no
    /// interpreter fallback — a dispatch miss halts loudly. The build
    /// gates this on a zero-fallback soak.
    #[serde(default = "default_true")]
    pub interpreter: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime {
            menu: true,
            enhanced_audio: true,
            screen_sim: true,
            engine_hle: EngineHle::Auto,
            interpreter: true,
        }
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EngineHle {
    #[default]
    Auto,
    Off,
    M4a,
    Gax,
    Rdrv,
}

impl EngineHle {
    /// The manifest spelling — must match the pack.toml kebab-case values.
    pub fn as_str(self) -> &'static str {
        match self {
            EngineHle::Auto => "auto",
            EngineHle::Off => "off",
            EngineHle::M4a => "m4a",
            EngineHle::Gax => "gax",
            EngineHle::Rdrv => "rdrv",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Output {
    /// The platform-native game binary.
    pub binary: bool,
    /// Also emit the recompiled C source tree, with function labels
    /// applied: named where the label set carries names, `sub_<addr>`
    /// elsewhere. The tree is derived from the image — it is for the
    /// packager's own use, not for distribution alongside a "no
    /// proprietary data" claim.
    pub c_source: bool,
}

impl Default for Output {
    fn default() -> Self {
        Output {
            binary: true,
            c_source: false,
        }
    }
}

fn default_version() -> String {
    "0.1.0".into()
}

fn default_platforms() -> Vec<Platform> {
    vec![Platform::Macos, Platform::Windows, Platform::Linux]
}

fn valid_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl PackConfig {
    /// Parse and validate. Path-relative fields resolve against the
    /// config file's directory.
    pub fn load(path: &Path) -> Result<PackConfig, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut cfg: PackConfig =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if cfg.package.name.is_empty() {
            return Err("package.name must not be empty".into());
        }
        if !valid_sha(&cfg.image.rom_sha256) {
            return Err("image.rom-sha256 must be 64 hex digits".into());
        }
        if !valid_sha(&cfg.image.bios_sha256) {
            return Err("image.bios-sha256 must be 64 hex digits".into());
        }
        cfg.image.rom_sha256.make_ascii_lowercase();
        cfg.image.bios_sha256.make_ascii_lowercase();
        if cfg.package.platforms.is_empty() {
            cfg.package.platforms = default_platforms();
        }
        if !cfg.output.binary && !cfg.output.c_source {
            return Err("output: nothing to emit (binary and c-source both off)".into());
        }
        if let (Some(f), Some(dir)) = (cfg.labels.file.as_mut(), path.parent()) {
            if f.is_relative() {
                *f = dir.join(&f);
            }
        }
        if let Some(icon) = cfg.package.icon.as_mut() {
            if let (true, Some(dir)) = (icon.is_relative(), path.parent()) {
                *icon = dir.join(&icon);
            }
            if !icon.is_file() {
                return Err(format!("package.icon not found: {}", icon.display()));
            }
            // Validate it's actually a PNG by magic — a mislabeled JPEG or
            // a .ico would silently break the per-platform conversion later.
            let head = std::fs::read(&*icon).map_err(|e| format!("{}: {e}", icon.display()))?;
            if !head.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
                return Err(format!("package.icon must be a PNG: {}", icon.display()));
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("gba-pack-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// Drop a tiny valid-headed PNG next to the configs and return its name.
    fn write_png(name: &str) -> String {
        let dir = std::env::temp_dir().join("gba-pack-test");
        std::fs::create_dir_all(&dir).unwrap();
        // 8-byte PNG signature is all the loader checks; the rest is
        // exercised at build time by the real encoder/decoder.
        std::fs::write(
            dir.join(name),
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        )
        .unwrap();
        name.to_string()
    }

    #[test]
    fn minimal_config_defaults() {
        let p = write(
            "min.toml",
            &format!(
                "[package]\nname = \"demo\"\n[image]\nrom-sha256 = \"{}\"\nbios-sha256 = \"{}\"\n",
                "a".repeat(64),
                "B".repeat(64),
            ),
        );
        let c = PackConfig::load(&p).unwrap();
        assert_eq!(c.package.platforms.len(), 3);
        assert_eq!(c.image.bios_sha256, "b".repeat(64)); // normalized
        assert!(c.runtime.menu && c.runtime.enhanced_audio && c.runtime.screen_sim);
        assert_eq!(c.runtime.engine_hle, EngineHle::Auto);
        assert!(c.output.binary && !c.output.c_source);
        assert!(c.package.icon.is_none());
    }

    #[test]
    fn icon_field_resolves_and_validates() {
        let png = write_png("game-icon.png");
        let ok = write(
            "icon-ok.toml",
            &format!(
                "[package]\nname = \"demo\"\nicon = \"{png}\"\n\
[image]\nrom-sha256 = \"{0}\"\nbios-sha256 = \"{0}\"\n",
                "a".repeat(64),
            ),
        );
        let c = PackConfig::load(&ok).unwrap();
        let icon = c.package.icon.unwrap();
        assert!(
            icon.is_absolute(),
            "relative icon should resolve against config dir"
        );
        assert!(icon.is_file());

        // Missing file is rejected.
        let missing = write(
            "icon-missing.toml",
            &format!(
                "[package]\nname = \"demo\"\nicon = \"nope.png\"\n\
[image]\nrom-sha256 = \"{0}\"\nbios-sha256 = \"{0}\"\n",
                "a".repeat(64),
            ),
        );
        assert!(PackConfig::load(&missing).is_err());

        // Present but not a PNG is rejected.
        let _notpng = {
            let dir = std::env::temp_dir().join("gba-pack-test");
            std::fs::write(dir.join("notpng.png"), b"GIF89a not really").unwrap();
        };
        let bad = write(
            "icon-notpng.toml",
            &format!(
                "[package]\nname = \"demo\"\nicon = \"notpng.png\"\n\
[image]\nrom-sha256 = \"{0}\"\nbios-sha256 = \"{0}\"\n",
                "a".repeat(64),
            ),
        );
        assert!(PackConfig::load(&bad).is_err());
    }

    #[test]
    fn rejects_bad_sha_unknown_keys_empty_output() {
        let bad_sha = write(
            "bad.toml",
            "[package]\nname = \"x\"\n[image]\nrom-sha256 = \"123\"\nbios-sha256 = \"123\"\n",
        );
        assert!(PackConfig::load(&bad_sha).is_err());
        let unknown = write(
            "unk.toml",
            &format!(
                "[package]\nname = \"x\"\nbogus = 1\n[image]\nrom-sha256 = \"{0}\"\nbios-sha256 = \"{0}\"\n",
                "a".repeat(64)
            ),
        );
        assert!(PackConfig::load(&unknown).is_err());
        let nothing = write(
            "none.toml",
            &format!(
                "[package]\nname = \"x\"\n[image]\nrom-sha256 = \"{0}\"\nbios-sha256 = \"{0}\"\n\
[output]\nbinary = false\nc-source = false\n",
                "a".repeat(64)
            ),
        );
        assert!(PackConfig::load(&nothing).is_err());
    }

    #[test]
    fn full_config_and_relative_labels() {
        let p = write(
            "full.toml",
            &format!(
                r#"
[package]
name = "demo"
version = "1.2.3"
platforms = ["linux", "windows"]

[image]
rom-sha256 = "{0}"
bios-sha256 = "{0}"

[labels]
file = "demo.labels.toml"

[runtime]
menu = false
enhanced-audio = true
screen-sim = false
engine-hle = "gax"

[output]
binary = true
c-source = true
"#,
                "a".repeat(64)
            ),
        );
        let c = PackConfig::load(&p).unwrap();
        assert_eq!(
            c.package.platforms,
            vec![Platform::Linux, Platform::Windows]
        );
        assert_eq!(c.runtime.engine_hle, EngineHle::Gax);
        assert!(!c.runtime.menu);
        assert!(c.output.c_source);
        assert!(c.labels.file.unwrap().is_absolute());
    }
}
