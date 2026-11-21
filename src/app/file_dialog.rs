use crate::i18n::text;
use eframe::egui;
use objc2::{rc::Retained, MainThreadMarker};
use objc2_app_kit::{
    NSModalResponse, NSModalResponseCancel, NSModalResponseOK, NSOpenPanel, NSSavePanel,
};
use objc2_foundation::NSString;
use std::{
    ffi::{CStr, OsStr},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    sync::mpsc,
};

#[derive(Clone, Copy)]
pub(super) enum Mode {
    Open,
    Save {
        suggested_name: &'static str,
        message: &'static str,
    },
}

pub(super) struct Picker {
    panel: Retained<NSSavePanel>,
    result: mpsc::Receiver<NSModalResponse>,
    pub(super) mode: Mode,
}

impl Picker {
    pub(super) fn open(mode: Mode, ctx: &egui::Context) -> Result<Self, String> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| text!("Не удалось открыть окно выбора файла."))?;
        let panel = match mode {
            Mode::Open => Retained::into_super(NSOpenPanel::openPanel(mtm)),
            Mode::Save {
                suggested_name,
                message,
            } => {
                let panel = NSSavePanel::savePanel(mtm);
                panel.setNameFieldStringValue(&NSString::from_str(suggested_name));
                panel.setMessage(Some(&NSString::from_str(message)));
                panel
            }
        };
        let (send, result) = mpsc::channel();
        let ctx = ctx.clone();
        let completion = block2::RcBlock::new(move |response| {
            let _ = send.send(response);
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        });
        panel.beginWithCompletionHandler(&completion);
        Ok(Self {
            panel,
            result,
            mode,
        })
    }

    pub(super) fn poll(&self) -> Option<Result<Option<PathBuf>, String>> {
        match self.result.try_recv() {
            Ok(response) if response == NSModalResponseOK => {
                let path = self.panel.URL().filter(|url| url.isFileURL()).map(|url| {
                    // SAFETY: NSURL supplies a NUL-terminated buffer valid while the URL is alive.
                    let bytes = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) };
                    PathBuf::from(OsStr::from_bytes(bytes.to_bytes()))
                });
                Some(
                    path.map(Some)
                        .ok_or_else(|| text!("Путь к выбранному файлу недоступен.").into()),
                )
            }
            Ok(response) if response == NSModalResponseCancel => Some(Ok(None)),
            Err(mpsc::TryRecvError::Empty) => None,
            Ok(_) | Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(text!("Не удалось открыть окно выбора файла.").into()))
            }
        }
    }
}
