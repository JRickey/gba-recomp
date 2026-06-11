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

/// `<config dir>/gba-recomp/input.cfg`, resolved per-platform without
/// pulling in a dependency.
pub fn default_path() -> Option<PathBuf> {
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
    Some(base.join("gba-recomp").join("input.cfg"))
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
}
