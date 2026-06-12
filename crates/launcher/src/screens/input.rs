//! Input device selection and button rebinding. Edits the shared
//! input-config file that the play runtime reads at startup.

use eframe::egui::{self, Event, Key, Sense, Ui, Vec2};
use input_config::{Button, Device, DpadSource, InputConfig, StickToken};

use crate::theme;

/// Vendored community controller mapping DB (zlib; see THIRD-PARTY.md).
/// Fed on top of gilrs's built-in snapshot so the launcher recognizes the
/// same broad controller set the play runtime does — both must agree on
/// pad names, or a binding made here wouldn't fire there.
const CONTROLLER_DB: &str = include_str!("../../../../assets/gamecontrollerdb.txt");

fn build_gilrs() -> Option<gilrs::Gilrs> {
    gilrs::GilrsBuilder::new()
        .add_mappings(CONTROLLER_DB)
        .build()
        .ok()
        .or_else(|| gilrs::Gilrs::new().ok())
}

pub struct InputScreen {
    cfg: InputConfig,
    gilrs: Option<gilrs::Gilrs>,
    capture: Option<Button>,
    status: String,
}

impl InputScreen {
    pub fn new() -> Self {
        Self {
            cfg: InputConfig::load(),
            gilrs: build_gilrs(),
            capture: None,
            status: String::new(),
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        // Pump pad events; remember button presses and axis pushes for
        // capture, so a stick direction can be bound like a button. The
        // gamepad list is rebuilt each frame, so hotplug shows up live.
        let mut pad_presses: Vec<String> = Vec::new();
        let mut pads: Vec<String> = Vec::new();
        if let Some(g) = self.gilrs.as_mut() {
            while let Some(ev) = g.next_event() {
                match ev.event {
                    gilrs::EventType::ButtonPressed(btn, _) => {
                        pad_presses.push(format!("{btn:?}"));
                    }
                    // An axis pushed well past a usable deadzone is a
                    // bindable source (lets the user map any GBA button to
                    // a stick direction). Use a firm threshold so resting
                    // drift never captures.
                    gilrs::EventType::AxisChanged(axis, value, _) => {
                        if let Some(tok) = stick_token_from_axis(axis, value) {
                            pad_presses.push(tok.name().to_string());
                        }
                    }
                    _ => {}
                }
            }
            pads = g.gamepads().map(|(_, gp)| gp.name().to_string()).collect();
        }

        theme::glass_frame().show(ui, |ui| {
            ui.set_min_size(Vec2::new(ui.available_width(), ui.available_height()));
            theme::section_title(ui, "Controllers");
            ui.add_space(4.0);

            // device pills: keyboard + every connected pad
            ui.horizontal_wrapped(|ui| {
                let kb = self.cfg.device == Device::Keyboard;
                if theme::glossy_button(ui, "\u{2328} KEYBOARD", kb, Vec2::new(150.0, 32.0))
                    .clicked()
                {
                    self.cfg.device = Device::Keyboard;
                    self.persist();
                }
                for name in &pads {
                    let sel = self.cfg.device == Device::Gamepad
                        && (self.cfg.gamepad_name == *name || self.cfg.gamepad_name.is_empty());
                    let label = format!("\u{1F3AE} {}", name.to_uppercase());
                    if theme::glossy_button(ui, &label, sel, Vec2::new(220.0, 32.0)).clicked() {
                        self.cfg.device = Device::Gamepad;
                        self.cfg.gamepad_name = name.clone();
                        self.persist();
                    }
                }
                if pads.is_empty() {
                    ui.label(
                        egui::RichText::new("no pads detected — plug one in, it shows up here")
                            .size(11.0)
                            .color(theme::white(110)),
                    );
                }
            });

            let gamepad = self.cfg.device == Device::Gamepad;

            // Analog section: only meaningful on a gamepad. D-pad source +
            // deadzone steer how the sticks map onto the GBA d-pad.
            if gamepad {
                ui.add_space(12.0);
                theme::section_title(ui, "Analog");
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new("D-PAD FROM")
                            .size(12.0)
                            .strong()
                            .color(theme::SILVER),
                    );
                    ui.add_space(6.0);
                    for (label, src) in [
                        ("LEFT STICK", DpadSource::LeftStick),
                        ("RIGHT STICK", DpadSource::RightStick),
                        ("D-PAD", DpadSource::Dpad),
                        ("BOTH", DpadSource::Both),
                    ] {
                        let sel = self.cfg.dpad_source == src;
                        if theme::glossy_button(ui, label, sel, Vec2::new(116.0, 28.0)).clicked()
                            && !sel
                        {
                            self.cfg.dpad_source = src;
                            self.persist();
                        }
                    }
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("DEADZONE")
                            .size(12.0)
                            .strong()
                            .color(theme::SILVER),
                    );
                    ui.add_space(6.0);
                    let mut dz = self.cfg.stick_deadzone;
                    let slider = egui::Slider::new(
                        &mut dz,
                        input_config::DEADZONE_MIN..=input_config::DEADZONE_MAX,
                    )
                    .fixed_decimals(2)
                    .trailing_fill(true);
                    // Persist only when the value actually moves — the
                    // slider reports `changed` while dragging.
                    if ui.add(slider).changed()
                        && (dz - self.cfg.stick_deadzone).abs() > f32::EPSILON
                    {
                        self.cfg.stick_deadzone = dz;
                        self.persist();
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "stick must pass the deadzone to register — raise it if a resting \
                         stick drifts",
                    )
                    .size(10.0)
                    .color(theme::white(90)),
                );
            }

            ui.add_space(12.0);
            theme::section_title(ui, "Bindings");
            ui.add_space(4.0);

            let pulse = ui.ctx().input(|i| i.time);
            let blink = (pulse * 2.5).sin() > 0.0;

            egui::Grid::new("bindings")
                .num_columns(4)
                .spacing([18.0, 7.0])
                .show(ui, |ui| {
                    for pair in Button::ALL.chunks(2) {
                        for &b in pair {
                            // button label with orb
                            ui.horizontal(|ui| {
                                let (r, _) =
                                    ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                                theme::orb(ui.painter(), r.center(), 5.0, theme::VIOLET);
                                ui.label(
                                    egui::RichText::new(b.name().to_uppercase())
                                        .size(13.0)
                                        .strong()
                                        .color(theme::SILVER_HI),
                                );
                            });
                            // current binding / capture pill
                            let capturing = self.capture == Some(b);
                            let text = if capturing {
                                if blink {
                                    "PRESS\u{2026}".to_string()
                                } else {
                                    String::new()
                                }
                            } else if gamepad {
                                self.cfg.pads[b.index()].clone()
                            } else {
                                self.cfg.keys[b.index()].clone()
                            };
                            if theme::glossy_button(ui, &text, capturing, Vec2::new(118.0, 26.0))
                                .clicked()
                            {
                                self.capture = if capturing { None } else { Some(b) };
                                self.status.clear();
                            }
                        }
                        ui.end_row();
                    }
                });

            // capture: next key / pad button (or stick push) becomes the binding
            if let Some(b) = self.capture {
                if ui.ctx().input(|i| i.key_pressed(Key::Escape)) {
                    self.capture = None;
                } else if gamepad {
                    if let Some(name) = pad_presses.first() {
                        self.cfg.pads[b.index()] = name.clone();
                        self.capture = None;
                        self.persist();
                    }
                } else {
                    let pressed: Option<Key> = ui.ctx().input(|i| {
                        i.events.iter().find_map(|e| match e {
                            Event::Key {
                                key, pressed: true, ..
                            } => Some(*key),
                            _ => None,
                        })
                    });
                    if let Some(key) = pressed {
                        match key_name(key) {
                            Some(name) => {
                                self.cfg.keys[b.index()] = name.to_string();
                                self.capture = None;
                                self.persist();
                            }
                            None => self.status = "that key can't be bound here".to_string(),
                        }
                    }
                }
            }

            // Live input monitor: which GBA buttons the selected pad is
            // driving right now, so the user can confirm a fresh mapping.
            if gamepad {
                ui.add_space(8.0);
                self.input_monitor(ui);
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::glossy_button(ui, "RESTORE DEFAULTS", false, Vec2::new(170.0, 28.0))
                    .clicked()
                {
                    let d = InputConfig::default();
                    self.cfg.keys = d.keys;
                    self.cfg.pads = d.pads;
                    self.cfg.dpad_source = d.dpad_source;
                    self.cfg.stick_deadzone = d.stick_deadzone;
                    self.capture = None;
                    self.persist();
                }
                let hint = if self.status.is_empty() {
                    "click a binding, then press the new key / pad button / stick — Esc cancels"
                } else {
                    &self.status
                };
                ui.label(
                    egui::RichText::new(hint)
                        .size(11.0)
                        .color(theme::white(120)),
                );
            });
            if let Some(p) = input_config::default_path() {
                ui.label(
                    egui::RichText::new(format!("saved to {}", p.display()))
                        .size(10.0)
                        .color(theme::white(80)),
                );
            }
        });
    }

    /// Render lit orbs for every GBA button the selected pad is currently
    /// pressing (per the live bindings, including stick directions). Reads
    /// the pad state directly — gilrs keeps it updated as events pump.
    fn input_monitor(&mut self, ui: &mut Ui) {
        let dz = self.cfg.stick_deadzone;
        let active: Vec<bool> = {
            let gp = self.gilrs.as_ref().and_then(|g| {
                g.gamepads()
                    .find(|(_, gp)| gp.name() == self.cfg.gamepad_name)
                    .or_else(|| g.gamepads().next())
                    .map(|(_, gp)| gp)
            });
            Button::ALL
                .iter()
                .map(|b| match &gp {
                    Some(gp) => {
                        pad_active(gp, &self.cfg.pads[b.index()], self.cfg.dpad_source, dz, *b)
                    }
                    None => false,
                })
                .collect()
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("LIVE")
                    .size(11.0)
                    .strong()
                    .color(theme::SILVER_LO),
            );
            ui.add_space(4.0);
            for (i, b) in Button::ALL.iter().enumerate() {
                let on = active[i];
                let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                let col = if on { theme::CYAN } else { theme::white(28) };
                theme::orb(ui.painter(), r.center(), 5.0, col);
                ui.label(
                    egui::RichText::new(b.name().to_uppercase())
                        .size(10.0)
                        .color(if on {
                            theme::SILVER_HI
                        } else {
                            theme::white(70)
                        }),
                );
                ui.add_space(4.0);
            }
        });
        // Repaint so the monitor tracks input without needing mouse motion.
        ui.ctx().request_repaint();
    }

    fn persist(&mut self) {
        self.status = match self.cfg.save() {
            Ok(()) => "saved \u{2713}".to_string(),
            Err(e) => format!("save failed: {e}"),
        };
    }
}

