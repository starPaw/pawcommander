use super::{
    directory_compare::DirectoryComparison, file_compare::FileComparisonDialog, PawCommander,
};
use crate::{
    i18n::text,
    operations::{Progress, ProgressView, TransferError},
    vfs::Location,
};
use eframe::egui::{self, RichText};
use std::{sync::mpsc, thread, time::Duration};

pub(super) struct ComparisonDialog {
    pub(super) body: ComparisonBody,
    previous_focus: Option<egui::Id>,
}

pub(super) enum ComparisonBody {
    Folders(DirectoryComparison),
    Files(FileComparisonDialog),
}

impl ComparisonDialog {
    pub(super) fn new(body: ComparisonBody, ctx: &egui::Context) -> Self {
        Self {
            body,
            previous_focus: ctx.memory(|memory| memory.focused()),
        }
    }

    pub(super) fn poll(&mut self) {
        match &mut self.body {
            ComparisonBody::Folders(view) => view.poll(),
            ComparisonBody::Files(view) => view.poll(),
        }
    }

    fn show(&mut self, ctx: &egui::Context) -> bool {
        match &mut self.body {
            ComparisonBody::Folders(view) => view.show(ctx),
            ComparisonBody::Files(view) => view.show(ctx),
        }
    }
}

impl PawCommander {
    pub(super) fn show_comparison(&mut self, ctx: &egui::Context) {
        if let Some(comparison) = &mut self.comparison
            && !comparison.show(ctx)
        {
            self.palette_focus_to_restore = comparison.previous_focus;
            self.comparison = None;
            ctx.request_repaint();
        }
    }
}

pub(super) enum Phase<T> {
    Ready,
    Running(Worker<T>),
    Finished(T),
    Failed(String),
    Stopped,
}

pub(super) struct Worker<T> {
    result: mpsc::Receiver<Result<T, TransferError>>,
    pub(super) progress: ProgressView,
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.progress.cancel.request();
    }
}

impl<T: Send + 'static> Phase<T> {
    pub(super) fn start(
        ctx: &egui::Context,
        work: impl FnOnce(&mut Progress) -> Result<T, TransferError> + Send + 'static,
    ) -> Self {
        let (send, result) = mpsc::channel();
        let (mut reporter, progress) = Progress::tracked();
        let ctx = ctx.clone();
        let started = thread::Builder::new()
            .name("pawcommander-compare".into())
            .spawn(move || {
                let _ = send.send(work(&mut reporter));
                ctx.request_repaint();
            });
        match started {
            Ok(_) => Self::Running(Worker { result, progress }),
            Err(error) => Self::Failed(crate::tr_format!(
                "Не удалось запустить сравнение: {error}",
                "Could not start comparison: {error}"
            )),
        }
    }

    pub(super) fn poll(&mut self) {
        let Self::Running(worker) = self else {
            return;
        };
        *self = match worker.result.try_recv() {
            Ok(Ok(result)) => Self::Finished(result),
            Ok(Err(TransferError::Cancelled)) => Self::Stopped,
            Ok(Err(error)) => Self::Failed(error.to_string()),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Self::Failed(text("Сравнение неожиданно завершилось").into())
            }
        };
    }
}

pub(super) fn show_pair(ui: &mut egui::Ui, pair: &[Location; 2]) {
    for (title, location) in ["Левая панель", "Правая панель"].into_iter().zip(pair)
    {
        ui.label(RichText::new(text(title)).weak());
        let path = location.display().escape_debug().to_string();
        ui.add(egui::Label::new(&path).truncate().selectable(true))
            .on_hover_text(&path);
    }
    ui.add_space(8.0);
}

pub(super) fn show_progress<T>(ui: &mut egui::Ui, worker: &mut Worker<T>, message: &'static str) {
    let stopping = worker.progress.cancel.is_requested();
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(text(if stopping {
            "Останавливаем сравнение…"
        } else {
            message
        }));
    });
    super::footer::operation_details(ui, worker.progress.snapshot(), false);
    ui.ctx().request_repaint_after(Duration::from_millis(100));
}
