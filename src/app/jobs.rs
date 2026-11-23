use super::dialogs::Dialog;
use super::directory::{FolderVisit, AUTO_REFRESH_INTERVAL};
use super::state::FolderMode;
use super::PawCommander;
use crate::conflict::{Answer, Conflict, ConflictDialog, Decision};
#[cfg(target_os = "macos")]
use crate::file_error::FilePromiseError;
use crate::file_error::{Failure, MutationError};
use crate::fs_ops::TransferKind;
use crate::operations::{Cancellation, Progress, ProgressView, TransferError};
use crate::vfs as fs_ops;
use crate::vfs::Location;
use eframe::egui;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub(super) enum TransferEvent {
    Conflict(Box<ConflictDialog>),
    Moved(Location, Vec<PathBuf>),
    RetryReady(Box<RetryJob>),
}

#[derive(Clone)]
pub(super) struct RetryJob {
    pub id: String,
    pub transfer: crate::vfs::RetryTransfer,
    cut_generation: Option<u64>,
    verify_copies: bool,
}

#[derive(Clone)]
pub(super) struct UndoJob {
    pub id: String,
    pub batch: crate::vfs::UndoBatch,
}

struct TransferRequest {
    label: String,
    sources: Vec<Location>,
    destination: Location,
    kind: TransferKind,
    cut_generation: Option<u64>,
    verify_copies: bool,
    retry: Option<RetryJob>,
}

pub(super) struct TransferInteraction {
    pub(super) events: Receiver<TransferEvent>,
    pub(super) cut_generation: Option<u64>,
    pub(super) conflict: Option<Box<ConflictDialog>>,
    pub(super) can_pause: bool,
}

pub(super) struct Job {
    pub(super) transfer: Option<TransferInteraction>,
    pub(super) cut: Option<CutRequest>,
    pub(super) label: String,
    pub(super) started: Instant,
    pub(super) result: Receiver<Result<String, String>>,
    pub(super) progress: ProgressView,
    pub(super) undo: Option<Receiver<UndoJob>>,
}

pub(super) type JobWork = Box<dyn FnOnce(&mut Progress) -> Result<String, TransferError> + Send>;

pub(super) type CutRequest = (u64, Vec<Location>);

// One global worker serializes all mutations, including aliased and nested paths.
// Pending closures own their inputs; no filesystem work is done until dispatch.
pub(super) struct PendingJob {
    pub(super) label: String,
    pub(super) description: String,
    pub(super) work: JobWork,
    pub(super) transfer: Option<TransferInteraction>,
    pub(super) cut: Option<CutRequest>,
    retry_origin: Option<RetryJob>,
    undo_origin: Option<UndoJob>,
    #[cfg(target_os = "macos")]
    native_completion: Option<mpsc::Sender<Result<(), TransferError>>>,
}

#[derive(Default)]
pub(super) struct JobQueue {
    pub(super) current: Option<Job>,
    pub(super) pending: VecDeque<PendingJob>,
    pub(super) retries: VecDeque<RetryJob>,
    pub(super) undo: VecDeque<UndoJob>,
    pub(super) start_failed: bool,
    #[cfg(test)]
    pub(super) fail_spawn: bool,
}

impl JobQueue {
    pub(super) fn is_busy(&self) -> bool {
        self.current.is_some() || !self.pending.is_empty()
    }

    pub(super) fn transfer(&self) -> Option<&TransferInteraction> {
        self.current.as_ref().and_then(|job| job.transfer.as_ref())
    }

    pub(super) fn transfer_mut(&mut self) -> Option<&mut TransferInteraction> {
        self.current.as_mut().and_then(|job| job.transfer.as_mut())
    }
}

pub(super) const MAX_PENDING_JOBS: usize = 100;

