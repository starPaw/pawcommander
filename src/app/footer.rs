use super::PawCommander;
use crate::operations::{PauseState, Snapshot};
use crate::theme::{paint_focus, Palette};
use eframe::egui;
use eframe::egui::RichText;
use std::time::Instant;

pub(super) fn operation_details(ui: &mut egui::Ui, progress: &Snapshot, paused: bool) {
    if let Some(item) = &progress.item {
        let name = item.file_name().map_or_else(
            || item.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let phase = if progress.waiting_for_conflict {
            crate::i18n::text!("Ожидается решение")
        } else if progress.verification.is_some() {
            crate::i18n::text!("Проверка SHA-256")
        } else {
            crate::i18n::text!("Текущий объект")
        };
        ui.add(egui::Label::new(format!("{phase}: {name}")).truncate())
            .on_hover_text(item.display().to_string());
    }
    if let Some(verification) = &progress.verification {
        ui.label(crate::tr_format!(
            "Read for verification: {} / {}",
            human_size(verification.read),
            human_size(verification.expected)
        ));
    }
    if let Some(transfer) = &progress.transfer {
        let written = human_size(transfer.written);
        let now = Instant::now();
        ui.horizontal_wrapped(|ui| {
            if let Some(expected) = transfer.expected.filter(|size| transfer.written <= *size) {
                ui.label(format!("{written} / {}", human_size(expected)));
                let fraction = if expected == 0 {
                    if transfer.stream_finished {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    (transfer.written as f64 / expected as f64) as f32
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(100.0)
                        .show_percentage(),
                );
            } else {
                ui.label(crate::tr_format!("Written: {written}", written = written));
            }
            if !paused && let Some(speed) = transfer.bytes_per_second(now) {
                ui.label(crate::tr_format!("{}/s", human_size(speed.round() as u64)));
            }
            if !paused && let Some(seconds) = transfer.remaining_seconds(now) {
                let time = if seconds < 60 {
                    crate::tr_format!("{seconds} s", seconds = seconds)
                } else {
                    crate::tr_format!("{} min {:02} s", seconds / 60, seconds % 60)
                };
                ui.label(crate::tr_format!("File remaining: ≈ {time}", time = time));
            }
            if !paused && transfer.stream_finished {
                ui.label(crate::i18n::text!("Завершение записи…"));
            }
        });
    }
    if progress.total_written > 0 {
        let total = human_size(progress.total_written);
        ui.label(if progress.reading_inputs {
            crate::tr_format!("Read from source files: {total}", total = total)
        } else {
            crate::tr_format!("Total written: {total}", total = total)
        });
    }
}

pub(super) fn human_size(bytes: u64) -> String {
    use crate::i18n::{display, text};

    if bytes < 1024 {
        return format!("{} {}", display::integer(bytes), text!("Б"));
    }
    let (scale, unit) = if bytes < 1024 * 1024 {
        (1024.0, text!("КБ"))
    } else if bytes < 1024 * 1024 * 1024 {
        (1024.0 * 1024.0, text!("МБ"))
    } else {
        (1024.0 * 1024.0 * 1024.0, text!("ГБ"))
    };
    format!("{} {unit}", display::decimal(bytes as f64 / scale, 1))
}

fn scroll_notice(ui: &mut egui::Ui, id: &str, message: RichText, max_height: f32) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(max_height)
        .show(ui, |ui| {
            // Use vertical layout so the first text row does not clip the remaining paths.
            ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                ui.add(egui::Label::new(message).wrap().selectable(true));
            });
        });
}

