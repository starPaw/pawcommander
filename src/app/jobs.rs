use super::dialogs::Dialog;
use super::directory::{FolderVisit, AUTO_REFRESH_INTERVAL};
use super::state::FolderMode;
use super::PawCommander;
use crate::conflict::{Answer, ConflictDialog, Decision};
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
}

pub(super) struct TransferInteraction {
    pub(super) events: Receiver<TransferEvent>,
    pub(super) cut_generation: Option<u64>,
    pub(super) conflict: Option<Box<ConflictDialog>>,
}

pub(super) struct Job {
    pub(super) label: String,
    pub(super) started: Instant,
    pub(super) result: Receiver<Result<String, String>>,
    pub(super) progress: ProgressView,
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
            },
        );
    }

    pub(super) fn submit_job(&mut self, ctx: &egui::Context, job: PendingJob) {
        if self.pending_jobs.len() >= MAX_PENDING_JOBS {
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
                ..
            }) = self.pending_jobs.pop_front()
            else {
                break;
            };
            debug_assert!(self.transfer.is_none());
            let (tx, rx) = mpsc::channel();
            let worker_ctx = ctx.clone();
            let (mut reporter, progress) = Progress::tracked();
            let started = Instant::now();
            let history = self.history.sender();
            let history_label = label.clone();
            let recovery_directory = self.recovery.directory();
            let recovery_description = description.clone();
            // A failed OS spawn consumes its closure. Retain the work separately
            // so failure cannot discard this job or the jobs behind it.
            let work = Arc::new(Mutex::new(Some(work)));
            let worker_work = work.clone();
            let worker = move || {
                let work = worker_work.lock().unwrap().take().unwrap();
                let history_started = crate::result_history::now();
                if recovery_directory.is_none() && !cfg!(test) {
                    let _ = tx.send(Err(crate::tr_format!(
                        "Операция не начата: папка данных приложения для восстановления недоступна.",
                        "Operation not started: application data directory for recovery is unavailable."
                    )));
                    worker_ctx.request_repaint();
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
                        let _ = tx.send(Err(crate::tr_format!(
                            "Операция не начата: не удалось сохранить запись восстановления: {error}",
                            "Operation not started: could not save recovery record: {error}"
                        )));
                        worker_ctx.request_repaint();
                        return;
                    }
                };
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&mut reporter)));
                let panicked = outcome.is_err();
                let mut result = outcome.unwrap_or_else(|_| {
                    Err(crate::i18n::text(
                        "Фоновая операция неожиданно завершилась. Проверьте файлы в обеих папках.",
                    )
                    .into())
                });
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
                self.pending_jobs.push_front(PendingJob {
                    label,
                    description,
                    work: work.lock().unwrap().take().unwrap(),
                    transfer,
                    cut,
                });
                self.queue_start_failed = true;
                self.notice = Some((
                    crate::tr_format!(
                        "Не удалось запустить операцию: {error}",
                        "Could not start operation: {error}"
                    ),
                    true,
                ));
                break;
            }
            self.job = Some(Job {
                label,
                started,
                result: rx,
                progress,
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
        if sources.is_empty() {
            return;
        }
        if destination.is_archive() || sources.iter().any(Location::is_archive) {
            self.notice = Some((crate::archive::readonly().into(), true));
            return;
        }
        let cut_generation = if from_cut_clipboard {
            self.clipboard
                .as_ref()
                .map(|clipboard| clipboard.generation)
        } else {
            None
        };
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
                self.notice = Some((
                    crate::i18n::text("Эти вырезанные объекты уже ожидают перемещения.").into(),
                    true,
                ));
                return;
            }
        }
        let verify_copies = kind == TransferKind::Copy && self.settings.verify_copies;
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
        let work = move |progress: &mut Progress| {
            progress.set_copy_verification(verify_copies);
            let cancellation = progress.cancellation();
            let summary = fs_ops::transfer_with_conflicts(
                &sources,
                &destination,
                kind,
                progress,
                |conflict| {
                    let (reply, answer) = mpsc::channel();
                    let dialog = ConflictDialog::new(conflict, reply);
                    events
                        .send(TransferEvent::Conflict(Box::new(dialog)))
                        .map_err(|_| crate::i18n::text("Диалог конфликта закрыт"))?;
                    worker_ctx.request_repaint();
                    Ok(wait_for_conflict_answer(&answer, &cancellation))
                },
                |location, covered| {
                    if cut_generation.is_some() {
                        let _ =
                            events.send(TransferEvent::Moved(location.clone(), covered.to_vec()));
                        worker_ctx.request_repaint();
                    }
                },
            )?;
            Ok(crate::i18n::transfer_finished(
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
            ))
        };
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(work),
                transfer: Some(TransferInteraction {
                    events: rx,
                    cut_generation,
                    conflict: None,
                }),
                cut,
            },
        );
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
        if let Some(rx) = &self.editor_job {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text(
                    "Задача открытия в Zed завершилась неожиданно",
                )
                .into())),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.notice = Some(match result {
                    Ok(message) => (message, false),
                    Err(error) => (error, true),
                });
                self.editor_job = None;
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
                    Ok(location) => {
                        self.directory_tree.refresh();
                        if job.save_profile {
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
                let automatic_allowed = automatic_allowed && scanning_tab != Some(tab.id);
                if can_reorder && (!tab.automatic_loading() || automatic_allowed) && tab.poll() {
                    ctx.request_repaint();
                }
                if tab.has_complete_listing {
                    match tab.folder_visit {
                        FolderVisit::CountOnOpen => {
                            self.settings.folder_history.record(&tab.path);
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
