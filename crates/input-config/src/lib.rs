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

/// Which analog/digital source drives the GBA d-pad. `Both` (the
/// historical behavior) lets the left stick AND the physical d-pad both
/// move the player; `Dpad` ignores the sticks entirely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DpadSource {
    LeftStick,
    RightStick,
    Dpad,
    Both,
}

impl DpadSource {
    /// Does this source read the left analog stick?
    pub fn uses_left_stick(self) -> bool {
        matches!(self, DpadSource::LeftStick | DpadSource::Both)
    }

    /// Does this source read the right analog stick?
    pub fn uses_right_stick(self) -> bool {
        matches!(self, DpadSource::RightStick)
    }

    fn token(self) -> &'static str {
        match self {
            DpadSource::LeftStick => "leftstick",
            DpadSource::RightStick => "rightstick",
            DpadSource::Dpad => "dpad",
            DpadSource::Both => "both",
        }
    }

    fn from_token(s: &str) -> Option<Self> {
        Some(match s {
            "leftstick" => DpadSource::LeftStick,
            "rightstick" => DpadSource::RightStick,
            "dpad" => DpadSource::Dpad,
            "both" => DpadSource::Both,
            _ => return None,
        })
    }
}

/// Default deadzone for stick-driven directions, and the range the
/// launcher clamps the user's choice into. Below ~0.05 stick drift leaks
/// through; above ~0.95 the stick is effectively unusable.
pub const DEADZONE_DEFAULT: f32 = 0.5;
pub const DEADZONE_MIN: f32 = 0.05;
pub const DEADZONE_MAX: f32 = 0.95;

/// A stick direction usable as a binding source for any of the ten pad
/// inputs (e.g. binding R to "RightStickUp"). The play runtime resolves
/// these against the live axis values past the configured deadzone; the
/// launcher recognizes an axis push as a bindable source during capture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StickToken {
    LeftStickUp,
    LeftStickDown,
    LeftStickLeft,
    LeftStickRight,
    RightStickUp,
    RightStickDown,
    RightStickLeft,
    RightStickRight,
}

impl StickToken {
    pub const ALL: [StickToken; 8] = [
        StickToken::LeftStickUp,
        StickToken::LeftStickDown,
        StickToken::LeftStickLeft,
        StickToken::LeftStickRight,
        StickToken::RightStickUp,
        StickToken::RightStickDown,
        StickToken::RightStickLeft,
        StickToken::RightStickRight,
    ];

    /// The canonical token stored in the config and shown in the UI.
    pub fn name(self) -> &'static str {
        match self {
            StickToken::LeftStickUp => "LeftStickUp",
            StickToken::LeftStickDown => "LeftStickDown",
            StickToken::LeftStickLeft => "LeftStickLeft",
            StickToken::LeftStickRight => "LeftStickRight",
            StickToken::RightStickUp => "RightStickUp",
            StickToken::RightStickDown => "RightStickDown",
            StickToken::RightStickLeft => "RightStickLeft",
            StickToken::RightStickRight => "RightStickRight",
        }
    }

    /// Parse a stored token back into a stick direction.
    pub fn by_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }

    /// `true` for the left stick, `false` for the right.
    pub fn left(self) -> bool {
        matches!(
            self,
            StickToken::LeftStickUp
                | StickToken::LeftStickDown
                | StickToken::LeftStickLeft
                | StickToken::LeftStickRight
        )
    }

    /// The axis component this token reads (`(horizontal, positive)`):
    /// horizontal = X axis, else Y axis; positive = push toward +value.
    /// Y is reported stick-up-positive by gilrs, matching this mapping.
    pub fn axis(self) -> (bool, bool) {
        match self {
            StickToken::LeftStickRight | StickToken::RightStickRight => (true, true),
            StickToken::LeftStickLeft | StickToken::RightStickLeft => (true, false),
            StickToken::LeftStickUp | StickToken::RightStickUp => (false, true),
            StickToken::LeftStickDown | StickToken::RightStickDown => (false, false),
        }
    }

    /// The stick token that a `DpadSource` maps onto a given direction
    /// (`(horizontal, positive)` axis geometry), or `None` when the source
    /// reads no stick (`Dpad`). Lets both frontends derive which stick
    /// direction lights a d-pad button from one place.
    pub fn dir(source: DpadSource, horizontal: bool, positive: bool) -> Option<StickToken> {
        let left = match source {
            DpadSource::LeftStick | DpadSource::Both => true,
            DpadSource::RightStick => false,
            DpadSource::Dpad => return None,
        };
        Some(match (left, horizontal, positive) {
            (true, true, true) => StickToken::LeftStickRight,
            (true, true, false) => StickToken::LeftStickLeft,
            (true, false, true) => StickToken::LeftStickUp,
            (true, false, false) => StickToken::LeftStickDown,
            (false, true, true) => StickToken::RightStickRight,
            (false, true, false) => StickToken::RightStickLeft,
            (false, false, true) => StickToken::RightStickUp,
            (false, false, false) => StickToken::RightStickDown,
        })
    }
}

