mod wayland;
mod x11;

use eframe::egui;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use std::path::PathBuf;

enum Backend {
    X11 {
        display: usize,
        window: u32,
        drag: Option<x11::Drag>,
    },
    Wayland(wayland::Client),
}

#[derive(Default)]
pub struct Platform {
    backend: Option<Backend>,
    error: Option<String>,
}

impl Platform {
    pub fn prepare(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if self.backend.is_some() || self.error.is_some() {
            return;
        }
        let (Ok(display), Ok(window)) = (frame.display_handle(), frame.window_handle()) else {
            return;
        };
        let backend = match (display.as_raw(), window.as_raw()) {
            (RawDisplayHandle::Xlib(d), RawWindowHandle::Xlib(w)) => d
                .display
                .map(|display| Backend::X11 {
                    display: display.as_ptr() as usize,
                    window: w.window as u32,
                    drag: None,
                })
                .ok_or_else(|| "X11 display is unavailable".to_owned()),
            (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) => {
                // SAFETY: eframe owns both handles. Client::drop stops/joins its
                // worker and destroys its objects before the app/window is dropped.
                unsafe { wayland::Client::new(d.display.as_ptr(), w.surface.as_ptr(), ctx.clone()) }
                    .map(Backend::Wayland)
            }
            _ => Err("Unsupported Linux window backend".into()),
        };
        match backend {
            Ok(backend) => self.backend = Some(backend),
            Err(error) => self.error = Some(error),
        }
    }

    pub fn start(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        paths: Vec<PathBuf>,
    ) -> Result<(), String> {
        self.prepare(ctx, frame);
        let uris = uri_list(&paths)?;
        match self.backend.as_mut() {
            Some(Backend::X11 {
                display,
                window,
                drag,
            }) => {
                *drag = Some(x11::Drag::start(*display, *window, uris, ctx.clone())?);
                Ok(())
            }
            Some(Backend::Wayland(client)) => client.start(uris),
            None => Err(native_error(
                self.error.as_deref().unwrap_or("No native window handle"),
            )),
        }
    }

    pub fn poll(&mut self) -> Option<Result<bool, String>> {
        match self.backend.as_mut()? {
            Backend::X11 { drag, .. } => {
                let result = drag.as_mut()?.poll()?;
                *drag = None;
                Some(result)
            }
            Backend::Wayland(client) => client.poll(),
        }
        .map(|result| result.map_err(|error| native_error(&error)))
    }
}

fn native_error(error: &str) -> String {
    crate::tr_format!(
        "Could not hand files to the receiving application: {error}",
        error = error
    )
}

fn uri_list(paths: &[PathBuf]) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    for path in paths {
        let url = url::Url::from_file_path(path)
            .map_err(|_| crate::i18n::text!("Недопустимый путь файла"))?;
        data.extend_from_slice(url.as_str().as_bytes());
        data.extend_from_slice(b"\r\n");
        if data.len() > 4 * 1024 * 1024 {
            return Err(crate::tr_format!("The file list is too large to drag"));
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn uri_payload_keeps_unicode_spaces_delimiters_and_non_utf8_paths() {
        let paths = vec![
            PathBuf::from("/tmp/Привет #100% two words.txt"),
            PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/raw-\xff".to_vec())),
        ];
        let bytes = uri_list(&paths).unwrap();
        let decoded: Vec<_> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| url::Url::parse(line).unwrap().to_file_path().unwrap())
            .collect();
        assert_eq!(decoded, paths);
        assert!(bytes.ends_with(b"\r\n"));
    }
}
