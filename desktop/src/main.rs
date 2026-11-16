// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod camera;
mod config;
mod devices;
mod input;
mod jobs;
mod player;
mod recorder;

fn main() -> eframe::Result {
    let paths = config::Paths::new().expect("tidak bisa membuat folder data Reka");
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../icons/128x128.png")).ok();
    let mut vp = eframe::egui::ViewportBuilder::default()
        .with_title("Reka")
        .with_app_id("reka")
        .with_inner_size([480.0, 760.0])
        .with_min_inner_size([1.0, 1.0])
        // Needed so the recording bar and camera bubble can be see-through; the panels paint their own background.
        .with_transparent(true);
    if let Some(i) = icon {
        vp = vp.with_icon(i);
    }
    eframe::run_native(
        "Reka",
        eframe::NativeOptions { viewport: vp, centered: true, ..Default::default() },
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, paths)))),
    )
}