/// Key names are a canonical set understood by both frontends (egui on the
/// launcher side, winit on the play side): letters `A`..`Z`, digits
/// `0`..`9`, `Up` `Down` `Left` `Right`, `Enter`, `Space`, `Tab`,
/// `Backspace`, `LeftShift`, `RightShift`. Pad names are gilrs button
/// names (`South`, `East`, `DPadUp`, `LeftTrigger`, ...).
///
/// Not `Eq` — `stick_deadzone` is an `f32`. Equality is `PartialEq`,
/// which is all the tests and config diffing need.
#[derive(Clone, PartialEq, Debug)]
pub struct InputConfig {
    pub device: Device,
    /// Preferred pad by name; empty = first connected.
    pub gamepad_name: String,
    pub keys: [String; 10],
    /// Pad bindings: a gilrs button name (`South`, `DPadUp`, ...) or a
    /// stick-direction token (`LeftStickUp`, ...). The play runtime
    /// resolves stick tokens against the live axes past `stick_deadzone`.
    pub pads: [String; 10],
    /// Which source(s) move the GBA d-pad on a gamepad.
    pub dpad_source: DpadSource,
    /// Threshold a stick axis must pass to register as a direction.
    pub stick_deadzone: f32,
}

impl Default for InputConfig {
    fn default() -> Self {
        let s = |v: &str| v.to_string();
        Self {
            device: Device::Keyboard,
            gamepad_name: String::new(),
            // the historical play-command mapping
            keys: [
                s("Z"),
                s("X"),
                s("RightShift"),
                s("Enter"),
                s("Right"),
                s("Left"),
                s("Up"),
                s("Down"),
                s("S"),
                s("A"),
            ],
            pads: [
                s("East"),
                s("South"),
                s("Select"),
                s("Start"),
                s("DPadRight"),
                s("DPadLeft"),
                s("DPadUp"),
                s("DPadDown"),
                s("RightTrigger"),
                s("LeftTrigger"),
            ],
            // Default matches the historical hardcoded play behavior:
            // left stick AND the physical d-pad bindings both steer.
            dpad_source: DpadSource::Both,
            stick_deadzone: DEADZONE_DEFAULT,
        }
    }
}

impl InputConfig {
    pub fn parse(text: &str) -> Self {
        let mut cfg = Self::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
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
                "dpad_source" => {
                    if let Some(d) = DpadSource::from_token(&v.to_ascii_lowercase()) {
                        cfg.dpad_source = d;
                    }
                }
                "stick_deadzone" => {
                    if let Ok(z) = v.parse::<f32>() {
                        if z.is_finite() {
                            cfg.stick_deadzone = z.clamp(DEADZONE_MIN, DEADZONE_MAX);
                        }
                    }
                }
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
        out.push_str(&format!("dpad_source = {}\n", self.dpad_source.token()));
        out.push_str(&format!("stick_deadzone = {:.2}\n", self.stick_deadzone));
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

/// A resolved pad binding: either a physical button (by gilrs name) or a
/// stick direction. The play runtime maps the button name through gilrs
/// and the stick token through the live axes; keeping the split here lets
/// the dependency-free model own the token vocabulary.
pub enum PadBinding<'a> {
    Button(&'a str),
    Stick(StickToken),
}

/// Classify a stored pad binding. A recognized stick token resolves to
/// `Stick`; everything else is handed back as a button name for gilrs to
/// interpret (unknown names there simply never fire).
pub fn pad_binding(name: &str) -> PadBinding<'_> {
    match StickToken::by_name(name) {
        Some(t) => PadBinding::Stick(t),
        None => PadBinding::Button(name),
    }
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
    /// Presentation vsync. Off (default) presents immediately
    /// (AutoNoVsync); on trades possible tearing for compositor pacing.
    /// Emulation speed is owned by the audio clock either way.
    pub video_vsync: bool,
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
            video_vsync: false,
        }
    }
}

