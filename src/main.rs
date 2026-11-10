// Release builds on Windows run without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod clean;
mod fmt;
mod platform;
mod rules;
mod scan;
mod settings;
mod space;
mod theme;

use eframe::egui;

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn main() {
    platform::init_process();
    let icon = eframe::icon_data::from_png_bytes(ICON_PNG).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Cleaner")
        .with_app_id(rules::APP_ID)
        .with_inner_size([1040.0, 680.0])
        .with_min_inner_size([800.0, 500.0]);
    // Inside Cleaner.app the Dock uses the sharper icon from the bundle. An empty icon
    // stops eframe from replacing it with its own default icon.
    let window_icon = if platform::running_in_bundle() {
        Some(egui::IconData::default())
    } else {
        icon.clone()
    };
    if let Some(i) = window_icon {
        viewport = viewport.with_icon(i);
    }
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        centered: true,
        ..Default::default()
    };
    let result = eframe::run_native(
        "Cleaner",
        options,
        Box::new(move |cc| Ok(Box::new(app::CleanerApp::new(cc, icon)))),
    );
    if let Err(e) = result {
        platform::fatal(&format!(
            "Cleaner could not open its window.\n\n{e}\n\nCleaner needs a graphics driver with OpenGL 2 or later."
        ));
    }
}
