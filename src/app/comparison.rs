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
    return_focus: Option<egui::Id>,
}

pub(super) enum ComparisonBody {
    Folders(DirectoryComparison),
    Files(FileComparisonDialog),
    #[cfg(target_os = "macos")]
    Duplicates(super::duplicates::DuplicateSearch),
    #[cfg(target_os = "macos")]
    Usage(super::disk_usage::DiskUsage),
    #[cfg(target_os = "macos")]
    Volumes,
    #[cfg(target_os = "macos")]
    SettingsTransfer(Box<super::settings_transfer::SettingsTransfer>),
    #[cfg(target_os = "macos")]
    Collections(Box<super::collections::CollectionsDialog>),
    #[cfg(target_os = "macos")]
    NewFile(Box<super::file_templates::FileTemplatesDialog>),
    #[cfg(target_os = "macos")]
    CompatibleNames(Box<super::compatible_names::CompatibleNamesDialog>),
    #[cfg(target_os = "macos")]
    Sync(Box<super::folder_sync::FolderSyncDialog>),
}

pub(super) enum ComparisonResponse {
    Open,
    Close,
    #[cfg(target_os = "macos")]
    ApplySync(super::folder_sync::Plan),
    #[cfg(target_os = "macos")]
    ApplyCompatibleNames(Box<crate::vfs::compatible_names::Plan>),
    #[cfg(target_os = "macos")]
    TrashDuplicates(Box<crate::vfs::duplicates::TrashPlan>),
    #[cfg(target_os = "macos")]
    RevealItem(Location),
    #[cfg(target_os = "macos")]
    OpenVolume(std::path::PathBuf),
    #[cfg(target_os = "macos")]
    EjectVolume(crate::volumes::EjectTarget),
    #[cfg(target_os = "macos")]
    ExportSettings(
        std::path::PathBuf,
        Box<super::settings_transfer::Preferences>,
    ),
    #[cfg(target_os = "macos")]
    ImportSettings(Box<super::settings_transfer::Preferences>),
    #[cfg(target_os = "macos")]
    CreateFile(
        crate::vfs::file_templates::NewFile,
        mpsc::Sender<Result<(), String>>,
    ),
}

impl ComparisonDialog {
    pub(super) fn new(
        body: ComparisonBody,
        return_focus: Option<egui::Id>,
        ctx: &egui::Context,
    ) -> Self {
        Self {
            body,
            return_focus: return_focus.or_else(|| ctx.memory(|memory| memory.focused())),
        }
    }

    pub(super) fn poll(&mut self) {
        match &mut self.body {
            ComparisonBody::Folders(view) => view.poll(),
            ComparisonBody::Files(view) => view.poll(),
            #[cfg(target_os = "macos")]
            ComparisonBody::Duplicates(view) => view.poll(),
            #[cfg(target_os = "macos")]
            ComparisonBody::Usage(view) => view.poll(),
            #[cfg(target_os = "macos")]
            ComparisonBody::Volumes => {}
            #[cfg(target_os = "macos")]
            ComparisonBody::SettingsTransfer(view) => view.poll(),
            #[cfg(target_os = "macos")]
            ComparisonBody::Collections(view) => view.poll(),
            #[cfg(target_os = "macos")]
            ComparisonBody::NewFile(view) => view.poll(),
            #[cfg(target_os = "macos")]
            ComparisonBody::CompatibleNames(view) => view.poll(),
            #[cfg(target_os = "macos")]
            ComparisonBody::Sync(view) => view.poll(),
        }
    }