impl AvConfig {
    pub fn parse(text: &str) -> Self {
        let mut cfg = Self::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "audio.enhanced" => cfg.audio_enhanced = v.eq_ignore_ascii_case("true"),
                "video.screen" => cfg.screen = v.to_ascii_lowercase(),
                "video.darken" => cfg.screen_darken = v.to_ascii_lowercase(),
                "video.response" => cfg.response = v.to_ascii_lowercase(),
                "video.response_keep" => cfg.response_keep = v.to_ascii_lowercase(),
                "video.grid" => cfg.grid = v.to_ascii_lowercase(),
                "video.gamut" => cfg.display_gamut = v.to_ascii_lowercase(),
                "video.vsync" => cfg.video_vsync = v.eq_ignore_ascii_case("true"),
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
             video.gamut = {}\n\
             video.vsync = {}\n",
            self.audio_enhanced,
            self.screen,
            self.screen_darken,
            self.response,
            self.response_keep,
            self.grid,
            self.display_gamut,
            self.video_vsync,
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
pub const BIOS_SHA256: &str = "fd2547724b505f487e6dcb29ec2ecff3af35a841a77ab2e85fd87350abd36570";

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
        // stick-as-button binding survives the round-trip too
        cfg.pads[Button::R.index()] = "RightStickUp".into();
        cfg.dpad_source = DpadSource::RightStick;
        cfg.stick_deadzone = 0.30;
        assert_eq!(InputConfig::parse(&cfg.serialize()), cfg);
    }

    #[test]
    fn parse_tolerates_noise() {
        let cfg =
            InputConfig::parse("garbage\nkey.a = Q # comment\nkey.zz = NO\ndevice = GAMEPAD\n");
        assert_eq!(cfg.keys[Button::A.index()], "Q");
        assert_eq!(cfg.device, Device::Gamepad);
        assert_eq!(cfg.keys[Button::B.index()], "X"); // untouched default
    }

    #[test]
    fn serialize_spells_analog_keys_as_specified() {
        // Lock the on-disk format so launcher and play never disagree.
        let s = InputConfig::default().serialize();
        assert!(s.contains("dpad_source = both\n"), "got:\n{s}");
        assert!(s.contains("stick_deadzone = 0.50\n"), "got:\n{s}");
        let cfg = InputConfig {
            dpad_source: DpadSource::RightStick,
            stick_deadzone: 0.25,
            ..InputConfig::default()
        };
        let s = cfg.serialize();
        assert!(s.contains("dpad_source = rightstick\n"), "got:\n{s}");
        assert!(s.contains("stick_deadzone = 0.25\n"), "got:\n{s}");
    }

    #[test]
    fn new_fields_default_and_clamp() {
        // Old config files (no analog keys) load with the defaults that
        // reproduce the historical behavior.
        let old = InputConfig::parse("device = gamepad\npad.a = South\n");
        assert_eq!(old.dpad_source, DpadSource::Both);
        assert_eq!(old.stick_deadzone, DEADZONE_DEFAULT);

        // dpad_source parses case-insensitively; bad values keep default.
        assert_eq!(
            InputConfig::parse("dpad_source = LEFTSTICK\n").dpad_source,
            DpadSource::LeftStick
        );
        assert_eq!(
            InputConfig::parse("dpad_source = nonsense\n").dpad_source,
            DpadSource::Both
        );

        // deadzone clamps into the launcher's range; garbage keeps default.
        assert_eq!(
            InputConfig::parse("stick_deadzone = 0.01\n").stick_deadzone,
            DEADZONE_MIN
        );
        assert_eq!(
            InputConfig::parse("stick_deadzone = 9.0\n").stick_deadzone,
            DEADZONE_MAX
        );
        assert_eq!(
            InputConfig::parse("stick_deadzone = abc\n").stick_deadzone,
            DEADZONE_DEFAULT
        );
    }

    #[test]
    fn stick_tokens_classify_and_resolve() {
        // A stick token resolves to Stick; anything else is a button.
        assert!(matches!(
            pad_binding("LeftStickUp"),
            PadBinding::Stick(StickToken::LeftStickUp)
        ));
        assert!(matches!(pad_binding("South"), PadBinding::Button("South")));
        // axis geometry: horizontal flag + positive direction.
        assert_eq!(StickToken::LeftStickRight.axis(), (true, true));
        assert_eq!(StickToken::RightStickDown.axis(), (false, false));
        assert!(StickToken::LeftStickUp.left());
        assert!(!StickToken::RightStickUp.left());
    }

    #[test]
    fn av_roundtrip_and_default_off() {
        assert!(!AvConfig::default().audio_enhanced);
        let cfg = AvConfig {
            audio_enhanced: true,
            ..AvConfig::default()
        };
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
