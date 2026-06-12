//! Shared input bindings for the launcher (which edits them) and the play
//! runtime (which honors them). Plain `key = value` text under the user
//! config dir; no dependencies, so the play side stays lean.

use std::path::PathBuf;

/// The ten pad inputs, in KEYINPUT bit order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    A,
    B,
    Select,
    Start,
    Right,
    Left,
    Up,
    Down,
    R,
    L,
}

impl Button {
    pub const ALL: [Button; 10] = [
        Button::A,
        Button::B,
        Button::Select,
        Button::Start,
        Button::Right,
        Button::Left,
        Button::Up,
        Button::Down,
        Button::R,
        Button::L,
    ];

    /// KEYINPUT bit (active-low in hardware; callers handle polarity).
    pub fn bit(self) -> u16 {
        1 << self.index()
    }

    pub fn index(self) -> usize {
        match self {
            Button::A => 0,
            Button::B => 1,
            Button::Select => 2,
            Button::Start => 3,
            Button::Right => 4,
            Button::Left => 5,
            Button::Up => 6,
            Button::Down => 7,
            Button::R => 8,
            Button::L => 9,
        }
    }

    /// Config-file key and display name.
    pub fn name(self) -> &'static str {
        match self {
            Button::A => "a",
            Button::B => "b",
            Button::Select => "select",
            Button::Start => "start",
            Button::Right => "right",
            Button::Left => "left",
            Button::Up => "up",
            Button::Down => "down",
            Button::R => "r",
            Button::L => "l",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Device {
    Keyboard,
    Gamepad,
}

/// Key names are a canonical set understood by both frontends (egui on the
/// launcher side, minifb on the play side): letters `A`..`Z`, digits
/// `0`..`9`, `Up` `Down` `Left` `Right`, `Enter`, `Space`, `Tab`,
/// `Backspace`, `LeftShift`, `RightShift`. Pad names are gilrs button
/// names (`South`, `East`, `DPadUp`, `LeftTrigger`, ...).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InputConfig {
    pub device: Device,
    /// Preferred pad by name; empty = first connected.
    pub gamepad_name: String,
    pub keys: [String; 10],
    pub pads: [String; 10],
}

impl Default for InputConfig {
    fn default() -> Self {
        let s = |v: &str| v.to_string();
        Self {
            device: Device::Keyboard,
            gamepad_name: String::new(),
            // the historical play-command mapping
            keys: [
                s("Z"), s("X"), s("RightShift"), s("Enter"),
                s("Right"), s("Left"), s("Up"), s("Down"),
                s("S"), s("A"),
            ],
            pads: [
                s("East"), s("South"), s("Select"), s("Start"),
                s("DPadRight"), s("DPadLeft"), s("DPadUp"), s("DPadDown"),
                s("RightTrigger"), s("LeftTrigger"),
            ],
        }
    }
}

impl InputConfig {
    pub fn parse(text: &str) -> Self {
        let mut cfg = Self::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "device" => {
                    cfg.device = if v.eq_ignore_ascii_case("gamepad") {
                        Device::Gamepad
                    } else {
                        Device::Keyboard
                    }
                }
                "gamepad_name" => cfg.gamepad_name = v.to_string(),
                _ => {
                    if let Some(name) = k.strip_prefix("key.") {
                        if let Some(b) = button_by_name(name) {
                            cfg.keys[b.index()] = v.to_string();
                        }
                    } else if let Some(name) = k.strip_prefix("pad.") {
                        if let Some(b) = button_by_name(name) {
                            cfg.pads[b.index()] = v.to_string();
                        }
                    }
                }
            }
        }
        cfg
    }

    pub fn serialize(&self) -> String {
        let mut out = String::from("# gba-recomp input bindings\n");
        out.push_str(&format!(
            "device = {}\n",
            match self.device {
                Device::Keyboard => "keyboard",
                Device::Gamepad => "gamepad",
            }
        ));
        out.push_str(&format!("gamepad_name = {}\n", self.gamepad_name));
        for b in Button::ALL {
            out.push_str(&format!("key.{} = {}\n", b.name(), self.keys[b.index()]));
        }
        for b in Button::ALL {
            out.push_str(&format!("pad.{} = {}\n", b.name(), self.pads[b.index()]));
        }
        out
    }

    /// Load from the default path; missing or unreadable = defaults.
    pub fn load() -> Self {
        default_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| Self::parse(&t))
            .unwrap_or_default()
    }

    /// Persist to the default path (creating the directory).
    pub fn save(&self) -> Result<(), String> {
        let path = default_path().ok_or("no config directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, self.serialize()).map_err(|e| e.to_string())
    }
}

pub fn button_by_name(name: &str) -> Option<Button> {
    Button::ALL.into_iter().find(|b| b.name() == name)
}

