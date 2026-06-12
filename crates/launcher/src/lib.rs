//! gba-recomp frontend launcher: cartridge selection, input setup, A/V.
//!
//! One egui codebase for Linux/macOS/Windows (binary) and Android
//! (cdylib via the NativeActivity glue; build with the
//! `android-native-activity` feature).

mod app;
pub mod assets;
mod platform;
mod screens;
mod theme;

pub use app::LauncherApp;

pub fn native_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("gba-recomp")
            .with_inner_size([780.0, 540.0])
            .with_min_inner_size([580.0, 420.0])
            .with_icon(std::sync::Arc::new(assets::app_icon())),
        ..Default::default()
    }
}

pub fn run(options: eframe::NativeOptions) -> eframe::Result {
    eframe::run_native(
        "gba-launcher",
        options,
        Box::new(|cc| Ok(Box::new(LauncherApp::new(cc)))),
    )
}

/// Android entry point, called by the NativeActivity glue.
///
/// UNVERIFIED scaffolding: it compiles only for the Android target and has
/// not been exercised on a device yet (needs the NDK toolchain and an apk
/// wrapper; see docs/launcher.md).
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    let mut options = native_options();
    options.android_app = Some(app);
    let _ = run(options);
}
