use super::{
    comparison::{ComparisonBody, ComparisonDialog, ComparisonResponse},
    file_dialog::{Mode, Picker},
    PawCommander,
};
use crate::{
    extensions::{Catalog, Extension, Preview},
    i18n::text,
    theme::paint_focus,
};
use eframe::egui;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
};

pub(super) struct Extensions {
    pub(super) catalog: Catalog,
    running: Arc<AtomicBool>,
}

impl Extensions {
    pub(super) fn restore(installed: Vec<Arc<Extension>>) -> Self {
        Self {
            catalog: Catalog::restore(installed),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub(super) struct ExtensionsDialog {
    catalog: Catalog,
    file: Option<PathBuf>,
    running: Arc<AtomicBool>,
    pending: Option<Pending>,
    preview: Option<(Arc<Extension>, Preview)>,
    error: Option<String>,
    catalog_changed: bool,
    ctx: egui::Context,
}

enum Pending {
    Picker(Picker),
    Worker(mpsc::Receiver<Result<Reply, String>>),
}

enum Reply {
    Imported(Arc<Extension>),
    Viewed(Arc<Extension>, Preview),
}

struct WorkerSlot(Arc<AtomicBool>);

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl ExtensionsDialog {
    pub(super) fn poll(&mut self) {
        match &self.pending {
            Some(Pending::Picker(picker)) => {
                let Some(result) = picker.poll() else { return };
                self.pending = None;
                match result {
                    Ok(Some(path)) => {
                        self.start(move || Extension::load(&path).map(Reply::Imported))
                    }
                    Ok(None) => {}
                    Err(error) => self.error = Some(error),
                }
            }
            Some(Pending::Worker(worker)) => {
                let result = match worker.try_recv() {
                    Ok(result) => result,
                    Err(mpsc::TryRecvError::Empty) => return,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Err(text!("Задача расширения завершилась без результата.").into())
                    }
                };
                self.pending = None;
                match result {
                    Ok(Reply::Imported(extension)) => match self.catalog.install(extension) {
                        Ok(()) => self.catalog_changed = true,
                        Err(error) => self.error = Some(error),
                    },
                    Ok(Reply::Viewed(extension, preview)) => {
                        self.preview = Some((extension, preview))
                    }
                    Err(error) => self.error = Some(error),
                }
            }
            None => {}
        }
    }

    fn start(&mut self, work: impl FnOnce() -> Result<Reply, String> + Send + 'static) {
        self.error = None;
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.error = Some(text!("Дождитесь завершения предыдущей задачи расширения.").into());
            return;
        }
        let slot = WorkerSlot(Arc::clone(&self.running));
        let ctx = self.ctx.clone();
        let (sender, receiver) = mpsc::channel();
        match thread::Builder::new()
            .name("pawcommander-viewer".into())
            .spawn(move || {
                let result = work();
                drop(slot);
                let _ = sender.send(result);
                ctx.request_repaint_of(egui::ViewportId::ROOT);
            }) {
            Ok(_) => self.pending = Some(Pending::Worker(receiver)),
            Err(error) => {
                self.error = Some(crate::tr_format!(
                    "Could not start the viewer worker ({error}).",
                    error = error
                ))
            }
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> ComparisonResponse {
        let mut imported = false;
        let mut remove = None;
        let mut run = None;
        let mut back = false;
        let mut close = false;
        let picking = matches!(self.pending, Some(Pending::Picker(_)));
        let idle = self.pending.is_none();
        let modal = egui::Modal::new(egui::Id::unique("viewer-extensions")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(240.0, 550.0));
            if let Some((extension, preview)) = &self.preview {
                ui.heading(extension.name());
                if preview.truncated {
                    ui.label(text!("Результат для первых 32 КиБ файла."));
                }
                egui::ScrollArea::vertical()
                    .max_height(ctx.content_rect().height() * 0.55)
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(&preview.text).wrap().selectable(true));
                    });
                back = button(ui, true, text!("Назад"));
                return;
            }
            ui.heading(if self.file.is_some() {
                text!("Предпросмотр с расширением")
            } else {
                text!("Расширения просмотра")
            });
            if let Some(file) = &self.file {
                let path = file.to_string_lossy().escape_debug().to_string();
                ui.add(egui::Label::new(&path).selectable(true).wrap());
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            match &self.pending {
                Some(Pending::Picker(_)) => {
                    ui.label(text!("Выберите файл в окне macOS."));
                }
                Some(Pending::Worker(_)) => {
                    ui.label(text!("Выполняется задача расширения…"));
                }
                None => {}
            }
            egui::ScrollArea::vertical()
                .max_height(ctx.content_rect().height() * 0.5)
                .show(ui, |ui| {
                    let mut visible = false;
                    for extension in self.catalog.entries() {
                        if self
                            .file
                            .as_ref()
                            .is_some_and(|path| !extension.matches(path))
                        {
                            continue;
                        }
                        visible = true;
                        ui.push_id(extension.id(), |ui| {
                            ui.separator();
                            ui.strong(extension.name());
                            if self.file.is_none() {
                                ui.small(extension.extensions().join(", "));
                            }
                            ui.label(if extension.reads_file() {
                                text!("Читает первые 32 КиБ выбранного файла")
                            } else {
                                text!("Работает без чтения файла")
                            });
                            if self.file.is_some() {
                                if button(ui, idle, text!("Запустить просмотрщик"))
                                {
                                    run = Some(Arc::clone(extension));
                                }
                            } else if !extension.is_builtin()
                                && button(ui, idle, text!("Удалить расширение"))
                            {
                                remove = Some(extension.id().to_owned());
                            }
                        });
                    }
                    if !visible {
                        ui.label(text!("Для этого файла нет просмотрщика."));
                    }
                });
            ui.horizontal_wrapped(|ui| {
                if self.file.is_none() {
                    imported = button(ui, idle, text!("Добавить расширение…"));
                }
                close = button(ui, !picking, text!("Закрыть"));
            });
        });
        if back {
            self.preview = None;
        }
        if let Some(id) = remove {
            self.catalog.remove(&id);
            self.catalog_changed = true;
        }
        if imported {
            self.error = None;
            match Picker::open(Mode::Open, ctx) {
                Ok(picker) => self.pending = Some(Pending::Picker(picker)),
                Err(error) => self.error = Some(error),
            }
        }
        if let (Some(extension), Some(path)) = (run, self.file.clone()) {
            self.start(move || {
                let preview = extension.preview(&path)?;
                Ok(Reply::Viewed(extension, preview))
            });
        }
        if std::mem::take(&mut self.catalog_changed) {
            return ComparisonResponse::ViewerExtensionsChanged(self.catalog.clone());
        }
        if !picking && (close || modal.should_close()) {
            ComparisonResponse::Close
        } else {
            ComparisonResponse::Open
        }
    }
}

fn button(ui: &mut egui::Ui, enabled: bool, label: &str) -> bool {
    let response = ui.add_enabled(enabled, egui::Button::new(label));
    paint_focus(ui, &response, 4);
    response.clicked()
}

impl PawCommander {
    pub(super) fn open_extensions(
        &mut self,
        file: Option<PathBuf>,
        return_focus: Option<egui::Id>,
        ctx: &egui::Context,
    ) {
        if self.comparison.is_some() || self.conflict_open() {
            return;
        }
        let view = ExtensionsDialog {
            catalog: self.extensions.catalog.clone(),
            file,
            running: Arc::clone(&self.extensions.running),
            pending: None,
            preview: None,
            error: None,
            catalog_changed: false,
            ctx: ctx.clone(),
        };
        self.comparison = Some(ComparisonDialog::new(
            ComparisonBody::ViewerExtensions(Box::new(view)),
            return_focus,
            ctx,
        ));
    }
}