/// Map an `AxisChanged` event past a firm capture threshold to the stick
/// token it represents. Triggers (`LeftZ`/`RightZ`) and the hat axes are
/// not stick directions and are skipped. The threshold is intentionally
/// higher than any sane play deadzone so capture is deliberate.
fn stick_token_from_axis(axis: gilrs::Axis, value: f32) -> Option<StickToken> {
    use gilrs::Axis::*;
    const CAPTURE: f32 = 0.7;
    if value.abs() < CAPTURE {
        return None;
    }
    let pos = value > 0.0;
    Some(match axis {
        LeftStickX => {
            if pos {
                StickToken::LeftStickRight
            } else {
                StickToken::LeftStickLeft
            }
        }
        LeftStickY => {
            if pos {
                StickToken::LeftStickUp
            } else {
                StickToken::LeftStickDown
            }
        }
        RightStickX => {
            if pos {
                StickToken::RightStickRight
            } else {
                StickToken::RightStickLeft
            }
        }
        RightStickY => {
            if pos {
                StickToken::RightStickUp
            } else {
                StickToken::RightStickDown
            }
        }
        _ => return None,
    })
}

/// Is the GBA button `b` currently driven on this pad? Mirrors the play
/// runtime: a button/stick-token binding, plus the configured d-pad
/// source for the four directions.
fn pad_active(
    gp: &gilrs::Gamepad,
    name: &str,
    dpad_source: DpadSource,
    deadzone: f32,
    b: Button,
) -> bool {
    if binding_active(gp, name, deadzone) {
        return true;
    }
    // The dpad-source stick also lights the directional buttons.
    let stick = match b {
        Button::Right => StickToken::dir(dpad_source, true, true),
        Button::Left => StickToken::dir(dpad_source, true, false),
        Button::Up => StickToken::dir(dpad_source, false, true),
        Button::Down => StickToken::dir(dpad_source, false, false),
        _ => None,
    };
    match stick {
        Some(tok) => stick_active(gp, tok, deadzone),
        None => false,
    }
}

