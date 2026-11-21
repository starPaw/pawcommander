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
pub(super) enum Kind {
    Import,
    Export,
}

pub(super) struct Picker {
    panel: Retained<NSSavePanel>,
    result: mpsc::Receiver<NSModalResponse>,
    pub(super) kind: Kind,
}

impl Picker {
    pub(super) fn open(kind: Kind, ctx: &egui::Context) -> Result<Self, String> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| text!("Не удалось открыть окно выбора файла."))?;
        let panel = match kind {
            Kind::Import => Retained::into_super(NSOpenPanel::openPanel(mtm)),
            Kind::Export => {
                let panel = NSSavePanel::savePanel(mtm);
                panel.setNameFieldStringValue(&NSString::from_str("PawCommander-settings.json"));
                panel.setMessage(Some(&NSString::from_str(text!(
                    "Выберите имя нового файла настроек."
                ))));
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
            kind,
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
                        .ok_or_else(|| text!("Путь к файлу настроек недоступен.").into()),
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
