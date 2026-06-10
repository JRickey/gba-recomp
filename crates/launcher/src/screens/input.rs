//! Input device selection and button rebinding.

use eframe::egui::Ui;

use crate::theme;

pub struct InputScreen {}

impl InputScreen {
    pub fn new() -> Self {
        Self {}
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        theme::glass_frame().show(ui, |ui| {
            theme::section_title(ui, "Controllers");
            ui.label("Coming online…");
        });
    }
}