/// Audio/video settings, launcher-edited, honored by the play runtime.
/// Lives in its own file (`av.cfg`) so input and A/V concerns stay
/// independently versionable.
///
/// Video fields are stored as plain strings here (this crate stays
/// dependency-free); the `screen` crate owns the vocabulary and both
/// frontends parse through it. "auto" means "the screen model's default".
#[derive(Clone, PartialEq, Debug)]
pub struct AvConfig {
    /// The premium audio path (final name TBD): band-limited sinc
    /// resampling, DC blocking, and buffer rate control instead of the
    /// hardware-faithful nearest-neighbor output. Off until the feature
    /// is complete enough to name and ship on by default.
    pub audio_enhanced: bool,
    /// Screen simulation model: raw | unlit | frontlit | backlit | classic.
    pub screen: String,
    /// Viewing-darkness knob for the reflective screens: "auto" or 0..1.
    pub screen_darken: String,
    /// Temporal response: off | simple | smart | persistence.
    pub response: String,
    /// Persistence carryover: "auto" or 0..0.9.
    pub response_keep: String,
    /// Pixel-grid strength: "auto" (per-screen default) or 0..1.
    pub grid: String,
    /// Output colorspace: auto | srgb | display-p3.
    pub display_gamut: String,
}

impl Default for AvConfig {
    fn default() -> Self {
        Self {
            audio_enhanced: false,
            screen: "frontlit".into(),
            screen_darken: "auto".into(),
            response: "smart".into(),
            response_keep: "auto".into(),
            grid: "auto".into(),
            display_gamut: "auto".into(),
        }
    }
}

impl AvConfig {
    pub fn parse(text: &str) -> Self {
        let mut cfg = Self::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "audio.enhanced" => cfg.audio_enhanced = v.eq_ignore_ascii_case("true"),
                "video.screen" => cfg.screen = v.to_ascii_lowercase(),
                "video.darken" => cfg.screen_darken = v.to_ascii_lowercase(),
                "video.response" => cfg.response = v.to_ascii_lowercase(),
                "video.response_keep" => cfg.response_keep = v.to_ascii_lowercase(),
                "video.grid" => cfg.grid = v.to_ascii_lowercase(),
                "video.gamut" => cfg.display_gamut = v.to_ascii_lowercase(),
                _ => {}
            }
        }
        cfg
    }

    pub fn serialize(&self) -> String {
        format!(
            "# gba-recomp a/v settings\n\
             audio.enhanced = {}\n\
             video.screen = {}\n\
             video.darken = {}\n\
             video.response = {}\n\
             video.response_keep = {}\n\
             video.grid = {}\n\
             video.gamut = {}\n",
            self.audio_enhanced,
            self.screen,
            self.screen_darken,
            self.response,
            self.response_keep,
            self.grid,
            self.display_gamut,
        )
    }

    /// "auto" (or anything unparseable) -> None; otherwise the number.
    pub fn knob(value: &str) -> Option<f32> {
        value.parse::<f32>().ok().filter(|v| v.is_finite())
    }

    /// Load from the default path; missing or unreadable = defaults.
    pub fn load() -> Self {
        av_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| Self::parse(&t))
            .unwrap_or_default()
    }

    /// Persist to the default path (creating the directory).
    pub fn save(&self) -> Result<(), String> {
        let path = av_path().ok_or("no config directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, self.serialize()).map_err(|e| e.to_string())
    }
}

/// `<config dir>/gba-recomp/av.cfg`.
pub fn av_path() -> Option<PathBuf> {
    Some(default_path()?.with_file_name("av.cfg"))
}

/// `<config dir>/gba-recomp/input.cfg`, resolved per-platform without
/// pulling in a dependency.
pub fn default_path() -> Option<PathBuf> {
    Some(config_root()?.join("input.cfg"))
}

/// `<config dir>/gba-recomp`, the per-user config/state directory,
/// resolved per-platform without pulling in a dependency.
pub fn config_root() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        match std::env::var_os("XDG_CONFIG_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x),
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        }
    };
    Some(base.join("gba-recomp"))
}

// ── BIOS image (real-BIOS boot) ─────────────────────────────────────
//
// The product boots the real BIOS: the launcher collects the user's
// dump once (first-launch setup), installs it where the whole release
// can find it, and the play runtime recompiles it alongside each
// cartridge. No image installed = the runtime falls back to BIOS HLE.

/// Canonical on-disk name for the installed BIOS image. Lowercase and
/// used verbatim everywhere — Linux filesystems are case-sensitive, so
/// resolution and install must never disagree on case.
pub const BIOS_FILE_NAME: &str = "gba_bios.bin";

/// Expected image size: the BIOS mask ROM is exactly 16 KB.
pub const BIOS_SIZE: usize = 0x4000;

/// SHA-256 of the canonical BIOS dump. Other images (homebrew
/// replacements, bad dumps) are warned about loudly but still tried —
/// the user owns the choice.
pub const BIOS_SHA256: &str =
    "fd2547724b505f487e6dcb29ec2ecff3af35a841a77ab2e85fd87350abd36570";

