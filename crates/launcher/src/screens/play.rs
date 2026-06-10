//! ROM selection and launch.

use eframe::egui::Ui;

use crate::theme;

pub struct PlayScreen {}

impl PlayScreen {
    pub fn new() -> Self {
        Self {}
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        theme::glass_frame().show(ui, |ui| {
            theme::section_title(ui, "Cartridges");
            ui.label("Coming online…");
        });
    }
}
