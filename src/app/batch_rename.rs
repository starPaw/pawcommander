use crate::i18n::text;
use crate::operations::Cancellation;
use crate::theme::Palette;
use crate::vfs::{BatchRenamePreview, BatchRenameRules, Location};
use eframe::egui;
use std::sync::{mpsc, Arc};

pub(super) struct BatchRenameDialog {
    sources: Arc<[Location]>,
    rules: BatchRenameRules,
    work: Option<PreviewWork>,
    preview: Option<Result<BatchRenamePreview, String>>,
}

struct PreviewWork {
    cancel: Cancellation,
    result: mpsc::Receiver<Result<BatchRenamePreview, String>>,
}

impl Drop for PreviewWork {
    fn drop(&mut self) {
        self.cancel.request();
    }
}

impl BatchRenameDialog {
    pub(super) fn new(sources: Vec<Location>) -> Self {
        Self {
            sources: sources.into(),
            rules: BatchRenameRules::default(),
            work: None,
            preview: None,
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui, focus: bool) {
        self.poll_preview();
        let rules = &mut self.rules;
        let mut changed = false;
        ui.add_enabled_ui(self.work.is_none(), |ui| {
            egui::Grid::new("batch_rename_rules")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    let template =
                        rule_field(ui, text!("Шаблон нового имени"), &mut rules.template);
                    if focus {
                        template.request_focus();
                    }
                    changed |= template.changed();
                    changed |=
                        rule_field(ui, text!("Поиск (регулярное выражение)"), &mut rules.search)
                            .changed();
                    changed |= rule_field(
                        ui,
                        text!("Замена (группы $1, $2, …)"),
                        &mut rules.replacement,
                    )
                    .changed();
                    let label = ui.label(text!("Счётчик с"));
                    changed |= ui
                        .horizontal(|ui| {
                            let start = ui
                                .add(egui::DragValue::new(&mut rules.start).range(0..=999_999_999))
                                .labelled_by(label.id);
                            let digits = ui.label(text!("Минимум цифр"));
                            let width = ui
                                .add(egui::DragValue::new(&mut rules.digits).range(1..=12))
                                .labelled_by(digits.id);
                            start.changed() || width.changed()
                        })
                        .inner;
                    ui.end_row();
                });
        });
        if changed {
            self.preview = None;
        }
        ui.weak(text!("{stem} — основа имени, {ext} — расширение с точкой, {n} — счётчик. Регулярное выражение изменяет основу имени."));
        ui.add_space(8.0);
        if ui
            .add_enabled(
                self.work.is_none(),
                egui::Button::new(text!("Проверить имена")),
            )
            .clicked()
        {
            self.start_preview(ui.ctx());
        }
        if self.work.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(text!("Проверка новых имён…"));
            });
        }
        match &self.preview {
            Some(Ok(preview)) => show_preview(ui, preview),
            Some(Err(error)) => {
                ui.colored_label(Palette::from_ctx(ui.ctx()).error, error);
            }
            None => {}
        }
    }

    pub(super) fn can_apply(&self) -> bool {
        matches!(&self.preview, Some(Ok(preview)) if self.work.is_none() && preview.can_apply())
    }

    pub(super) fn into_plan(self) -> Option<BatchRenamePreview> {
        self.preview.and_then(Result::ok)
    }

    fn start_preview(&mut self, ctx: &egui::Context) {
        self.preview = None;
        let sources = self.sources.clone();
        let rules = self.rules.clone();
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (sender, result) = mpsc::channel();
        let ctx = ctx.clone();
        match std::thread::Builder::new()
            .name("pawcommander-rename-preview".into())
            .spawn(move || {
                let result = BatchRenamePreview::build(&sources, &rules, &worker_cancel)
                    .map_err(|error| crate::i18n::file_error(&error));
                let _ = sender.send(result);
                ctx.request_repaint();
            }) {
            Ok(_) => self.work = Some(PreviewWork { cancel, result }),
            Err(error) => self.preview = Some(Err(error.to_string())),
        }
    }

    fn poll_preview(&mut self) {
        let Some(work) = &self.work else { return };
        let result = match work.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(text!("Проверка новых имён завершилась неожиданно.").into())
            }
        };
        self.work = None;
        self.preview = Some(result);
    }
}

fn rule_field(ui: &mut egui::Ui, title: &str, value: &mut String) -> egui::Response {
    let label = ui.label(title);
    let response = ui
        .add(
            egui::TextEdit::singleline(value)
                .char_limit(1024)
                .desired_width(315.0),
        )
        .labelled_by(label.id);
    ui.end_row();
    response
}

fn show_preview(ui: &mut egui::Ui, preview: &BatchRenamePreview) {
    let changes = preview.changes;
    let conflicts = preview.conflicts;
    ui.label(crate::tr_format!(
        "{changes} changed names · {conflicts} conflicts",
        changes = changes,
        conflicts = conflicts
    ));
    ui.columns(2, |columns| {
        columns[0].strong(text!("Текущее имя"));
        columns[1].strong(text!("Новое имя"));
    });
    const HEIGHT: f32 = 27.0;
    let error_color = Palette::from_ctx(ui.ctx()).error;
    egui::ScrollArea::vertical()
        .id_salt("batch_rename_names")
        .max_height(205.0)
        .show_rows(ui, HEIGHT, preview.rows.len(), |ui, range| {
            ui.spacing_mut().interact_size.y = HEIGHT;
            let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
            for row in &preview.rows[range] {
                ui.horizontal(|ui| {
                    let source = row.source.name();
                    ui.add_sized([width, HEIGHT], egui::Label::new(&source).truncate())
                        .on_hover_text(source);
                    let name = row.name.as_deref().unwrap_or("—");
                    let content = match &row.problem {
                        Some(error) => {
                            egui::RichText::new(format!("{name} · {error}")).color(error_color)
                        }
                        None => egui::RichText::new(name),
                    };
                    let help = content.text().to_string();
                    ui.add_sized([width, HEIGHT], egui::Label::new(content).truncate())
                        .on_hover_text(help);
                });
            }
        });
}
