use super::{
    comparison::ComparisonBody, comparison::ComparisonDialog, comparison::ComparisonResponse,
    PawCommander,
};
use crate::{
    i18n::{display, text},
    volumes::{self, EjectReport, EjectTarget, Volume},
};
use eframe::egui;
use std::{
    io,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

#[derive(Default)]
pub(super) struct VolumeBrowser {
    rows: Vec<Volume>,
    worker: Option<Worker>,
    confirmation: Option<EjectTarget>,
    notice: Option<(String, bool)>,
}

struct Worker {
    reply: mpsc::Receiver<Reply>,
    ejecting: bool,
}

enum Reply {
    Scan(io::Result<Vec<Volume>>),
    Eject(EjectReport),
}

pub(super) fn wait_for_eject() -> &'static str {
    text!("Дождитесь завершения извлечения диска.")
}

impl VolumeBrowser {
    pub(super) fn ejecting(&self) -> bool {
        self.worker.as_ref().is_some_and(|worker| worker.ejecting)
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        ejecting: bool,
        work: impl FnOnce() -> Reply + Send + 'static,
    ) {
        self.notice = None;
        let (sender, reply) = mpsc::channel();
        let ctx = ctx.clone();
        match thread::Builder::new()
            .name("pawcommander-volumes".into())
            .spawn(move || {
                let _ = sender.send(work());
                ctx.request_repaint_of(egui::ViewportId::ROOT);
            }) {
            Ok(_) => self.worker = Some(Worker { reply, ejecting }),
            Err(error) => {
                self.notice = Some((
                    crate::tr_format!("Could not start the volume task ({error}).", error = error),
                    true,
                ));
            }
        }
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        if self.worker.is_none() {
            self.start(ctx, false, || Reply::Scan(volumes::scan()));
        }
    }

    fn poll(&mut self) -> Option<Vec<PathBuf>> {
        let worker = self.worker.as_ref()?;
        let result = match worker.reply.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.worker = None;
                self.rows.clear();
                self.notice = Some((
                    text!("Задача носителей завершилась без результата.").into(),
                    true,
                ));
                return None;
            }
        };
        self.worker = None;
        let (rows, detached) = match result {
            Reply::Scan(rows) => (rows, None),
            Reply::Eject(report) => {
                self.notice = Some(match report.result {
                    Ok(()) => (text!("Диск извлечён.").into(), false),
                    Err(error) => (error, true),
                });
                (report.rows, Some(report.detached))
            }
        };
        match rows {
            Ok(rows) => self.rows = rows,
            Err(error) => {
                self.rows.clear();
                if self.notice.is_none() {
                    self.notice = Some((
                        crate::tr_format!(
                            "Could not read mounted volumes ({error}).",
                            error = error
                        ),
                        true,
                    ));
                }
            }
        }
        detached
    }

    pub(super) fn show(&mut self, ctx: &egui::Context, file_work: bool) -> ComparisonResponse {
        let idle = self.worker.is_none();
        let ejecting = self.ejecting();
        let mut response = ComparisonResponse::Open;
        let modal = egui::Modal::new(egui::Id::unique("mounted-volumes")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(250.0, 580.0));
            ui.heading(text!("Подключённые тома"));
            if file_work {
                ui.colored_label(ui.visuals().warn_fg_color, text!(
                    "Перед извлечением дождитесь завершения активных и ожидающих файловых операций PawCommander."
                ));
            }
            if ejecting {
                ui.label(text!("Извлечение диска…"));
            } else if !idle {
                ui.label(text!("Чтение списка томов…"));
            }
            if let Some((message, error)) = &self.notice {
                if *error {
                    ui.colored_label(ui.visuals().error_fg_color, message);
                } else {
                    ui.label(message);
                }
            }
            if let Some(target) = &self.confirmation {
                let name = target.name.escape_debug();
                ui.label(crate::tr_format!("Eject the disk containing “{name}”?", name = name));
                ui.small(target.path.to_string_lossy().escape_debug().to_string());
                ui.label(text!("macOS отключит все тома на этом диске."));
                let cancel = ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(idle && !file_work, egui::Button::new(text!("Извлечь"))).clicked() {
                        response = ComparisonResponse::EjectVolume(target.clone());
                    }
                    ui.button(text!("Отмена")).clicked()
                }).inner;
                if cancel {
                    self.confirmation = None;
                }
                return;
            }
            egui::ScrollArea::vertical()
                .max_height((ctx.content_rect().height() * 0.56).max(120.0))
                .show(ui, |ui| {
                    if idle && self.rows.is_empty() && self.notice.is_none() {
                        ui.label(text!("Подключённых томов нет."));
                    }
                    for row in &self.rows {
                        ui.push_id(&row.path, |ui| {
                            ui.separator();
                            ui.strong(row.name.escape_debug().to_string());
                            let path = row.path.to_string_lossy().escape_debug().to_string();
                            ui.add(egui::Label::new(&path).truncate().selectable(true)).on_hover_text(&path);
                            let mut details = format!("{} · {}", row.placement.label(), row.filesystem);
                            if !row.protocol.is_empty() && !matches!(row.placement, volumes::Placement::DiskImage) {
                                details.push_str(" · ");
                                details.push_str(&row.protocol);
                            }
                            ui.label(details);
                            ui.label(crate::tr_format!(
                                "{available} available of {capacity}",
                                available = size(row.available),
                                capacity = size(row.capacity)
                            ));
                            ui.horizontal_wrapped(|ui| {
                                if ui.add_enabled(idle, egui::Button::new(text!("Открыть"))).clicked() {
                                    response = ComparisonResponse::OpenVolume(row.path.clone());
                                }
                                if let Some(target) = &row.eject
                                    && ui.add_enabled(idle && !file_work, egui::Button::new(text!("Извлечь…"))).clicked()
                                {
                                    self.confirmation = Some(target.clone());
                                }
                            });
                        });
                    }
                });
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(idle, egui::Button::new(text!("Обновить"))).clicked() {
                    self.refresh(ctx);
                }
                if ui.add_enabled(!ejecting, egui::Button::new(text!("Закрыть"))).clicked() {
                    response = ComparisonResponse::Close;
                }
            });
        });
        if !ejecting && modal.should_close() && self.confirmation.take().is_none() {
            return ComparisonResponse::Close;
        }
        response
    }
}

