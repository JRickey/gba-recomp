//! Audio and video settings (one shared av.cfg, two tabs). Audio holds
//! the first premium enhancement toggle (unnamed until the feature is
//! complete); video is the screen simulation: per-revision panel models,
//! temporal response, pixel grid, and output-colorspace handling.

use eframe::egui::{self, Ui, Vec2};

use crate::theme;

pub struct AvScreen {
    av: input_config::AvConfig,
}

impl AvScreen {
    pub fn new() -> Self {
        Self {
            av: input_config::AvConfig::load(),
        }
    }

    fn save(&self) {
        if let Err(e) = self.av.save() {
            eprintln!("av.cfg: {e}");
        }
    }

    pub fn audio_ui(&mut self, ui: &mut Ui) {
        theme::glass_frame().show(ui, |ui| {
            theme::section_title(ui, "Audio");
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                let label = if self.av.audio_enhanced {
                    "ENHANCED AUDIO \u{2014} ON"
                } else {
                    "ENHANCED AUDIO \u{2014} OFF"
                };
                if theme::glossy_button(ui, label, self.av.audio_enhanced, Vec2::new(240.0, 34.0))
                    .clicked()
                {
                    self.av.audio_enhanced = !self.av.audio_enhanced;
                    self.save();
                }
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Working title. Cleaner sound than the original speaker: \
                     band-limited resampling and filtering tuned for modern \
                     speakers and headphones. Off = exactly as the hardware \
                     mixed it.",
                )
                .size(11.0)
                .color(theme::white(140)),
            );

            applies_note(ui);
        });
    }

    pub fn video_ui(&mut self, ui: &mut Ui) {
        theme::glass_frame().show(ui, |ui| {
            theme::section_title(ui, "Screen Simulation");
            ui.add_space(6.0);

            // Screen model: which physical screen revision to reproduce.
            ui.horizontal_wrapped(|ui| {
                for (token, label) in [
                    ("raw", "RAW"),
                    ("unlit", "ORIGINAL"),
                    ("frontlit", "FRONTLIT"),
                    ("backlit", "BACKLIT"),
                    ("classic", "CLASSIC"),
                ] {
                    let selected = self.av.screen == token;
                    if theme::glossy_button(ui, label, selected, Vec2::new(106.0, 30.0)).clicked()
                        && !selected
                    {
                        self.av.screen = token.into();
                        self.save();
                    }
                }
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(match self.av.screen.as_str() {
                    "raw" => {
                        "Pixels exactly as the game wrote them \u{2014} vivid, \
                         oversaturated, not what players saw on hardware."
                    }
                    "unlit" => {
                        "The launch-era reflective screen, no light: deep tone \
                         response, washed colors, ambient glow in the blacks."
                    }
                    "backlit" => {
                        "The late backlit revision: near-modern colors, clean \
                         blacks. The screen late-era games were tuned on."
                    }
                    "classic" => {
                        "The community-classic gamma-4.0 color model, kept \
                         bit-faithful for people who know that exact look."
                    }
                    _ => {
                        "The frontlit screen most of the library was designed \
                         for \u{2014} measured colors, comfortable brightness. \
                         Recommended."
                    }
                })
                .size(11.0)
                .color(theme::white(140)),
            );

            // Reflective-family viewing darkness (auto = per-screen default).
            if matches!(self.av.screen.as_str(), "unlit" | "frontlit") {
                ui.add_space(6.0);
                self.knob_row(
                    ui,
                    "Viewing darkness",
                    "video.darken",
                    0.0..=1.0,
                    |av| &mut av.screen_darken,
                    |s| {
                        screen::ScreenKind::by_name(s)
                            .map(|k| k.default_darken() as f32)
                            .unwrap_or(0.0)
                    },
                );
            }

            // Temporal response.
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("RESPONSE")
                        .size(11.0)
                        .color(theme::white(160)),
                );
                for (token, label) in [
                    ("off", "OFF"),
                    ("simple", "BLEND"),
                    ("smart", "SMART"),
                    ("persistence", "GHOST"),
                ] {
                    let selected = self.av.response == token;
                    if theme::glossy_button(ui, label, selected, Vec2::new(88.0, 26.0)).clicked()
                        && !selected
                    {
                        self.av.response = token.into();
                        self.save();
                    }
                }
            });
            ui.label(
                egui::RichText::new(
                    "The original panel fused alternating frames; many games \
                     rely on it for transparency. Smart blends only pixels \
                     that actually flicker. Off can visibly strobe in those \
                     games.",
                )
                .size(11.0)
                .color(theme::white(140)),
            );

            // Pixel grid.
            ui.add_space(8.0);
            self.knob_row(
                ui,
                "Pixel grid",
                "video.grid",
                0.0..=1.0,
                |av| &mut av.grid,
                |s| {
                    screen::ScreenKind::by_name(s)
                        .map(screen::ScreenKind::default_grid_strength)
                        .unwrap_or(0.0)
                },
            );
            ui.label(
                egui::RichText::new(
                    "Subpixel stripes and row gaps, integrated so they stay \
                     clean at any window size. Fades out automatically below \
                     ~2\u{00d7} scale.",
                )
                .size(11.0)
                .color(theme::white(140)),
            );

            // Output colorspace.
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("DISPLAY")
                        .size(11.0)
                        .color(theme::white(160)),
                );
                for (token, label) in [
                    ("auto", "AUTO"),
                    ("srgb", "SRGB"),
                    ("display-p3", "WIDE (P3)"),
                ] {
                    let selected = self.av.display_gamut == token;
                    if theme::glossy_button(ui, label, selected, Vec2::new(96.0, 26.0)).clicked()
                        && !selected
                    {
                        self.av.display_gamut = token.into();
                        self.save();
                    }
                }
            });

            applies_note(ui);
        });
    }

    /// A labelled 0..1 slider bound to a string config field that may be
    /// "auto"; dragging writes a number, AUTO restores the default.
    fn knob_row(
        &mut self,
        ui: &mut Ui,
        label: &str,
        _key: &str,
        range: std::ops::RangeInclusive<f32>,
        field: impl Fn(&mut input_config::AvConfig) -> &mut String,
        auto_value: impl Fn(&str) -> f32,
    ) {
        let screen_token = self.av.screen.clone();
        let is_auto = input_config::AvConfig::knob(field(&mut self.av)).is_none();
        let mut value = input_config::AvConfig::knob(field(&mut self.av))
            .unwrap_or_else(|| auto_value(&screen_token));
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(label.to_uppercase())
                    .size(11.0)
                    .color(theme::white(160)),
            );
            let slider = ui.add(
                egui::Slider::new(&mut value, range)
                    .show_value(true)
                    .fixed_decimals(2),
            );
            if slider.changed() {
                *field(&mut self.av) = format!("{value:.2}");
                self.save();
            }
            if theme::glossy_button(ui, "AUTO", is_auto, Vec2::new(64.0, 22.0)).clicked()
                && !is_auto
            {
                *field(&mut self.av) = "auto".into();
                self.save();
            }
        });
    }
}

/// Shared tail line: settings take effect on the next launch.
fn applies_note(ui: &mut Ui) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new("Applies the next time a cartridge is launched.")
            .size(10.0)
            .color(theme::white(100)),
    );
}
