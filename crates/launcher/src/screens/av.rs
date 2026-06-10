//! Video / audio settings — not built yet; wears the classic stripes.

use eframe::egui::{Rect, Sense, Ui, Vec2};

use crate::theme;

pub struct AvScreen {}

impl AvScreen {
    pub fn new() -> Self {
        Self {}
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        theme::glass_frame().show(ui, |ui| {
            theme::section_title(ui, "Video / Audio");
            ui.add_space(8.0);
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), 46.0),
                Sense::hover(),
            );
            theme::hazard_stripes(ui.painter(), Rect::from_min_size(rect.min, rect.size()));
            ui.add_space(8.0);
            ui.label("UNDER CONSTRUCTION — scaling, filters, and audio mix land here.");
        });
    }
}