impl PawCommander {
    pub(super) fn footer(&mut self, ui: &mut egui::Ui) {
        let ctx = &ui.ctx().clone();
        let colors = Palette::from_ctx(ctx);
        let mut stop_requested = false;
        let mut pause_requested = None;
        let mut speed_requested = false;
        let conflict_open = self.conflict_open();
        let mut archive_add_requested = false;
        let history_error = self.history.error.is_some();
        let history_button = |ui: &mut egui::Ui| {
            let response = ui.button(crate::i18n::text!("Журнал результатов"));
            paint_focus(ui, &response, 5);
            if history_error {
                ui.colored_label(
                    colors.error,
                    crate::i18n::text!("Ошибка журнала результатов"),
                );
            }
            response.clicked()
        };
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(colors.bg)
                    .inner_margin(egui::Margin::symmetric(18, 10)),
            )
            .show(ui, |ui| {
                if let Some(job) = &mut self.jobs.current {
                    let stopping = job.progress.cancel.is_requested();
                    let pause = job.progress.pause_state();
                    let paused = pause == Some(PauseState::Paused);
                    ui.horizontal_wrapped(|ui| {
                        self.show_history |= history_button(ui);
                        ui.add_visible(!paused || stopping, egui::Spinner::new());
                        ui.label(crate::tr_format!(
                            "File operations: {} · {} s",
                            job.label,
                            job.started.elapsed().as_secs()
                        ));
                        if let Some(pause) = pause
                            && !stopping
                        {
                            let running = pause == PauseState::Running;
                            let response = ui.add_enabled(
                                !conflict_open,
                                egui::Button::new(if running {
                                    crate::i18n::text!("Пауза")
                                } else {
                                    crate::i18n::text!("Продолжить")
                                }),
                            );
                            paint_focus(ui, &response, 5);
                            if response.clicked() {
                                pause_requested = Some(running);
                            }
                            let speed = ui.add_enabled(
                                !conflict_open,
                                egui::Button::new(crate::i18n::text!("Скорость…")),
                            );
                            paint_focus(ui, &speed, 5);
                            speed_requested = speed.clicked();
                        }
                        if job.transfer.is_some() {
                            let response = ui
                                .add_enabled(
                                    !stopping,
                                    egui::Button::new(if stopping {
                                        crate::i18n::text!("Остановка…")
                                    } else {
                                        crate::i18n::text!("Остановить")
                                    }),
                                )
                                .on_hover_text(crate::i18n::text!(
                                    "Остановить операцию · Shift+Esc",
                                ));
                            paint_focus(ui, &response, 5);
                            stop_requested = response.clicked();
                        }
                        if !stopping {
                            match pause {
                                Some(PauseState::Requested) => {
                                    ui.label(crate::i18n::text!("Приостанавливаем передачу…"));
                                }
                                Some(PauseState::Paused) => {
                                    ui.label(crate::i18n::text!("Передача приостановлена"));
                                }
                                _ => {}
                            }
                        }
                    });
                    operation_details(ui, job.progress.snapshot(), paused);
                    if let Some((message, _)) = &self.notice {
                        egui::ScrollArea::vertical()
                            .id_salt("active_operation_notice")
                            .max_height(80.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(message).color(colors.muted))
                                        .wrap()
                                        .selectable(true),
                                );
                            });
                    }
                } else if let Some((message, error)) = self.notice.clone() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        self.show_history |= history_button(ui);
                        if ui.small_button("×").clicked() {
                            self.notice = None;
                        }
                        let color = if error { colors.error } else { colors.success };
                        scroll_notice(
                            ui,
                            "operation_notice",
                            RichText::new(message).color(color),
                            160.0,
                        );
                    });
                } else {
                    ui.horizontal(|ui| {
                        self.show_history |= history_button(ui);
                        let visible = self.tab().visible(self.folder_view(self.active)).len();
                        ui.label(
                            RichText::new(crate::tr_format!(
                                "{} items  ·  Selected: {}",
                                crate::i18n::display::integer(visible),
                                crate::i18n::display::integer(self.tab().selected.len())
                            ))
                            .color(colors.muted),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.tab().path.is_archive() && self.settings.split {
                                let add = ui
                                    .add_enabled(
                                        !self.archive_add_sources().is_empty(),
                                        egui::Button::new(crate::i18n::text!(
                                            "Добавить из соседней панели…",
                                        )),
                                    )
                                    .on_disabled_hover_text(crate::i18n::text!(
                                        "Выберите файлы или папки в соседней панели",
                                    ));
                                paint_focus(ui, &add, 5);
                                archive_add_requested = add.clicked();
                            } else if !self.tab().path.is_archive() {
                                ui.label(
                                    RichText::new(crate::i18n::text!("Двойной щелчок — открыть"))
                                        .size(11.0)
                                        .color(colors.muted),
                                );
                            }
                        });
                    });
                }
                #[cfg(target_os = "macos")]
                if let Some(message) = &self.system_open_error {
                    let dismissed = ui
                        .with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                            let dismiss = ui
                                .small_button("×")
                                .on_hover_text(crate::i18n::text!("Закрыть"));
                            scroll_notice(
                                ui,
                                "system_open_error",
                                RichText::new(message).color(colors.error),
                                80.0,
                            );
                            dismiss.clicked()
                        })
                        .inner;
                    if dismissed {
                        self.system_open_error = None;
                    }
                }
                self.pending_queue_ui(ui);
            });
        if stop_requested {
            self.request_transfer_stop(ctx);
        } else if let (Some(paused), Some(job)) = (pause_requested, &self.jobs.current) {
            job.progress.set_paused(paused);
            ctx.request_repaint();
        } else if speed_requested {
            self.open_transfer_limits(ctx);
        }
        if archive_add_requested {
            self.action(super::actions::Action::AddToArchive, ctx);
        }
    }

    pub(super) fn pending_queue_ui(&mut self, ui: &mut egui::Ui) {
        if self.jobs.pending.is_empty() {
            self.jobs.start_failed = false;
            return;
        }
        if self.jobs.start_failed {
            let retry = ui.button(crate::i18n::text!("Повторить запуск очереди"));
            paint_focus(ui, &retry, 5);
            if retry.clicked() {
                self.jobs.start_failed = false;
                self.start_next_job(ui.ctx());
            }
        }
        let mut remove = None;
        let mut reorder = None;
        egui::CollapsingHeader::new(crate::tr_format!(
            "Queued: {} · concurrency: 1",
            self.jobs.pending.len()
        ))
        .id_salt("pending_file_jobs")
        .show(ui, |ui| {
            ui.label(crate::i18n::text!("Остановка действует только на текущее задание. Ожидающие задания хранятся до закрытия приложения."));
            egui::ScrollArea::vertical().id_salt("pending_jobs_scroll").max_height(120.0).show(ui, |ui| {
                for (index, job) in self.jobs.pending.iter().enumerate() {
                    ui.push_id(index, |ui| {
                        ui.horizontal(|ui| {
                            let up = ui.add_enabled(index > 0, egui::Button::new("↑")).on_hover_text(crate::i18n::text!("Раньше"));
                            paint_focus(ui, &up, 5);
                            if up.clicked() { reorder = Some((index, -1)); }
                            let down = ui.add_enabled(index + 1 < self.jobs.pending.len(), egui::Button::new("↓")).on_hover_text(crate::i18n::text!("Позже"));
                            paint_focus(ui, &down, 5);
                            if down.clicked() { reorder = Some((index, 1)); }
                            let cancel = ui.button(crate::i18n::text!("Удалить из очереди"));
                            paint_focus(ui, &cancel, 5);
                            if cancel.clicked() { remove = Some(index); }
                            ui.add(egui::Label::new(format!("{}. {}", index + 1, job.description)).truncate()).on_hover_text(&job.description);
                        });
                    });
                }
            });
        });
        if let Some(index) = remove {
            self.remove_pending_job(index);
        } else if let Some((index, direction)) = reorder {
            self.move_pending_job(index, direction);
        }
    }
}