fn binding_active(gp: &gilrs::Gamepad, name: &str, deadzone: f32) -> bool {
    match input_config::pad_binding(name) {
        input_config::PadBinding::Button(b) => {
            gilrs_button(b).is_some_and(|btn| gp.is_pressed(btn))
        }
        input_config::PadBinding::Stick(t) => stick_active(gp, t, deadzone),
    }
}

fn stick_active(gp: &gilrs::Gamepad, tok: StickToken, deadzone: f32) -> bool {
    use gilrs::Axis::*;
    let (horizontal, positive) = tok.axis();
    let axis = match (tok.left(), horizontal) {
        (true, true) => LeftStickX,
        (true, false) => LeftStickY,
        (false, true) => RightStickX,
        (false, false) => RightStickY,
    };
    let v = gp.value(axis);
    if positive {
        v > deadzone
    } else {
        v < -deadzone
    }
}

fn gilrs_button(name: &str) -> Option<gilrs::Button> {
    use gilrs::Button::*;
    Some(match name {
        "South" => South,
        "East" => East,
        "North" => North,
        "West" => West,
        "C" => C,
        "Z" => Z,
        "LeftTrigger" => LeftTrigger,
        "LeftTrigger2" => LeftTrigger2,
        "RightTrigger" => RightTrigger,
        "RightTrigger2" => RightTrigger2,
        "Select" => Select,
        "Start" => Start,
        "Mode" => Mode,
        "LeftThumb" => LeftThumb,
        "RightThumb" => RightThumb,
        "DPadUp" => DPadUp,
        "DPadDown" => DPadDown,
        "DPadLeft" => DPadLeft,
        "DPadRight" => DPadRight,
        _ => return None,
    })
}

/// Egui key to the canonical name shared with the play runtime.
fn key_name(key: Key) -> Option<&'static str> {
    use Key::*;
    Some(match key {
        A => "A",
        B => "B",
        C => "C",
        D => "D",
        E => "E",
        F => "F",
        G => "G",
        H => "H",
        I => "I",
        J => "J",
        K => "K",
        L => "L",
        M => "M",
        N => "N",
        O => "O",
        P => "P",
        Q => "Q",
        R => "R",
        S => "S",
        T => "T",
        U => "U",
        V => "V",
        W => "W",
        X => "X",
        Y => "Y",
        Z => "Z",
        Num0 => "0",
        Num1 => "1",
        Num2 => "2",
        Num3 => "3",
        Num4 => "4",
        Num5 => "5",
        Num6 => "6",
        Num7 => "7",
        Num8 => "8",
        Num9 => "9",
        ArrowUp => "Up",
        ArrowDown => "Down",
        ArrowLeft => "Left",
        ArrowRight => "Right",
        Enter => "Enter",
        Space => "Space",
        Tab => "Tab",
        Backspace => "Backspace",
        _ => return None,
    })
}
