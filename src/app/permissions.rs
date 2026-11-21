use crate::i18n::{file_error, text};
use crate::operations::{Cancellation, Progress, TransferError};
use crate::vfs::{Location, ModeRule, PermissionsPreview};
use eframe::egui;
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub(super) struct PermissionsDialog {
    sources: Vec<Location>,
    draft: String,
    pending: Option<Pending>,
    preview: Option<PermissionsPreview>,
    error: Option<String>,
}

struct Pending {
    cancel: Cancellation,
    receive: Receiver<Result<PermissionsPreview, TransferError>>,
}

impl PermissionsDialog {
    pub(super) fn new(sources: Vec<Location>) -> Self {
        Self {
            sources,
            draft: String::new(),
            pending: None,
            preview: None,
            error: None,
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui, focus: bool) {
        self.poll();
        ui.horizontal(|ui| {
            let label = ui.label(text!("Права доступа"));
            let edit = ui
                .add_enabled(
                    self.pending.is_none(),
                    egui::TextEdit::singleline(&mut self.draft)
                        .hint_text("0644, u+rw,go-w")
                        .char_limit(256)
                        .desired_width(220.0),
                )
                .labelled_by(label.id);
            if focus {
                edit.request_focus();
            }
            if edit.changed() {
                self.preview = None;
                self.error = None;
            }
            if ui
                .add_enabled(
                    self.pending.is_none() && !self.draft.trim().is_empty(),
                    egui::Button::new(text!("Предпросмотр")),
                )
                .clicked()
            {
                self.start(ui.ctx());
            }
        });
        ui.label(text!(
            "Если класс не указан, правило применяется ко всем (a).",
        ));

        if self.pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(text!("Чтение прав SFTP…"));
            });
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(preview) = &self.preview {
            ui.label(crate::tr_format!(
                "Changes for {} of {} items",
                preview.changes,
                preview.rows.len()
            ));
            let row_height = ui.text_style_height(&egui::TextStyle::Body);
            egui::ScrollArea::vertical().max_height(250.0).show_rows(
                ui,
                row_height,
                preview.rows.len(),
                |ui, rows| {
                    for row in &preview.rows[rows] {
                        ui.columns(2, |columns| {
                            columns[0]
                                .add(egui::Label::new(row.source.name()).truncate())
                                .on_hover_text(row.source.display());
                            match &row.change {
                                Ok(change) => {
                                    columns[1].monospace(format!(
                                        "{:04o} → {:04o}",
                                        change.current(),
                                        change.after
                                    ));
                                }
                                Err(error) => {
                                    let error = file_error(error);
                                    let label = egui::RichText::new(&error)
                                        .color(columns[1].visuals().error_fg_color);
                                    columns[1]
                                        .add(egui::Label::new(label).truncate())
                                        .on_hover_text(error);
                                }
                            }
                        });
                    }
                },
            );
        }
    }

    pub(super) fn can_apply(&self) -> bool {
        self.preview.as_ref().is_some_and(|plan| plan.changes > 0)
    }

    pub(super) fn into_plan(mut self) -> Option<PermissionsPreview> {
        self.preview.take().filter(|plan| plan.changes > 0)
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.preview = None;
        self.error = None;
        let rule = match ModeRule::parse(&self.draft) {
            Ok(rule) => rule,
            Err(error) => {
                self.error = Some(file_error(&error));
                return;
            }
        };
        let sources = self.sources.clone();
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let ctx = ctx.clone();
        let (send, receive) = mpsc::channel();
        match std::thread::Builder::new()
            .name("pawcommander-permissions".into())
            .spawn(move || {
                let _ = send.send(PermissionsPreview::load(&sources, &rule, &worker_cancel));
                ctx.request_repaint();
            }) {
            Ok(_) => self.pending = Some(Pending { cancel, receive }),
            Err(error) => {
                self.error = Some(crate::tr_format!(
                    "Could not start the permissions preview. {}",
                    error
                ))
            }
        }
    }

    fn poll(&mut self) {
        let Some(pending) = &self.pending else {
            return;
        };
        match pending.receive.try_recv() {
            Ok(Ok(preview)) => self.preview = Some(preview),
            Ok(Err(error)) => self.error = Some(file_error(&error)),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.error =
                    Some(text!("Предпросмотр прав прервался. Запустите его снова.").into());
            }
        }
        self.pending = None;
    }
}

pub(super) fn apply(plan: PermissionsPreview, progress: &mut Progress) -> Result<String, String> {
    let report = match plan.apply(progress) {
        Ok(report) => report,
        Err(TransferError::Cancelled) => {
            return Ok(text!("Операция остановлена пользователем").into())
        }
        Err(error) => return Err(file_error(&error)),
    };
    let mut summary = crate::tr_format!(
        "Changed {}. With warnings {}. Skipped {}. Failed {}.",
        report.completed,
        report.warned,
        report.skipped,
        report.failed
    );
    if report.cancelled {
        summary = format!("{}. {summary}", text!("Операция остановлена пользователем"));
    }
    match report.error {
        Some(error) => Err(format!("{summary} {}", file_error(&error))),
        None => Ok(summary),
    }
}

impl Drop for PermissionsDialog {
    fn drop(&mut self) {
        if let Some(pending) = &self.pending {
            pending.cancel.request();
        }
    }
}