/// Directory of the running executable — the "release directory" for
/// portable installs. The exe path is canonicalized first so symlinked
/// launches (Linux desktop entries, dev shims) resolve to the real
/// install directory rather than the symlink's.
pub fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(|d| d.to_path_buf())
}

/// Locate the installed BIOS image. Resolution order:
/// 1. `$GBA_RECOMP_BIOS` (explicit override, dev/CI),
/// 2. next to the executable (portable release directory),
/// 3. the per-user config directory (fallback installs — read-only
///    release dirs like /Applications or Program Files).
pub fn find_bios() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("GBA_RECOMP_BIOS") {
        if !v.is_empty() {
            let p = PathBuf::from(v);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Some(dir) = exe_dir() {
        let p = dir.join(BIOS_FILE_NAME);
        if p.is_file() {
            return Some(p);
        }
    }
    let p = config_root()?.join(BIOS_FILE_NAME);
    p.is_file().then_some(p)
}

/// Install a user-selected BIOS image where `find_bios` will see it.
/// Prefers the release directory (portable layout: the whole product
/// travels as one folder); falls back to the per-user config directory
/// when the release directory isn't writable — macOS app translocation
/// and /Applications, Windows Program Files, system-wide Linux installs.
/// The copy is staged to a temp file and renamed so a crash mid-write
/// can never leave a torn image at the resolved name.
pub fn install_bios(src: &std::path::Path) -> Result<PathBuf, String> {
    let bytes = std::fs::read(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if bytes.len() != BIOS_SIZE {
        return Err(format!(
            "{}: expected a {BIOS_SIZE}-byte BIOS image, got {} bytes",
            src.display(),
            bytes.len()
        ));
    }
    let mut targets = Vec::new();
    if let Some(dir) = exe_dir() {
        targets.push(dir.join(BIOS_FILE_NAME));
    }
    if let Some(dir) = config_root() {
        targets.push(dir.join(BIOS_FILE_NAME));
    }
    let mut last_err = "no writable install location".to_string();
    for dest in targets {
        if let Some(dir) = dest.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Same-directory temp + rename: atomic on POSIX, and never
        // crosses a volume boundary (fs::rename can't).
        let tmp = dest.with_extension("bin.tmp");
        let write = std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, &dest));
        match write {
            Ok(()) => return Ok(dest),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                last_err = format!("{}: {e}", dest.display());
            }
        }
    }
    Err(format!("could not install BIOS image ({last_err})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut cfg = InputConfig::default();
        cfg.device = Device::Gamepad;
        cfg.gamepad_name = "Pro Pad 2".into();
        cfg.keys[Button::A.index()] = "Space".into();
        cfg.pads[Button::L.index()] = "LeftTrigger2".into();
        assert_eq!(InputConfig::parse(&cfg.serialize()), cfg);
    }

    #[test]
    fn parse_tolerates_noise() {
        let cfg = InputConfig::parse("garbage\nkey.a = Q # comment\nkey.zz = NO\ndevice = GAMEPAD\n");
        assert_eq!(cfg.keys[Button::A.index()], "Q");
        assert_eq!(cfg.device, Device::Gamepad);
        assert_eq!(cfg.keys[Button::B.index()], "X"); // untouched default
    }

    #[test]
    fn av_roundtrip_and_default_off() {
        assert!(!AvConfig::default().audio_enhanced);
        let cfg = AvConfig { audio_enhanced: true, ..AvConfig::default() };
        assert_eq!(AvConfig::parse(&cfg.serialize()), cfg);
        assert_eq!(
            AvConfig::parse("junk\naudio.enhanced = TRUE # ok\n"),
            cfg,
            "unknown keys keep defaults"
        );
    }

    #[test]
    fn install_bios_rejects_wrong_size() {
        let dir = std::env::temp_dir();
        let src = dir.join("gba-recomp-test-not-a-bios.bin");
        std::fs::write(&src, vec![0u8; 123]).unwrap();
        let err = install_bios(&src).unwrap_err();
        assert!(err.contains("16384-byte"), "got: {err}");
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn av_video_roundtrip() {
        let cfg = AvConfig {
            screen: "backlit".into(),
            screen_darken: "0.5".into(),
            response: "persistence".into(),
            response_keep: "0.42".into(),
            grid: "0".into(),
            display_gamut: "display-p3".into(),
            ..AvConfig::default()
        };
        assert_eq!(AvConfig::parse(&cfg.serialize()), cfg);
        // Old config files (audio-only) parse to video defaults.
        let old = AvConfig::parse("audio.enhanced = true\n");
        assert_eq!(old.screen, "frontlit");
        assert_eq!(old.response, "smart");
        // Knob parsing: auto and garbage are None, numbers are Some.
        assert_eq!(AvConfig::knob("auto"), None);
        assert_eq!(AvConfig::knob("0.5"), Some(0.5));
        assert_eq!(AvConfig::knob("nan"), None);
    }
}