pub(super) fn wait_for_conflict_answer(
    answer: &Receiver<Answer>,
    cancellation: &Cancellation,
) -> Answer {
    loop {
        if cancellation.is_requested() {
            break;
        }
        match answer.recv_timeout(Duration::from_millis(30)) {
            Ok(answer) if !cancellation.is_requested() => return answer,
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    Answer {
        decision: Decision::Cancel,
        apply_to_group: false,
    }
}

fn ask_conflict(
    events: &mpsc::Sender<TransferEvent>,
    ctx: &egui::Context,
    cancellation: &Cancellation,
    conflict: Conflict,
) -> Result<Answer, TransferError> {
    let (reply, answer) = mpsc::channel();
    let dialog = ConflictDialog::new(conflict, reply);
    events
        .send(TransferEvent::Conflict(Box::new(dialog)))
        .map_err(|_| TransferError::Failed(Failure::ConflictClosed))?;
    ctx.request_repaint();
    Ok(wait_for_conflict_answer(&answer, cancellation))
}

impl PawCommander {
    pub(super) fn start_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        work: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) {
        self.start_progress_job(ctx, label, |_| work());
    }

    pub(super) fn start_progress_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        work: impl FnOnce(&mut Progress) -> Result<String, String> + Send + 'static,
    ) {
        self.start_described_job(ctx, label, label.into(), work);
    }

    pub(super) fn start_described_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        description: String,
        work: impl FnOnce(&mut Progress) -> Result<String, String> + Send + 'static,
    ) {
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| work(progress).map_err(TransferError::from)),
                transfer: None,
                cut: None,
                retry_origin: None,
                undo_origin: None,
                #[cfg(target_os = "macos")]
                native_completion: None,
            },
        );
    }

    pub(super) fn start_batch_rename(
        &mut self,
        ctx: &egui::Context,
        plan: crate::vfs::BatchRenamePreview,
    ) {
        let label = crate::i18n::text!("Массовое переименование");
        let folder = plan.directory().display();
        let description = format!("{label} ({}) · {folder}", plan.changes);
        let (_, events) = mpsc::channel();
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| {
                    plan.apply(progress)
                        .map(super::batch_rename::rename_finished)
                }),
                transfer: Some(TransferInteraction {
                    events,
                    cut_generation: None,
                    conflict: None,
                    can_pause: false,
                }),
                cut: None,
                retry_origin: None,
                undo_origin: None,
                #[cfg(target_os = "macos")]
                native_completion: None,
            },
        );
    }

    #[cfg(target_os = "macos")]
    pub(super) fn enqueue_new_file(
        &mut self,
        job: crate::vfs::file_templates::NewFile,
        completion: mpsc::Sender<Result<(), TransferError>>,
        ctx: &egui::Context,
    ) {
        let label = crate::i18n::text!("Создание файла");
        let description = format!("{label} · {}", job.target().display().escape_debug());
        let (_events, receive) = mpsc::channel();
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| {
                    let created = job.create(progress)?;
                    Ok(if let Some(warning) = created.cleanup_warning {
                        crate::i18n::file_error(&warning)
                    } else {
                        let path = created.target.display().escape_debug().to_string();
                        crate::tr_format!("File created: {path}", path = path)
                    })
                }),
                transfer: Some(TransferInteraction {
                    events: receive,
                    cut_generation: None,
                    conflict: None,
                    can_pause: false,
                }),
                cut: None,
                retry_origin: None,
                undo_origin: None,
                native_completion: Some(completion),
            },
        );
    }

    pub(super) fn start_stoppable_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        description: String,
        work: impl FnOnce(&mut Progress) -> Result<String, String> + Send + 'static,
    ) {
        self.start_conflict_job(ctx, label, description, move |progress, _| work(progress));
    }

    #[cfg(target_os = "macos")]
    pub(super) fn start_typed_stoppable_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        description: String,
        work: impl FnOnce(&mut Progress) -> Result<String, TransferError> + Send + 'static,
    ) {
        self.start_conflict_job(ctx, label, description, move |progress, _| work(progress));
    }

    #[cfg(target_os = "macos")]
    pub(super) fn enqueue_compatible_names(
        &mut self,
        plan: crate::vfs::compatible_names::Plan,
        ctx: &egui::Context,
    ) {
        let label = crate::shortcuts::Command::CompatibleNames.label();
        let description = format!("{label} · {}", plan.destination().display().escape_debug());
        let verify = plan.kind() == TransferKind::Copy && self.settings.verify_copies;
        let (_events, receive) = mpsc::channel();
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| {
                    progress.set_copy_verification(verify);
                    let summary = plan.apply(progress).map_err(|failure| {
                        let error = crate::i18n::file_error(&failure.error);
                        if failure.summary.preparing {
                            error
                        } else {
                            crate::tr_format!(
                                "Completed items: {completed}. {error}",
                                completed = failure.summary.completed,
                                error = error
                            )
                        }
                    })?;
                    let completed = summary.completed;
                    let mut message = if summary.cancelled {
                        crate::tr_format!(
                            "Completed items: {completed}. {error}",
                            completed = completed,
                            error = crate::tr_format!("Operation cancelled")
                        )
                    } else {
                        crate::tr_format!(
                            "Transfer with compatible names finished. Items: {completed}",
                            completed = completed
                        )
                    };
                    if summary.metadata_warnings != 0 {
                        message.push('\n');
                        message.push_str(crate::i18n::text!("Даты или права доступа сохранены частично. Подробности указаны в строках журнала."));
                    }
                    Ok(message)
                }),
                transfer: Some(TransferInteraction {
                    events: receive,
                    cut_generation: None,
                    conflict: None,
                    can_pause: true,
                }),
                cut: None,
                retry_origin: None,
                undo_origin: None,
                native_completion: None,
            },
        );
    }

    #[cfg(target_os = "macos")]
    pub(super) fn accept_file_promises(&mut self, ctx: &egui::Context) {
        while let Some(request) = self.native_drag.next_promise() {
            self.enqueue_file_promise(request, ctx);
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn enqueue_file_promise(
        &mut self,
        request: crate::native_drag::PromiseRequest,
        ctx: &egui::Context,
    ) {
        use crate::native_drag::PromiseRequest;
        use fs_ops::DropDelivery;

        let PromiseRequest {
            source,
            kind,
            target,
            completion,
        } = request;
        if self.jobs.start_failed || self.jobs.pending.len() >= MAX_PENDING_JOBS {
            let error = FilePromiseError::QueueUnavailable.into();
            self.notice = Some((crate::i18n::file_error(&error), true));
            let _ = completion.send(Err(error));
            return;
        }
        let label = crate::i18n::text!("Скачивание для принимающего приложения");
        let description = format!("{label} · {source} → {}", target.display());
        let recovery = self.recovery.directory();
        let verify = self.settings.verify_copies;
        let (_, events) = mpsc::channel();
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| {
                    progress.resume_directory = recovery;
                    progress.set_copy_verification(verify);
                    let delivery = fs_ops::copy_sftp_for_drop(&source, &target, kind, progress)?;
                    let message = match delivery {
                        DropDelivery::Completed => {
                            crate::i18n::text!("Файлы переданы принимающему приложению")
                        }
                        DropDelivery::CompletedWithWarning => crate::i18n::text!(
                            "Файл передан. Подробности о предупреждениях в журнале результатов."
                        ),
                    };
                    Ok(message.into())
                }),
                transfer: Some(TransferInteraction {
                    events,
                    cut_generation: None,
                    conflict: None,
                    can_pause: true,
                }),
                cut: None,
                retry_origin: None,
                undo_origin: None,
                native_completion: Some(completion),
            },
        );
    }

    #[cfg(target_os = "macos")]
    pub(super) fn resume_copy(
        &mut self,
        ctx: &egui::Context,
        offer: crate::vfs::resume_copy::Offer,
    ) {
        let Some(directory) = self.recovery.directory() else {
            return;
        };
        let (_, events) = mpsc::channel();
        let label = if offer.moving {
            crate::i18n::text!("Продолжение переноса")
        } else {
            crate::i18n::text!("Продолжение копирования")
        };
        let description = format!("{label} · {} → {}", offer.source, offer.target);
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| {
                    crate::vfs::resume_copy::resume(&directory, &offer.id, progress)
                        .map(crate::i18n::resume_completion)
                        .or_else(|error| {
                            if error.is_completed_cleanup_warning() {
                                Ok(crate::i18n::file_error(&error))
                            } else {
                                Err(error)
                            }
                        })
                }),
                transfer: Some(TransferInteraction {
                    events,
                    cut_generation: None,
                    conflict: None,
                    can_pause: true,
                }),
                cut: None,
                retry_origin: None,
                undo_origin: None,
                #[cfg(target_os = "macos")]
                native_completion: None,
            },
        );
    }

    pub(super) fn start_conflict_job<E: Into<TransferError>>(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        description: String,
        work: impl FnOnce(
                &mut Progress,
                &mut dyn FnMut(Conflict) -> Result<Answer, TransferError>,
            ) -> Result<String, E>
            + Send
            + 'static,
    ) {
        let (events, receive) = mpsc::channel();
        let worker_ctx = ctx.clone();
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| {
                    let cancel = progress.cancellation();
                    work(progress, &mut |conflict| {
                        ask_conflict(&events, &worker_ctx, &cancel, conflict)
                    })
                    .map_err(Into::into)
                }),
                transfer: Some(TransferInteraction {
                    events: receive,
                    cut_generation: None,
                    conflict: None,
                    can_pause: false,
                }),
                cut: None,
                retry_origin: None,
                undo_origin: None,
                #[cfg(target_os = "macos")]
                native_completion: None,
            },
        );
    }

    pub(super) fn submit_job(&mut self, ctx: &egui::Context, job: PendingJob) {
        let rejection =
            (self.jobs.pending.len() >= MAX_PENDING_JOBS).then_some(crate::i18n::text!(
            "Очередь заполнена (100 заданий). Дождитесь завершения или удалите ожидающее задание."
        ));
        #[cfg(target_os = "macos")]
        let rejection = if self.volumes.ejecting() {
            Some(super::volumes::wait_for_eject())
        } else {
            rejection
        };
        if let Some(message) = rejection {
            if let Some(retry) = job.retry_origin {
                self.remember_retry(retry);
            }
            if let Some(undo) = job.undo_origin {
                self.remember_undo(undo);
            }
            #[cfg(target_os = "macos")]
            if let Some(completion) = job.native_completion {
                let _ = completion.send(Err(message.into()));
            }
            self.notice = Some((message.into(), true));
            return;
        }
        if !self.jobs.is_busy() {
            self.notice = None;
        }
        self.jobs.pending.push_back(job);
        self.start_next_job(ctx);
        ctx.request_repaint();
    }

    pub(super) fn start_next_job(&mut self, ctx: &egui::Context) {
        if self.jobs.start_failed {
            return;
        }
        while self.jobs.current.is_none() {
            let Some(PendingJob {
                label,
                description,
                work,
                transfer,
                cut,
                retry_origin,
                undo_origin,
                #[cfg(target_os = "macos")]
                native_completion,
                ..
            }) = self.jobs.pending.pop_front()
            else {
                break;
            };
            let (tx, rx) = mpsc::channel();
            let (undo_send, undo_receive) = mpsc::channel();
            let undo_id = undo_origin.as_ref().map(|job| job.id.clone());
            let worker_ctx = ctx.clone();
            let (mut reporter, progress) = if transfer.as_ref().is_some_and(|job| job.can_pause) {
                Progress::tracked_transfer()
            } else {
                Progress::tracked()
            };
            progress.set_transfer_limits(&self.settings.transfer_limits);
            let started = Instant::now();
            let history = self.history.sender();
            let history_label = label.clone();
            let recovery_directory = self.recovery.directory();
            let recovery_description = description.clone();
            // A failed OS spawn consumes its closure. Retain the work separately
            // so failure cannot discard this job or the jobs behind it.
            let work = Arc::new(Mutex::new(Some(work)));
            let worker_work = work.clone();
            #[cfg(target_os = "macos")]
            let worker_completion = native_completion.clone();
            let worker = move || {
                let fail_to_start = |error: String| {
                    #[cfg(target_os = "macos")]
                    if let Some(completion) = &worker_completion {
                        let _ = completion.send(Err(error.clone().into()));
                    }
                    let _ = tx.send(Err(error));
                    worker_ctx.request_repaint();
                };
                let work = worker_work.lock().unwrap().take().unwrap();
                reporter.results.id = crate::result_history::operation_id();
                let history_started = crate::result_history::now();
                if recovery_directory.is_none() && !cfg!(test) {
                    fail_to_start(crate::tr_format!(
                        "Operation not started: application data directory for recovery is unavailable."
                    ));
                    return;
                }
                let pending = match recovery_directory
                    .as_deref()
                    .map(|directory| {
                        crate::recovery::begin(
                            directory,
                            &history_label,
                            &recovery_description,
                            history_started,
                        )
                    })
                    .transpose()
                {
                    Ok(pending) => pending,
                    Err(error) => {
                        fail_to_start(crate::tr_format!(
                            "Operation did not start. {error}",
                            error = crate::i18n::recovery_record_error(&error)
                        ));
                        return;
                    }
                };
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        match pending.as_ref() {
                            Some(pending) => pending.run(|| work(&mut reporter)),
                            None => Ok(work(&mut reporter)),
                        }
                    }));
                let panicked = outcome.is_err();
                let result = match outcome {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => {
                        let mut message = crate::tr_format!(
                            "Operation did not start. {error}",
                            error = crate::i18n::recovery_record_error(&error)
                        );
                        if let Some(pending) = pending
                            && let Err(cleanup) = pending.complete()
                        {
                            message.push('\n');
                            message.push_str(&crate::i18n::recovery_record_error(&cleanup));
                        }
                        fail_to_start(message);
                        return;
                    }
                    Err(_) => Err(crate::i18n::text!(
                        "Фоновая операция неожиданно завершилась. Проверьте файлы в обеих папках.",
                    )
                    .into()),
                };
                // The native file promise acknowledges the file operation. History
                // persistence and recovery record cleanup remain worker follow-up.
                let display_result = if matches!(&result, Err(TransferError::Cancelled)) {
                    reporter.results.cancelled = true;
                    Ok(crate::tr_format!("Operation cancelled"))
                } else {
                    result.as_ref().cloned().map_err(crate::i18n::file_error)
                };
                #[cfg(target_os = "macos")]
                if let Some(completion) = worker_completion {
                    let _ = completion.send(result.map(|_| ()));
                }
                let result = display_result;
                let undo_id = undo_id.unwrap_or_else(|| reporter.results.id.clone());
                let persisted = std::mem::take(&mut reporter.results)
                    .finish(history_label, history_started, &result)
                    .map_or(Ok(()), |record| history.append(record));
                let warning = match &persisted {
                    Err(error) => pending.as_ref().map(|pending| {
                        let reason = crate::i18n::history_error(error);
                        let retained = crate::tr_format!(
                            "Recovery record kept at {path}.",
                            path = pending.path().display()
                        );
                        format!("{reason}\n{retained}")
                    }),
                    Ok(()) if !panicked => pending
                        .and_then(|pending| pending.complete().err())
                        .map(|error| crate::i18n::recovery_record_error(&error)),
                    Ok(()) => None,
                };
                if !panicked && persisted.is_ok() && reporter.undo.len() != 0 {
                    let _ = undo_send.send(UndoJob {
                        id: undo_id,
                        batch: reporter.undo,
                    });
                }
                let result = match warning {
                    Some(warning) => Err(format!(
                        "{}\n{warning}",
                        result.unwrap_or_else(|error| error)
                    )),
                    None => result,
                };
                let _ = tx.send(result);
                worker_ctx.request_repaint();
            };
            let builder = std::thread::Builder::new().name("pawcommander-file-operation".into());
            #[cfg(test)]
            let spawn_result = if self.jobs.fail_spawn {
                Err(std::io::Error::other("injected worker spawn failure"))
            } else {
                builder.spawn(worker)
            };
            #[cfg(not(test))]
            let spawn_result = builder.spawn(worker);
            if let Err(error) = spawn_result {
                let message =
                    crate::tr_format!("Could not start operation: {error}", error = error);
                #[cfg(target_os = "macos")]
                if let Some(completion) = native_completion {
                    let error = TransferError::io(error, |_| message.clone());
                    let _ = completion.send(Err(error));
                    self.notice = Some((message, true));
                    continue;
                }
                self.jobs.pending.push_front(PendingJob {
                    label,
                    description,
                    work: work.lock().unwrap().take().unwrap(),
                    transfer,
                    cut,
                    retry_origin,
                    undo_origin,
                    #[cfg(target_os = "macos")]
                    native_completion: None,
                });
                self.jobs.start_failed = true;
                self.notice = Some((message, true));
                break;
            }
            self.jobs.current = Some(Job {
                transfer,
                cut,
                label,
                started,
                result: rx,
                progress,
                undo: Some(undo_receive),
            });
            ctx.request_repaint();
        }
    }

    pub(super) fn drop_enabled(&self) -> bool {
        !self.conflict_open()
            && self.quick_look.is_none()
            && self.dialog.is_none()
            && self.properties.is_none()
            && self.comparison.is_none()
            && self.sftp_form.is_none()
            && !self.show_help
            && !self.show_history
            && !self.recovery.is_open()
            && self.shortcut_editor.is_none()
            && self.command_palette.is_none()
            && self.folder_jump.is_none()
            && !self.show_list_columns
            && !self.search.visible
    }

    pub(super) fn start_transfer(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        sources: Vec<Location>,
        destination: Location,
        kind: TransferKind,
        from_cut_clipboard: bool,
    ) {
        let cut_generation = from_cut_clipboard
            .then(|| {
                self.clipboard
                    .as_ref()
                    .map(|clipboard| clipboard.generation)
            })
            .flatten();
        self.submit_transfer(
            ctx,
            TransferRequest {
                label: label.into(),
                sources,
                destination,
                kind,
                cut_generation,
                verify_copies: kind == TransferKind::Copy && self.settings.verify_copies,
                retry: None,
            },
        );
    }

    pub(super) fn retry_transfer(&mut self, ctx: &egui::Context, id: &str) {
        if self.jobs.pending.len() >= MAX_PENDING_JOBS {
            self.notice = Some((crate::i18n::text!("Очередь заполнена (100 заданий). Дождитесь завершения или удалите ожидающее задание.").into(), true));
            return;
        }
        let Some(index) = self.jobs.retries.iter().position(|job| job.id == id) else {
            return;
        };
        // Move ownership before enqueueing: a second click cannot repeat the
        // same attempt, even while this job waits behind another operation.
        let job = self.jobs.retries.remove(index).unwrap();
        self.history.retryable.remove(id);
        self.history.inspectable.remove(id);
        self.show_history = false;
        if job.transfer.needs_inspection() {
            self.check_interrupted_transfer(ctx, job);
            return;
        }
        self.submit_transfer(
            ctx,
            TransferRequest {
                label: crate::tr_format!("Retry unfinished items"),
                sources: job.transfer.sources(),
                destination: job.transfer.destination.clone(),
                kind: job.transfer.kind,
                cut_generation: job.cut_generation,
                verify_copies: job.verify_copies,
                retry: Some(job),
            },
        );
    }

    fn check_interrupted_transfer(&mut self, ctx: &egui::Context, mut job: RetryJob) {
        let (publish, events) = mpsc::channel();
        let repaint = ctx.clone();
        let retry_origin = Some(job.clone());
        let label = crate::i18n::text!("Проверка прерванной операции SFTP");
        let description = format!("{label} · {}", job.transfer.destination);
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| {
                    let id = crate::result_history::operation_id();
                    progress.results.id = id.clone();
                    let send = |job| {
                        let _ = publish.send(TransferEvent::RetryReady(Box::new(job)));
                        repaint.request_repaint();
                    };
                    let checked = job.transfer.len();
                    match job.transfer.inspect(progress) {
                        Ok((ready, pending)) => {
                            let ready_count =
                                ready.as_ref().map_or(0, crate::vfs::RetryTransfer::len);
                            if let Some(transfer) = ready {
                                send(RetryJob {
                                    id,
                                    transfer,
                                    cut_generation: job.cut_generation,
                                    verify_copies: job.verify_copies,
                                });
                            }
                            if let Some(transfer) = pending {
                                job.transfer = transfer;
                                send(job);
                            }
                            Ok(crate::tr_format!(
                                "Checked {checked} items. Ready to retry {ready_count}.",
                                checked = checked,
                                ready_count = ready_count
                            ))
                        }
                        Err(crate::operations::TransferError::Cancelled) => {
                            send(job);
                            progress.results.cancelled = true;
                            Ok(
                                crate::i18n::text!("Проверка прерванной операции SFTP отменена.")
                                    .into(),
                            )
                        }
                        Err(error) => {
                            send(job);
                            Err(error)
                        }
                    }
                }),
                transfer: Some(TransferInteraction {
                    events,
                    cut_generation: None,
                    conflict: None,
                    can_pause: false,
                }),
                cut: None,
                retry_origin,
                undo_origin: None,
                #[cfg(target_os = "macos")]
                native_completion: None,
            },
        );
    }

    fn submit_transfer(&mut self, ctx: &egui::Context, request: TransferRequest) {
        let TransferRequest {
            label,
            sources,
            destination,
            kind,
            cut_generation,
            verify_copies,
            retry,
        } = request;
        if sources.is_empty() {
            return;
        }
        if let Some(archive) = std::iter::once(&destination)
            .chain(&sources)
            .find(|location| location.is_archive())
        {
            let error = MutationError::ReadOnlyArchive(archive.clone()).into();
            self.notice = Some((crate::i18n::file_error(&error), true));
            return;
        }
        let cut = cut_generation.map(|generation| (generation, sources.clone()));
        if let Some((generation, paths)) = &cut {
            let overlaps = |(other_generation, other_paths): &CutRequest| {
                generation == other_generation
                    && paths.iter().any(|path| {
                        other_paths.iter().any(|other| {
                            path.same_backend(other)
                                && (path.path().starts_with(other.path())
                                    || other.path().starts_with(path.path()))
                        })
                    })
            };
            let active_cut = self.jobs.current.as_ref().and_then(|job| job.cut.as_ref());
            if active_cut.is_some_and(overlaps)
                || self
                    .jobs
                    .pending
                    .iter()
                    .filter_map(|job| job.cut.as_ref())
                    .any(overlaps)
            {
                if let Some(retry) = retry {
                    self.remember_retry(retry);
                }
                self.notice = Some((
                    crate::i18n::text!("Эти вырезанные объекты уже ожидают перемещения.").into(),
                    true,
                ));
                return;
            }
        }
        let mut description = format!(
            "{} ({}): {} → {}",
            label,
            sources.len(),
            sources.first().unwrap().display(),
            destination.display()
        );
        if verify_copies {
            description.push_str(" · SHA-256");
        }
        let (events, rx) = mpsc::channel();
        let worker_ctx = ctx.clone();
        let retry_origin = retry.clone();
        #[cfg(target_os = "macos")]
        let resume_directory = if cfg!(test) {
            None
        } else {
            self.recovery.directory()
        };
        let work = move |progress: &mut Progress| {
            let id = crate::result_history::operation_id();
            progress.results.id = id.clone();
            progress.retry = Some(
                retry
                    .map(|job| job.transfer)
                    .unwrap_or_else(|| crate::vfs::RetryTransfer::new(kind, destination.clone())),
            );
            progress.set_copy_verification(verify_copies);
            #[cfg(target_os = "macos")]
            {
                progress.resume_directory = resume_directory;
            }
            let cancellation = progress.cancellation();
            let result = fs_ops::transfer_with_conflicts(
                &sources,
                &destination,
                kind,
                progress,
                |conflict| ask_conflict(&events, &worker_ctx, &cancellation, conflict),
                |location, covered| {
                    if cut_generation.is_some() {
                        let _ =
                            events.send(TransferEvent::Moved(location.clone(), covered.to_vec()));
                        worker_ctx.request_repaint();
                    }
                },
            );
            if let Some(transfer) = progress
                .retry
                .take()
                .and_then(|retry| retry.unfinished(&progress.results))
            {
                let _ = events.send(TransferEvent::RetryReady(Box::new(RetryJob {
                    id,
                    transfer,
                    cut_generation,
                    verify_copies,
                })));
                worker_ctx.request_repaint();
            }
            #[cfg(target_os = "macos")]
            if let Err(failure) = &result
                && failure.completed_with_cleanup_warning()
            {
                return Ok(crate::i18n::transfer_failure(failure));
            }
            let summary = result.map_err(|failure| crate::i18n::transfer_failure(&failure))?;
            let mut message = crate::i18n::transfer_finished(
                summary.completed,
                summary.skipped,
                &summary
                    .remaining
                    .iter()
                    .map(Location::display)
                    .collect::<Vec<_>>(),
                summary.preparing,
                summary.cancelled,
                &destination.display(),
            );
            if let Some(warning) = &summary.move_history_warning {
                message.push('\n');
                message.push_str(&crate::i18n::file_error(warning));
            }
            if summary.metadata_warnings != 0 {
                message.push('\n');
                message.push_str(crate::i18n::text!("Даты или права доступа сохранены частично. Подробности указаны в строках журнала."));
            }
            Ok(message)
        };
        self.submit_job(
            ctx,
            PendingJob {
                label,
                description,
                work: Box::new(work),
                transfer: Some(TransferInteraction {
                    events: rx,
                    cut_generation,
                    conflict: None,
                    can_pause: true,
                }),
                cut,
                retry_origin,
                undo_origin: None,
                #[cfg(target_os = "macos")]
                native_completion: None,
            },
        );
    }

    fn remember_retry(&mut self, job: RetryJob) {
        self.jobs.retries.retain(|saved| saved.id != job.id);
        self.jobs.retries.push_back(job);
        while self.jobs.retries.len() > 100
            || self
                .jobs
                .retries
                .iter()
                .map(|job| job.transfer.weight())
                .sum::<usize>()
                > 100_000
        {
            self.jobs.retries.pop_front();
        }
        self.history.retryable = self
            .jobs
            .retries
            .iter()
            .filter(|job| !job.transfer.needs_inspection())
            .map(|job| job.id.clone())
            .collect();
        self.history.inspectable = self
            .jobs
            .retries
            .iter()
            .filter(|job| job.transfer.needs_inspection())
            .map(|job| job.id.clone())
            .collect();
    }

    fn remember_undo(&mut self, job: UndoJob) {
        self.jobs.undo.retain(|saved| saved.id != job.id);
        self.jobs.undo.push_back(job);
        while self.jobs.undo.len() > 100
            || self
                .jobs
                .undo
                .iter()
                .map(|job| job.batch.weight())
                .sum::<usize>()
                > 100_000
        {
            self.jobs.undo.pop_front();
        }
        self.history.undoable = self
            .jobs
            .undo
            .iter()
            .map(|job| (job.id.clone(), job.batch.len()))
            .collect();
    }

    pub(super) fn undo_operation(&mut self, ctx: &egui::Context, id: &str) {
        use crate::vfs::UndoOutcome;

        if self.jobs.pending.len() >= MAX_PENDING_JOBS {
            self.notice = Some((crate::i18n::text!("Очередь заполнена (100 заданий). Дождитесь завершения или удалите ожидающее задание.").into(), true));
            return;
        }
        let Some(index) = self.jobs.undo.iter().position(|job| job.id == id) else {
            return;
        };
        let job = self.jobs.undo.remove(index).unwrap();
        self.history.undoable.remove(id);
        self.show_history = false;
        let mut batch = job.batch.clone();
        let label = crate::i18n::text!("Отмена изменений");
        let description = format!("{label}: {}", batch.len());
        // A transfer interaction exposes Stop and keeps the normal queue/close
        // lifecycle even though Undo never offers an overwrite conflict dialog.
        let (_, events) = mpsc::channel();
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| {
                    let result = batch.restore(progress);
                    progress.undo = batch;
                    Ok(match result? {
                        UndoOutcome::Restored(completed) => {
                            crate::tr_format!("Changes undone: {completed}", completed = completed)
                        }
                        UndoOutcome::Stopped {
                            restored,
                            remaining,
                        } => crate::tr_format!(
                            "Undo stopped. Restored: {restored}. Remaining: {remaining}.",
                            restored = restored,
                            remaining = remaining
                        ),
                    })
                }),
                transfer: Some(TransferInteraction {
                    events,
                    cut_generation: None,
                    conflict: None,
                    can_pause: false,
                }),
                cut: None,
                retry_origin: None,
                undo_origin: Some(job),
                #[cfg(target_os = "macos")]
                native_completion: None,
            },
        );
    }

    pub(super) fn remove_pending_job(&mut self, index: usize) {
        if let Some(job) = self.jobs.pending.remove(index) {
            if let Some(retry) = job.retry_origin {
                self.remember_retry(retry);
            }
            if let Some(undo) = job.undo_origin {
                self.remember_undo(undo);
            }
            #[cfg(target_os = "macos")]
            if let Some(completion) = job.native_completion {
                let _ = completion.send(Err(TransferError::Cancelled));
            }
        }
        if self.jobs.pending.is_empty() {
            self.jobs.start_failed = false;
        }
    }

    pub(super) fn poll_transfer(&mut self) {
        while let Some(job) = &mut self.jobs.current
            && let Some(transfer) = &mut job.transfer
        {
            match transfer.events.try_recv() {
                Ok(TransferEvent::Conflict(dialog)) => {
                    if !job.progress.cancel.is_requested() {
                        transfer.conflict = Some(dialog);
                    }
                }
                Ok(TransferEvent::Moved(location, covered)) => {
                    if let Some(generation) = transfer.cut_generation {
                        self.consume_cut_source(&location, &covered, generation);
                    }
                }
                Ok(TransferEvent::RetryReady(job)) => {
                    self.remember_retry(*job);
                }
                // The worker drops its event sender before publishing its result. Keep
                // ownership (and Stop state) until that terminal result is consumed.
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
    }

    pub(super) fn request_transfer_stop(&mut self, ctx: &egui::Context) {
        if let Some(job) = &mut self.jobs.current
            && let Some(transfer) = &mut job.transfer
            && !job.progress.cancel.is_requested()
        {
            job.progress.cancel.request();
            // Dropping a dialog releases its answer sender too. The worker also
            // observes the token if a conflict event is still in flight.
            transfer.conflict = None;
            ctx.request_repaint();
        }
    }

    pub(super) fn transfer_stop_shortcut(&mut self, ctx: &egui::Context) {
        if self.jobs.transfer().is_some()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::Escape))
        {
            self.request_transfer_stop(ctx);
        }
    }

    pub(super) fn conflict_open(&self) -> bool {
        self.jobs
            .transfer()
            .is_some_and(|transfer| transfer.conflict.is_some())
    }

    pub(super) fn poll(&mut self, ctx: &egui::Context) {
        self.poll_transfer();
        if self.conflict_open() && self.dialog_file_focus.is_none() {
            self.remember_dialog_file_focus(ctx);
        }
        if let Some(notice) = self
            .application_job
            .as_ref()
            .and_then(super::actions::ApplicationJob::poll_notice)
        {
            self.notice = Some(notice);
            self.application_job = None;
        }
        if let Some(job) = &self.connection {
            let result = match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text!(
                    "Задача подключения завершилась",
                )
                .into())),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                match result {
                    Ok(crate::vfs::ConnectedLocation {
                        location,
                        saved_profile,
                    }) => {
                        self.directory_tree.refresh();
                        if saved_profile {
                            self.settings
                                .sftp_profiles
                                .retain(|p| match (p, &location) {
                                    (
                                        Location::Sftp { endpoint: a, .. },
                                        Location::Sftp { endpoint: b, .. },
                                    ) => a != b,
                                    _ => true,
                                });
                            self.settings.sftp_profiles.push(location.clone());
                        }
                        for pane in &mut self.panes {
                            pane.tab_mut().address_completion.suspend();
                        }
                        self.panes[job.pane].tab_mut().navigate(location, ctx);
                        self.active = job.pane;
                        self.sftp_form = None;
                    }
                    Err(error) => {
                        if let Some(form) = &mut self.sftp_form {
                            form.error = Some(crate::i18n::connection_result_error(&error));
                        }
                    }
                }
                self.connection = None;
            }
        }
        // Avoid reordering rows under a held pointer. Defer automatic updates
        // during operations as well.
        let can_reorder = !self.native_drag.active()
            && !ctx.input(|input| input.pointer.any_down() || input.pointer.any_released())
            && !egui::DragAndDrop::has_any_payload(ctx);
        let automatic_allowed = self.jobs.current.is_none() && can_reorder;
        // Hold automatic refresh results while a rule examines this tab's visible rows.
        let scanning_tab = match &self.dialog {
            Some(Dialog::SelectionRule {
                tab, scan: Some(_), ..
            }) => Some(*tab),
            _ => None,
        };
        let now = Instant::now();
        for (pane_index, pane) in self.panes.iter_mut().enumerate() {
            let displayed = self.settings.split || pane_index == 0;
            for (tab_index, tab) in pane.tabs.iter_mut().enumerate() {
                let current = displayed && tab_index == pane.current;
                #[cfg(target_os = "macos")]
                if !current {
                    tab.git_snapshot = None;
                }
                let automatic_allowed = automatic_allowed && scanning_tab != Some(tab.id);
                if can_reorder && (!tab.automatic_loading() || automatic_allowed) && tab.poll() {
                    ctx.request_repaint();
                }
                if tab.has_complete_listing {
                    match &tab.folder_visit {
                        FolderVisit::CountOnOpen => {
                            self.settings.folder_history.record(&tab.path);
                            tab.folder_visit = FolderVisit::Recorded;
                        }
                        FolderVisit::SharedOpen(recorded) => {
                            if !recorded.replace(true) {
                                self.settings.folder_history.record(&tab.path);
                            }
                            tab.folder_visit = FolderVisit::Recorded;
                        }
                        FolderVisit::Unseen if current => {
                            self.settings.folder_history.record_if_new(&tab.path);
                            tab.folder_visit = FolderVisit::Recorded;
                        }
                        FolderVisit::Unseen | FolderVisit::Recorded => {}
                    }
                }
                if current {
                    if automatic_allowed {
                        tab.auto_refresh(ctx, now);
                    } else {
                        ctx.request_repaint_after(AUTO_REFRESH_INTERVAL);
                    }
                }
            }
            #[cfg(target_os = "macos")]
            pane.git_status
                .update(ctx, displayed.then_some(&mut pane.tabs[pane.current]), now);
            let tab = &pane.tabs[pane.current];
            let columns = displayed
                && tab
                    .folder_view(&self.settings, pane_index, &self.folder_views)
                    .mode
                    == FolderMode::Columns;
            let can_read = can_reorder
                && (!tab.path.is_archive() || tab.has_complete_listing)
                && (!tab.path.remote() || (tab.has_complete_listing && self.connection.is_none()));
            pane.columns
                .update(ctx, columns.then_some(tab), can_read, now);
        }
        if let Some(job) = &self.jobs.current {
            let result = match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text!(
                    "Фоновая операция неожиданно завершилась. Проверьте файлы в обеих папках.",
                )
                .into())),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                let undo = job
                    .undo
                    .as_ref()
                    .and_then(|receive| receive.try_recv().ok());
                if let Some(undo) = undo {
                    self.remember_undo(undo);
                }
                self.notice = Some(match result {
                    Ok(s) => (s, false),
                    Err(s) => (s, true),
                });
                // Moved events can arrive between the first event poll and the
                // result. Consume them before releasing transfer ownership.
                self.poll_transfer();
                self.jobs.current = None;
                self.refresh(ctx);
            }
        }
        self.start_next_job(ctx);
    }

    pub(super) fn move_pending_job(&mut self, index: usize, direction: isize) {
        if let Some(other) = index.checked_add_signed(direction)
            && index < self.jobs.pending.len()
            && other < self.jobs.pending.len()
        {
            self.jobs.pending.swap(index, other);
        }
    }
}
