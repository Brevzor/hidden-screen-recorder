#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod config;
mod debug_log;
mod hotkeys;
mod i18n;
mod recorder;
mod window;

fn main() -> eframe::Result<()> {
    let (config, warning) = match config::AppConfig::load() {
        Ok(loaded) => loaded,
        Err(error) => (config::AppConfig::default(), Some(error)),
    };
    debug_log::set_enabled(config.logging_enabled);
    debug_log::reset();
    debug_log::write("hidden-screen-recorder started");
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size(app::WINDOW_SIZE)
        .with_min_inner_size(app::MIN_WINDOW_SIZE)
        .with_title(app::WINDOW_TITLE)
        .with_resizable(true);

    eframe::run_native(
        app::WINDOW_TITLE,
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(move |cc| {
            Ok(Box::new(app::HiddenScreenRecorderApp::new(
                cc, config, warning,
            )))
        }),
    )
}
