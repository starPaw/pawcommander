use super::dialogs::Dialog;
use super::directory::{FolderVisit, AUTO_REFRESH_INTERVAL};
use super::state::FolderMode;
use super::PawCommander;
use crate::conflict::{Answer, Conflict, ConflictDialog, Decision};
use crate::fs_ops::TransferKind;
use crate::operations::{Cancellation, Progress, ProgressView};
use crate::vfs as fs_ops;
use crate::vfs::Location;
use eframe::egui;
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
    pub(super) label: String,
    pub(super) started: Instant,
    pub(super) result: Receiver<Result<String, String>>,
    pub(super) progress: ProgressView,
    pub(super) undo: Option<Receiver<UndoJob>>,
}

pub(super) type JobWork = Box<dyn FnOnce(&mut Progress) -> Result<String, String> + Send>;

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
    native_completion: Option<mpsc::Sender<Result<(), String>>>,
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
) -> Result<Answer, String> {
    let (reply, answer) = mpsc::channel();
    let dialog = ConflictDialog::new(conflict, reply);
    events
        .send(TransferEvent::Conflict(Box::new(dialog)))
        .map_err(|_| crate::i18n::text("Диалог конфликта закрыт"))?;
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
                work: Box::new(work),
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
        let label = crate::i18n::text("Массовое переименование");
        let folder = plan.directory().display();
        let description = format!("{label} ({}) · {folder}", plan.changes);
        self.start_stoppable_job(ctx, label, description, move |progress| {
            plan.apply(progress)
        });
    }

    #[cfg(target_os = "macos")]
    pub(super) fn enqueue_new_file(
        &mut self,
        job: crate::vfs::file_templates::NewFile,
        completion: mpsc::Sender<Result<(), String>>,
        ctx: &egui::Context,
    ) {
        let label = crate::i18n::text("Создание файла");
        let description = format!("{label} · {}", job.target().display().escape_debug());
        let (_events, receive) = mpsc::channel();
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| job.create(progress)),
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
                    plan.apply(progress)
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
        use crate::native_drag::{PromiseKind, PromiseRequest};

        while let Some(PromiseRequest {
            source,
            kind,
            target,
            completion,
        }) = self.native_drag.next_promise()
        {
            let label = crate::i18n::text("Скачивание для принимающего приложения");
            if self.queue_start_failed || self.pending_jobs.len() >= MAX_PENDING_JOBS {
                let error = crate::i18n::text("Очередь файловых операций недоступна.").to_owned();
                let _ = completion.send(Err(error.clone()));
                self.notice = Some((error, true));
                continue;
            }
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
                        fs_ops::copy_sftp_for_drop(
                            &source,
                            &target,
                            matches!(kind, PromiseKind::Directory),
                            progress,
                        )
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
        let label = crate::i18n::text(if offer.moving {
            "Продолжение переноса"
        } else {
            "Продолжение копирования"
        });
        let description = format!("{label} · {} → {}", offer.source, offer.target);
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(move |progress| {
                    crate::vfs::resume_copy::resume(&directory, &offer.id, progress)
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

    pub(super) fn start_conflict_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        description: String,
        work: impl FnOnce(
                &mut Progress,
                &mut dyn FnMut(Conflict) -> Result<Answer, String>,
            ) -> Result<String, String>
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
        if self.pending_jobs.len() >= MAX_PENDING_JOBS {
            if let Some(retry) = job.retry_origin {
                self.remember_retry(retry);
            }
            if let Some(undo) = job.undo_origin {
                self.remember_undo(undo);
            }
            self.notice = Some((crate::i18n::text("Очередь заполнена (100 заданий). Дождитесь завершения или удалите ожидающее задание.").into(), true));
            return;
        }
        if self.job.is_none() && self.pending_jobs.is_empty() {
            self.notice = None;
        }
        self.pending_jobs.push_back(job);
        self.start_next_job(ctx);
        ctx.request_repaint();
    }

    pub(super) fn start_next_job(&mut self, ctx: &egui::Context) {
        if self.queue_start_failed {
            return;
        }
        while self.job.is_none() {
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
            }) = self.pending_jobs.pop_front()
            else {
                break;
            };
            debug_assert!(self.transfer.is_none());
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
                #[cfg(target_os = "macos")]
                let report_to_receiver = |result: &Result<String, String>| {
                    if let Some(completion) = &worker_completion {
                        let _ = completion.send(result.as_ref().map(|_| ()).map_err(Clone::clone));
                    }
                };
                let fail_to_start = |error: String| {
                    let result = Err(error);
                    #[cfg(target_os = "macos")]
                    report_to_receiver(&result);
                    let _ = tx.send(result);
                    worker_ctx.request_repaint();
                };
                let work = worker_work.lock().unwrap().take().unwrap();
                reporter.results.id = crate::result_history::operation_id();
                let history_started = crate::result_history::now();
                if recovery_directory.is_none() && !cfg!(test) {
                    fail_to_start(crate::tr_format!(
                        "Операция не начата: папка данных приложения для восстановления недоступна.",
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
                            "Операция не начата: не удалось сохранить запись восстановления: {error}",
                            "Operation not started: could not save recovery record: {error}"
                        ));
                        return;
                    }
                };
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Some(pending) = pending.as_ref() {
                        pending.run(|| work(&mut reporter)).map_err(|error| crate::tr_format!(
                            "Операция не начата: не удалось активировать запись восстановления: {error}",
                            "Operation not started: could not activate recovery record: {error}"
                        ))?
                    } else {
                        work(&mut reporter)
                    }
                }));
                let panicked = outcome.is_err();
                let mut result = outcome.unwrap_or_else(|_| {
                    Err(crate::i18n::text(
                        "Фоновая операция неожиданно завершилась. Проверьте файлы в обеих папках.",
                    )
                    .into())
                });
                #[cfg(target_os = "macos")]
                report_to_receiver(&result);
                let undo_id = undo_id.unwrap_or_else(|| reporter.results.id.clone());
                let persisted = if let Some(record) = std::mem::take(&mut reporter.results).finish(
                    history_label,
                    history_started,
                    &result,
                ) {
                    history.append(record)
                } else {
                    true
                };
                if !panicked && persisted {
                    if let Some(pending) = pending
                        && let Err(error) = pending.complete()
                    {
                        result = Err(crate::tr_format!(
                                "Операция завершилась, но очистка записи восстановления не подтверждена: {error}. Проверьте журнал результатов.",
                                "Operation finished, but recovery record cleanup could not be confirmed: {error}. Check result history."
                            ));
                    }
                } else if !persisted && recovery_directory.is_some() {
                    result = Err(crate::tr_format!(
                        "Операция завершилась, но журнал результатов не сохранён. Проверьте файлы в обеих папках; запись восстановления сохранена, если была создана.",
                        "Operation finished, but result history was not saved. Check files in both folders; the recovery record was retained if created."
                    ));
                }
                if !panicked && persisted && reporter.undo.len() != 0 {
                    let _ = undo_send.send(UndoJob {
                        id: undo_id,
                        batch: reporter.undo,
                    });
                }
                let _ = tx.send(result);
                worker_ctx.request_repaint();
            };
            let builder = std::thread::Builder::new().name("pawcommander-file-operation".into());
            #[cfg(test)]
            let spawn_result = if self.fail_job_spawn {
                Err(std::io::Error::other("injected worker spawn failure"))
            } else {
                builder.spawn(worker)
            };
            #[cfg(not(test))]
            let spawn_result = builder.spawn(worker);
            if let Err(error) = spawn_result {
                let error = crate::tr_format!(
                    "Не удалось запустить операцию: {error}",
                    "Could not start operation: {error}"
                );
                #[cfg(target_os = "macos")]
                if let Some(completion) = native_completion {
                    let _ = completion.send(Err(error.clone()));
                    self.notice = Some((error, true));
                    continue;
                }
                self.pending_jobs.push_front(PendingJob {
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
                self.queue_start_failed = true;
                self.notice = Some((error, true));
                break;
            }
            self.job = Some(Job {
                label,
                started,
                result: rx,
                progress,
                undo: Some(undo_receive),
            });
            self.transfer = transfer;
            self.active_cut = cut;
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
        if self.pending_jobs.len() >= MAX_PENDING_JOBS {
            self.notice = Some((crate::i18n::text("Очередь заполнена (100 заданий). Дождитесь завершения или удалите ожидающее задание.").into(), true));
            return;
        }
        let Some(index) = self.retry_jobs.iter().position(|job| job.id == id) else {
            return;
        };
        // Move ownership before enqueueing: a second click cannot repeat the
        // same attempt, even while this job waits behind another operation.
        let job = self.retry_jobs.remove(index).unwrap();
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
                label: crate::tr_format!("Повтор незавершённого", "Retry unfinished items"),
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
        let label = crate::i18n::text("Проверка прерванной операции SFTP");
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
                                "Проверено объектов {checked}. Готово к повтору {ready_count}.",
                                "Checked {checked} items. Ready to retry {ready_count}."
                            ))
                        }
                        Err(crate::operations::TransferError::Cancelled) => {
                            send(job);
                            progress.results.cancelled = true;
                            Ok(
                                crate::i18n::text("Проверка прерванной операции SFTP отменена.")
                                    .into(),
                            )
                        }
                        Err(error) => {
                            send(job);
                            Err(error.to_string())
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
        if destination.is_archive() || sources.iter().any(Location::is_archive) {
            self.notice = Some((crate::archive::readonly().into(), true));
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
            if self.active_cut.as_ref().is_some_and(overlaps)
                || self
                    .pending_jobs
                    .iter()
                    .filter_map(|job| job.cut.as_ref())
                    .any(overlaps)
            {
                if let Some(retry) = retry {
                    self.remember_retry(retry);
                }
                self.notice = Some((
                    crate::i18n::text("Эти вырезанные объекты уже ожидают перемещения.").into(),
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
            let summary = result?;
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
            if summary.metadata_warnings != 0 {
                message.push('\n');
                message.push_str(crate::i18n::text("Даты или права доступа сохранены частично. Подробности указаны в строках журнала."));
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
        self.retry_jobs.retain(|saved| saved.id != job.id);
        self.retry_jobs.push_back(job);
        while self.retry_jobs.len() > 100
            || self
                .retry_jobs
                .iter()
                .map(|job| job.transfer.weight())
                .sum::<usize>()
                > 100_000
        {
            self.retry_jobs.pop_front();
        }
        self.history.retryable = self
            .retry_jobs
            .iter()
            .filter(|job| !job.transfer.needs_inspection())
            .map(|job| job.id.clone())
            .collect();
        self.history.inspectable = self
            .retry_jobs
            .iter()
            .filter(|job| job.transfer.needs_inspection())
            .map(|job| job.id.clone())
            .collect();
    }

    fn remember_undo(&mut self, job: UndoJob) {
        self.undo_jobs.retain(|saved| saved.id != job.id);
        self.undo_jobs.push_back(job);
        while self.undo_jobs.len() > 100
            || self
                .undo_jobs
                .iter()
                .map(|job| job.batch.weight())
                .sum::<usize>()
                > 100_000
        {
            self.undo_jobs.pop_front();
        }
        self.history.undoable = self
            .undo_jobs
            .iter()
            .map(|job| (job.id.clone(), job.batch.len()))
            .collect();
    }

    pub(super) fn undo_operation(&mut self, ctx: &egui::Context, id: &str) {
        if self.pending_jobs.len() >= MAX_PENDING_JOBS {
            self.notice = Some((crate::i18n::text("Очередь заполнена (100 заданий). Дождитесь завершения или удалите ожидающее задание.").into(), true));
            return;
        }
        let Some(index) = self.undo_jobs.iter().position(|job| job.id == id) else {
            return;
        };
        let job = self.undo_jobs.remove(index).unwrap();
        self.history.undoable.remove(id);
        self.show_history = false;
        let mut batch = job.batch.clone();
        let label = crate::i18n::text("Отмена изменений");
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
                    let before = batch.len();
                    let result = batch.restore(progress);
                    let remaining = batch.len();
                    progress.undo = batch;
                    match result {
                        Ok(completed) => Ok(crate::tr_format!(
                            "Отменено изменений: {completed}",
                            "Changes undone: {completed}"
                        )),
                        Err(crate::operations::TransferError::Cancelled) => Ok(crate::tr_format!(
                            "Отмена изменений остановлена. Возвращено: {}. Осталось: {remaining}.",
                            "Undo stopped. Restored: {}. Remaining: {remaining}.",
                            before - remaining
                        )),
                        Err(error) => Err(crate::tr_format!(
                            "{error}. Доступно для следующей попытки: {remaining}.",
                            "{error}. Available for another attempt: {remaining}."
                        )),
                    }
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
        if let Some(job) = self.pending_jobs.remove(index) {
            if let Some(retry) = job.retry_origin {
                self.remember_retry(retry);
            }
            if let Some(undo) = job.undo_origin {
                self.remember_undo(undo);
            }
        }
        if self.pending_jobs.is_empty() {
            self.queue_start_failed = false;
        }
    }

    pub(super) fn poll_transfer(&mut self) {
        while let Some(transfer) = &mut self.transfer {
            match transfer.events.try_recv() {
                Ok(TransferEvent::Conflict(dialog)) => {
                    if !self
                        .job
                        .as_ref()
                        .is_some_and(|job| job.progress.cancel.is_requested())
                    {
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
        if self.transfer.is_none() {
            return;
        }
        if let Some(job) = &self.job {
            if job.progress.cancel.is_requested() {
                return;
            }
            job.progress.cancel.request();
            if let Some(transfer) = &mut self.transfer {
                // Dropping a dialog releases its answer sender too. The worker also
                // observes the token if a conflict event is still in flight.
                transfer.conflict = None;
            }
            ctx.request_repaint();
        }
    }

    pub(super) fn transfer_stop_shortcut(&mut self, ctx: &egui::Context) {
        if self.transfer.is_some()
            && self.job.is_some()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::Escape))
        {
            self.request_transfer_stop(ctx);
        }
    }

    pub(super) fn conflict_open(&self) -> bool {
        self.transfer
            .as_ref()
            .is_some_and(|transfer| transfer.conflict.is_some())
    }

    pub(super) fn poll(&mut self, ctx: &egui::Context) {
        self.poll_transfer();
        if self.conflict_open() && self.dialog_file_focus.is_none() {
            self.remember_dialog_file_focus(ctx);
        }
        if let Some(rx) = &self.application_job {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err(crate::i18n::text("Запуск приложения прервался.").into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.notice = Some(match result {
                    Ok(message) => (message, false),
                    Err(error) => (error, true),
                });
                self.application_job = None;
            }
        }
        if let Some(job) = &self.connection {
            let result = match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text(
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
                            form.error = Some(error);
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
        let automatic_allowed = self.job.is_none() && self.transfer.is_none() && can_reorder;
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
        if let Some(job) = &self.job {
            let result = match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text(
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
                self.transfer = None;
                self.active_cut = None;
                self.job = None;
                self.refresh(ctx);
            }
        }
        self.start_next_job(ctx);
    }

    pub(super) fn move_pending_job(&mut self, index: usize, direction: isize) {
        if let Some(other) = index.checked_add_signed(direction)
            && index < self.pending_jobs.len()
            && other < self.pending_jobs.len()
        {
            self.pending_jobs.swap(index, other);
        }
    }
}
