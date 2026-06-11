//! Desktop entry point; Android enters through `android_main` in the lib.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result {
    gba_launcher::run(gba_launcher::native_options())
}

#[cfg(target_os = "android")]
fn main() {}
