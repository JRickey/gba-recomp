//! Input device selection and button rebinding. Edits the shared
//! input-config file that the play runtime reads at startup.

use eframe::egui::{self, Event, Key, Sense, Ui, Vec2};
use input_config::{Button, Device, InputConfig};

use crate::theme;

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
            gilrs: gilrs::Gilrs::new().ok(),
            capture: None,
            status: String::new(),
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        // pump pad events; remember presses for capture mode
        let mut pad_presses: Vec<String> = Vec::new();
        let mut pads: Vec<String> = Vec::new();
        if let Some(g) = self.gilrs.as_mut() {
            while let Some(ev) = g.next_event() {
                if let gilrs::EventType::ButtonPressed(btn, _) = ev.event {
                    pad_presses.push(format!("{btn:?}"));
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

            ui.add_space(12.0);
            theme::section_title(ui, "Bindings");
            ui.add_space(4.0);

            let gamepad = self.cfg.device == Device::Gamepad;
            let pulse = ui.ctx().input(|i| i.time);
            let blink = (pulse * 2.5).sin() > 0.0;

            egui::Grid::new("bindings").num_columns(4).spacing([18.0, 7.0]).show(ui, |ui| {
                for pair in Button::ALL.chunks(2) {
                    for &b in pair {
                        // button label with orb
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
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
                            if blink { "PRESS\u{2026}".to_string() } else { String::new() }
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

            // capture: next key / pad button becomes the binding
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
                            Event::Key { key, pressed: true, .. } => Some(*key),
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

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::glossy_button(ui, "RESTORE DEFAULTS", false, Vec2::new(170.0, 28.0))
                    .clicked()
                {
                    let d = InputConfig::default();
                    self.cfg.keys = d.keys;
                    self.cfg.pads = d.pads;
                    self.capture = None;
                    self.persist();
                }
                let hint = if self.status.is_empty() {
                    "click a binding, then press the new key / pad button — Esc cancels"
                } else {
                    &self.status
                };
                ui.label(egui::RichText::new(hint).size(11.0).color(theme::white(120)));
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

    fn persist(&mut self) {
        self.status = match self.cfg.save() {
            Ok(()) => "saved \u{2713}".to_string(),
            Err(e) => format!("save failed: {e}"),
        };
    }
}

/// Egui key to the canonical name shared with the play runtime.
fn key_name(key: Key) -> Option<&'static str> {
    use Key::*;
    Some(match key {
        A => "A", B => "B", C => "C", D => "D", E => "E", F => "F", G => "G",
        H => "H", I => "I", J => "J", K => "K", L => "L", M => "M", N => "N",
        O => "O", P => "P", Q => "Q", R => "R", S => "S", T => "T", U => "U",
        V => "V", W => "W", X => "X", Y => "Y", Z => "Z",
        Num0 => "0", Num1 => "1", Num2 => "2", Num3 => "3", Num4 => "4",
        Num5 => "5", Num6 => "6", Num7 => "7", Num8 => "8", Num9 => "9",
        ArrowUp => "Up", ArrowDown => "Down", ArrowLeft => "Left", ArrowRight => "Right",
        Enter => "Enter", Space => "Space", Tab => "Tab", Backspace => "Backspace",
        _ => return None,
    })
}