fn size(bytes: Option<u64>) -> String {
    const TIB: u64 = 1 << 40;
    match bytes {
        Some(bytes) if bytes >= TIB => format!(
            "{} {}",
            display::decimal(bytes as f64 / TIB as f64, 1),
            text!("ТиБ")
        ),
        Some(bytes) => super::footer::human_size(bytes),
        None => "—".into(),
    }
}

impl PawCommander {
    pub(super) fn open_volumes(&mut self, ctx: &egui::Context) {
        self.volumes.confirmation = None;
        self.volumes.refresh(ctx);
        self.comparison = Some(ComparisonDialog::new(ComparisonBody::Volumes, None, ctx));
    }

    pub(super) fn volume_file_work_active(&self) -> bool {
        self.job.is_some() || !self.pending_jobs.is_empty() || self.smb.busy()
    }

    pub(super) fn eject_volume(&mut self, target: EjectTarget, ctx: &egui::Context) {
        if self.volume_file_work_active() || self.volumes.worker.is_some() {
            return;
        }
        self.volumes.confirmation = None;
        self.volumes
            .start(ctx, true, move || Reply::Eject(volumes::eject(&target)));
    }

    pub(super) fn poll_volumes(&mut self, ctx: &egui::Context) {
        let Some(detached) = self.volumes.poll() else {
            return;
        };
        if detached.is_empty() {
            return;
        }
        for pane in &mut self.panes {
            for tab in &mut pane.tabs {
                if tab
                    .path
                    .local()
                    .or_else(|| tab.path.archive_source())
                    .is_some_and(|path| {
                        detached
                            .iter()
                            .any(|mount| mount != Path::new("/") && path.starts_with(mount))
                    })
                {
                    tab.navigate(PathBuf::from("/").into(), ctx);
                }
            }
        }
        self.directory_tree.refresh();
    }
}
