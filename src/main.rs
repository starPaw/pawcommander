#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod address;
mod app;
mod conflict;
mod credentials;
mod drag_drop;
mod file_clipboard;
mod fs_ops;
mod i18n;
mod icons;
#[cfg(target_os = "macos")]
mod macos_about;
mod native_drag;
mod operations;
mod preview;
mod properties;
mod search;
mod tab_drag;
mod theme;
mod tools;
mod vfs;
mod zed;

fn main() -> eframe::Result {
    let start = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 860.0])
        .with_min_inner_size([960.0, 600.0])
        .with_icon(
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                .expect("bundled app icon"),
        )
        .with_app_id("dev.pawcommander.filemanager");
    #[cfg(target_os = "macos")]
    let viewport = viewport
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "PawCommander",
        options,
        Box::new(move |cc| {
            #[cfg(target_os = "macos")]
            macos_about::install();
            Ok(Box::new(app::PawCommander::new(cc, start)))
        }),
    )
}
