#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod address;
mod app;
mod archive;
#[cfg(target_os = "macos")]
mod cli;
mod command_palette;
mod conflict;
mod credentials;
mod directory_tree;
mod drag_drop;
mod file_clipboard;
mod folder_jump;
mod fs_ops;
mod i18n;
mod icons;
mod list_columns;
#[cfg(target_os = "macos")]
mod macos_about;
mod name_match;
mod native_drag;
mod operations;
mod preview;
mod properties;
mod recovery;
mod result_history;
mod search;
mod shortcuts;
#[cfg(target_os = "macos")]
mod smb;
mod ssh_config;
mod tab_drag;
mod terminal;
#[cfg(test)]
mod test_ui;
mod theme;
mod thumbnails;
mod tools;
mod vfs;
#[cfg(target_os = "macos")]
mod volumes;
#[cfg(windows)]
mod windows_file_id;
mod zed;

const APP_ID: &str = "dev.pawcommander.filemanager";

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    let mut arguments = std::env::args_os().skip(1).peekable();
    if let Some(mode) = arguments.peek().and_then(|arg| arg.to_str()) {
        match mode {
            "--paw-drag-escape-listener" => std::process::exit(native_drag::listen_for_escape()),
            preview::PDF_WORKER_ARG => std::process::exit(preview::run_pdf_worker()),
            preview::MEDIA_WORKER_ARG => std::process::exit(preview::run_media_worker()),
            _ => {}
        }
    }
    cli::run(arguments)
}

#[cfg(not(target_os = "macos"))]
fn main() -> eframe::Result {
    let folder = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    run_gui(folder.into(), None)
}

fn run_gui(start: app::Startup, persistence_path: Option<std::path::PathBuf>) -> eframe::Result {
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 860.0])
        .with_min_inner_size([960.0, 600.0])
        .with_icon(
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                .expect("bundled app icon"),
        )
        .with_app_id(APP_ID);
    #[cfg(target_os = "macos")]
    let viewport = viewport
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    let options = eframe::NativeOptions {
        viewport,
        persistence_path,
        // macOS file paste needs the modifiers from the press, including Option.
        // The app restores text clipboard events in the root and Quick Look viewports.
        clipboard_shortcuts: !cfg!(target_os = "macos"),
        ..Default::default()
    };
    eframe::run_native(
        "PawCommander",
        options,
        Box::new(move |cc| {
            #[cfg(target_os = "macos")]
            macos_about::install(&cc.egui_ctx);
            Ok(Box::new(app::PawCommander::start(cc, start)))
        }),
    )
}
