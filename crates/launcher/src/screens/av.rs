//! Audio / video settings. Audio holds the first premium enhancement
//! toggle (unnamed until the feature is complete); video still wears
//! the classic stripes.

use eframe::egui::{self, Rect, Sense, Ui, Vec2};

use crate::theme;

pub struct AvScreen {
    av: input_config::AvConfig,
}

impl AvScreen {
    pub fn new() -> Self {
        Self { av: input_config::AvConfig::load() }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        theme::glass_frame().show(ui, |ui| {
            theme::section_title(ui, "Audio");
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                let label = if self.av.audio_enhanced {
                    "ENHANCED AUDIO \u{2014} ON"
                } else {
                    "ENHANCED AUDIO \u{2014} OFF"
                };
                if theme::glossy_button(ui, label, self.av.audio_enhanced,
                    Vec2::new(240.0, 34.0)).clicked()
                {
                    self.av.audio_enhanced = !self.av.audio_enhanced;
                    if let Err(e) = self.av.save() {
                        eprintln!("av.cfg: {e}");
                    }
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
            ui.label(
                egui::RichText::new("Applies the next time a cartridge is launched.")
                    .size(10.0)
                    .color(theme::white(100)),
            );

            ui.add_space(14.0);
            theme::section_title(ui, "Video");
            ui.add_space(6.0);
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), 46.0),
                Sense::hover(),
            );
            theme::hazard_stripes(ui.painter(), Rect::from_min_size(rect.min, rect.size()));
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    "UNDER CONSTRUCTION \u{2014} original-screen color, response \
                     blending, and pixel-grid options land here.",
                )
                .size(11.0)
                .color(theme::white(140)),
            );
        });
    }
}
