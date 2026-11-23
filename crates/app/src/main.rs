// Release builds are a proper Windows GUI app (no console window).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod clips_panel;
mod settings;
mod theme;
mod util;
mod video_gl;
mod views;

use eframe::egui;

fn main() -> eframe::Result {
    let (cfg, warnings) = tapestry_core::Config::load();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Tapestry")
        .with_inner_size([1360.0, 820.0])
        .with_min_inner_size([720.0, 440.0])
        .with_icon(theme::window_icon());
    if cfg.always_on_top {
        viewport = viewport.with_window_level(egui::WindowLevel::AlwaysOnTop);
    }

    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Tapestry", options, Box::new(move |cc| Ok(Box::new(app::TapestryApp::new(cc, cfg, warnings)))))
}
