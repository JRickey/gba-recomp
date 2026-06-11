//! gba-recomp frontend launcher.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod assets;
mod platform;
mod screens;
mod theme;

#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("gba-recomp")
            .with_inner_size([780.0, 540.0])
            .with_min_inner_size([580.0, 420.0])
            .with_icon(std::sync::Arc::new(assets::app_icon())),
        ..Default::default()
    };
    eframe::run_native(
        "gba-launcher",
        options,
        Box::new(|cc| Ok(Box::new(app::LauncherApp::new(cc)))),
    )
}