    fn show(
        &mut self,
        ctx: &egui::Context,
        #[cfg(target_os = "macos")] profiles: &mut Vec<super::sync_profiles::SyncProfile>,
        #[cfg(target_os = "macos")] catalog: &mut crate::vfs::collections::Catalog,
        #[cfg(target_os = "macos")] templates: &mut crate::vfs::file_templates::Library,
        #[cfg(target_os = "macos")] volumes: &mut super::volumes::VolumeBrowser,
        #[cfg(target_os = "macos")] file_work: bool,
    ) -> ComparisonResponse {
        let open = match &mut self.body {
            ComparisonBody::Folders(view) => view.show(ctx),
            ComparisonBody::Files(view) => view.show(ctx),
            #[cfg(target_os = "macos")]
            ComparisonBody::Duplicates(view) => return view.show(ctx),
            #[cfg(target_os = "macos")]
            ComparisonBody::Usage(view) => view.show(ctx),
            #[cfg(target_os = "macos")]
            ComparisonBody::Volumes => return volumes.show(ctx, file_work),
            #[cfg(target_os = "macos")]
            ComparisonBody::SettingsTransfer(view) => return view.show(ctx),
            #[cfg(target_os = "macos")]
            ComparisonBody::Collections(view) => return view.show(ctx, catalog),
            #[cfg(target_os = "macos")]
            ComparisonBody::NewFile(view) => return view.show(ctx, templates),
            #[cfg(target_os = "macos")]
            ComparisonBody::CompatibleNames(view) => return view.show(ctx),
            #[cfg(target_os = "macos")]
            ComparisonBody::Sync(view) => return view.show(ctx, profiles),
        };
        if open {
            ComparisonResponse::Open
        } else {
            ComparisonResponse::Close
        }
    }
}

impl PawCommander {
    pub(super) fn show_comparison(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "macos")]
        let file_work = self.volume_file_work_active();
        let Some(comparison) = &mut self.comparison else {
            return;
        };
        #[cfg(target_os = "macos")]
        let response = comparison.show(
            ctx,
            &mut self.sync_profiles,
            &mut self.collections,
            &mut self.file_templates,
            &mut self.volumes,
            file_work,
        );
        #[cfg(not(target_os = "macos"))]
        let response = comparison.show(ctx);
        #[cfg(target_os = "macos")]
        if let ComparisonResponse::CreateFile(job, completion) = response {
            self.enqueue_new_file(job, completion, ctx);
            return;
        }
        #[cfg(target_os = "macos")]
        if let ComparisonResponse::EjectVolume(target) = response {
            self.eject_volume(target, ctx);
            return;
        }
        if matches!(response, ComparisonResponse::Open) {
            return;
        }
        self.palette_focus_to_restore = comparison.return_focus;
        self.comparison = None;
        #[cfg(target_os = "macos")]
        match response {
            ComparisonResponse::ApplySync(plan) => self.enqueue_folder_sync(plan, ctx),
            ComparisonResponse::ApplyCompatibleNames(plan) => {
                self.enqueue_compatible_names(*plan, ctx)
            }
            ComparisonResponse::TrashDuplicates(plan) => self.enqueue_duplicate_trash(*plan, ctx),
            ComparisonResponse::RevealItem(path) => self.reveal_search_result(path, ctx),
            ComparisonResponse::OpenVolume(path) => {
                self.action(super::Action::Navigate(self.active, path.into()), ctx)
            }
            ComparisonResponse::ExportSettings(path, preferences) => {
                self.export_settings(path, *preferences, ctx)
            }
            ComparisonResponse::ImportSettings(preferences) => {
                self.import_settings(*preferences, ctx)
            }
            _ => {}
        }
        ctx.request_repaint();
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
                "Could not start comparison: {error}",
                error = error
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
                Self::Failed(text!("Сравнение неожиданно завершилось").into())
            }
        };
    }
}

pub(super) fn show_pair(
    ui: &mut egui::Ui,
    pair: &[Location; 2],
    titles: [crate::i18n::MessageKey; 2],
) {
    for (title, location) in titles.into_iter().zip(pair) {
        ui.label(RichText::new(text(title)).weak());
        let path = location.display().escape_debug().to_string();
        ui.add(egui::Label::new(&path).truncate().selectable(true))
            .on_hover_text(&path);
    }
    ui.add_space(8.0);
}

pub(super) fn show_progress<T>(
    ui: &mut egui::Ui,
    worker: &mut Worker<T>,
    message: crate::i18n::MessageKey,
) {
    show_progress_with_stop(
        ui,
        worker,
        message,
        crate::i18n::key!("Останавливаем сравнение…"),
    );
}

pub(super) fn show_progress_with_stop<T>(
    ui: &mut egui::Ui,
    worker: &mut Worker<T>,
    running: crate::i18n::MessageKey,
    stopping: crate::i18n::MessageKey,
) {
    let message = if worker.progress.cancel.is_requested() {
        stopping
    } else {
        running
    };
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(text(message));
    });
    super::footer::operation_details(ui, worker.progress.snapshot(), false);
    ui.ctx().request_repaint_after(Duration::from_millis(100));
}
