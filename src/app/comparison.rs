use super::{
    directory_compare::DirectoryComparison, file_compare::FileComparisonDialog, PawCommander,
};
use crate::{
    i18n::text,
    operations::{Progress, ProgressView, TransferError},
    vfs::Location,
};
use eframe::egui::{self, RichText};
use std::{io, sync::mpsc, thread, time::Duration};

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
    ViewerExtensions(Box<super::extensions::ExtensionsDialog>),
    #[cfg(target_os = "macos")]
    Diagnostics(Box<super::diagnostics::DiagnosticDialog>),
    #[cfg(target_os = "macos")]
    Help(Box<super::help::HelpDialog>),
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
    OpenCommandPalette(String),
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
    ViewerExtensionsChanged(crate::extensions::Catalog),
    #[cfg(target_os = "macos")]
    ExportDiagnostic(
        crate::file_error::ExportTarget,
        String,
        mpsc::Sender<Result<(), TransferError>>,
    ),
    #[cfg(target_os = "macos")]
    CreateFile(
        crate::vfs::file_templates::NewFile,
        mpsc::Sender<std::sync::Arc<super::file_templates::CreationResult>>,
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
            ComparisonBody::ViewerExtensions(view) => view.poll(),
            #[cfg(target_os = "macos")]
            ComparisonBody::Diagnostics(view) => view.poll(),
            #[cfg(target_os = "macos")]
            ComparisonBody::Help(_) => {}
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
            ComparisonBody::ViewerExtensions(view) => return view.show(ctx),
            #[cfg(target_os = "macos")]
            ComparisonBody::Diagnostics(view) => return view.show(ctx),
            #[cfg(target_os = "macos")]
            ComparisonBody::Help(view) => return view.show(ctx),
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
        #[cfg(target_os = "macos")]
        if let ComparisonResponse::ViewerExtensionsChanged(catalog) = response {
            self.extensions.catalog = catalog;
            return;
        }
        #[cfg(target_os = "macos")]
        if let ComparisonResponse::ExportDiagnostic(target, draft, send) = response {
            self.export_diagnostics(target, draft, send, ctx);
            return;
        }
        if matches!(response, ComparisonResponse::Open) {
            return;
        }
        self.palette_focus_to_restore = comparison.return_focus;
        self.comparison = None;
        #[cfg(target_os = "macos")]
        match response {
            ComparisonResponse::OpenCommandPalette(query) => {
                let focus = self.palette_focus_to_restore.take();
                Self::restore_focus(ctx, focus);
                self.open_command_palette(ctx, focus, query);
            }
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
    Failed(PhaseFailure),
    Stopped,
}

pub(super) enum PhaseFailure {
    Operation(TransferError),
    WorkerStart(io::Error),
    WorkerStopped,
}

impl PhaseFailure {
    pub(super) fn message(&self, operation: &str) -> String {
        #[cfg(not(target_os = "macos"))]
        let _ = operation;

        match self {
            Self::Operation(error) => crate::i18n::file_error(error),
            Self::WorkerStart(error) => {
                #[cfg(target_os = "macos")]
                let message = crate::tr_format!(
                    "Could not start “{operation}”. {error}",
                    operation = operation,
                    error = error
                );
                #[cfg(not(target_os = "macos"))]
                let message =
                    crate::tr_format!("Could not start comparison: {error}", error = error);
                message
            }
            Self::WorkerStopped => {
                #[cfg(target_os = "macos")]
                let message =
                    crate::tr_format!("“{operation}” ended unexpectedly.", operation = operation);
                #[cfg(not(target_os = "macos"))]
                let message = text!("Сравнение неожиданно завершилось").into();
                message
            }
        }
    }
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
            Err(error) => Self::Failed(PhaseFailure::WorkerStart(error)),
        }
    }

    pub(super) fn poll(&mut self) {
        let Self::Running(worker) = self else {
            return;
        };
        *self = match worker.result.try_recv() {
            Ok(Ok(result)) => Self::Finished(result),
            Ok(Err(TransferError::Cancelled)) => Self::Stopped,
            Ok(Err(error)) => Self::Failed(PhaseFailure::Operation(error)),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Self::Failed(PhaseFailure::WorkerStopped),
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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::{
        file_error::{AccessCause, AccessError, Failure},
        shortcuts::Command,
    };

    fn waiting_phase() -> (mpsc::Sender<Result<(), TransferError>>, Phase<()>) {
        let (send, result) = mpsc::channel();
        let (_, progress) = Progress::tracked();
        (send, Phase::Running(Worker { result, progress }))
    }

    #[test]
    fn phase_retains_the_requested_path_and_errno_after_a_stop_request() {
        let target = Location::Local("/fixture/missing template.txt".into());
        let (send, mut phase) = waiting_phase();
        let Phase::Running(worker) = &phase else {
            panic!("expected a running worker");
        };
        worker.progress.cancel.request();
        let error = AccessError::read(&target, io::Error::from_raw_os_error(libc::ENOENT));
        send.send(Err(error.into())).unwrap();

        phase.poll();

        let Phase::Failed(
            failure @ PhaseFailure::Operation(TransferError::Failed(Failure::Access(error))),
        ) = &phase
        else {
            panic!("expected the original access failure");
        };
        assert_eq!(error.location, target);
        assert!(
            matches!(&error.cause, AccessCause::Local(source) if source.raw_os_error() == Some(libc::ENOENT))
        );
        let message = failure.message(text!("Сохранение шаблона"));
        assert!(
            message.contains(&target.display()) && message.contains("os error 2"),
            "{message}"
        );
    }

    #[test]
    fn phase_distinguishes_cancelled_and_lost_workers_and_names_the_operation() {
        let (send, mut cancelled) = waiting_phase();
        send.send(Err(TransferError::Cancelled)).unwrap();
        cancelled.poll();
        assert!(matches!(cancelled, Phase::Stopped));

        let (send, mut disconnected) = waiting_phase();
        drop(send);
        disconnected.poll();
        let Phase::Failed(failure @ PhaseFailure::WorkerStopped) = &disconnected else {
            panic!("expected a lost-worker failure");
        };
        let title = Command::DiskUsage.label();
        let message = failure.message(title);
        assert!(message.contains(title), "{message}");

        let start = PhaseFailure::WorkerStart(io::Error::from_raw_os_error(libc::EAGAIN));
        let message = start.message(title);
        assert!(
            message.contains(title) && message.contains("os error 35"),
            "{message}"
        );
    }
}
