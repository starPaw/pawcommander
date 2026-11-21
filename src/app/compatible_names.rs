use super::{
    comparison::{
        show_progress_with_stop, ComparisonBody, ComparisonDialog, ComparisonResponse, Phase,
    },
    PawCommander,
};
use crate::{
    fs_ops::TransferKind,
    i18n::text,
    shortcuts::Command,
    vfs::{
        compatible_names::{self, Plan},
        Location,
    },
};
use eframe::egui;

pub(super) struct CompatibleNamesDialog {
    selected: Vec<Location>,
    destination: Location,
    kind: TransferKind,
    preview: Phase<Plan>,
}

impl CompatibleNamesDialog {
    pub(super) fn poll(&mut self) {
        self.preview.poll();
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> ComparisonResponse {
        let mut preview = false;
        let mut apply = false;
        let mut close = false;
        let mut stop = false;
        let modal = egui::Modal::new(egui::Id::unique("compatible-transfer")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(240.0, 795.0));
            ui.heading(Command::CompatibleNames.label());
            let folder = self.destination.display().escape_debug().to_string();
            ui.add(egui::Label::new(&folder).truncate().selectable(true))
                .on_hover_text(&folder);
            let count = self.selected.len();
            ui.label(crate::tr_format!(
                "Выбрано объектов: {count}",
                "Selected items: {count}"
            ));
            ui.small(text(
                "Имена будут подготовлены для Windows и FAT с учётом ограничений тома назначения.",
            ));
            let running = matches!(self.preview, Phase::Running(_));
            let before = self.kind;
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.kind, TransferKind::Copy, text("Копировать"));
                    ui.selectable_value(&mut self.kind, TransferKind::Move, text("Переместить"));
                });
            });
            if before != self.kind {
                self.preview = Phase::Ready;
            }
            ui.separator();
            match &mut self.preview {
                Phase::Ready => {}
                Phase::Failed(error) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                Phase::Stopped => {
                    ui.label(text("Предпросмотр остановлен"));
                }
                Phase::Running(worker) => {
                    show_progress_with_stop(ui, worker, "Проверяем имена…", "Остановка…");
                }
                Phase::Finished(plan) => show_plan(ui, plan),
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if let Phase::Running(worker) = &self.preview {
                    stop = ui
                        .add_enabled(
                            !worker.progress.cancel.is_requested(),
                            egui::Button::new(text("Остановить")),
                        )
                        .clicked();
                } else {
                    preview = ui.button(text("Предпросмотр имён")).clicked();
                    if matches!(self.preview, Phase::Finished(_)) {
                        let label = match self.kind {
                            TransferKind::Copy => text("Копировать с этими именами"),
                            TransferKind::Move => text("Переместить с этими именами"),
                        };
                        apply = ui.button(label).clicked();
                    }
                    close = ui.button(text("Отмена")).clicked();
                }
            });
        });
        if let Phase::Running(worker) = &self.preview {
            if stop || modal.should_close() {
                worker.progress.cancel.request();
            }
        } else if close || modal.should_close() {
            return ComparisonResponse::Close;
        } else if apply {
            if let Phase::Finished(plan) = std::mem::replace(&mut self.preview, Phase::Ready) {
                return ComparisonResponse::ApplyCompatibleNames(Box::new(plan));
            }
        } else if preview {
            let destination = self.destination.clone();
            let sources = self.selected.clone();
            let kind = self.kind;
            self.preview = Phase::start(ctx, move |progress| {
                compatible_names::build(&sources, &destination, kind, progress)
            });
        }
        ComparisonResponse::Open
    }
}

fn show_plan(ui: &mut egui::Ui, plan: &Plan) {
    let entries = plan.entries;
    let changes = plan.changes.len();
    ui.strong(crate::tr_format!(
        "Проверено объектов: {entries} · исправлений: {changes}",
        "Items checked: {entries} · name changes: {changes}"
    ));
    if plan.direct {
        ui.small(text(
            "На exFAT файлы появляются в папке назначения по мере копирования.",
        ));
    }
    if plan.changes.is_empty() {
        ui.label(text("Все имена уже совместимы"));
        return;
    }

    let line = ui.text_style_height(&egui::TextStyle::Body);
    let height = line + ui.text_style_height(&egui::TextStyle::Small) + ui.spacing().item_spacing.y;
    ui.columns_const(|[source, target]| {
        source
            .add(egui::Label::new(egui::RichText::new(text("Исходный путь")).strong()).truncate());
        target.add(
            egui::Label::new(egui::RichText::new(text("Путь назначения")).strong()).truncate(),
        );
    });
    let viewport = (ui.ctx().content_rect().height() * 0.46).clamp(72.0, 385.0);
    egui::ScrollArea::vertical()
        .id_salt("compatible-name-changes")
        .max_height(viewport)
        .auto_shrink([false, true])
        .show_rows(ui, height, plan.changes.len(), |ui, range| {
            for change in &plan.changes[range] {
                ui.vertical(|ui| {
                    ui.columns_const(|[source, target]| {
                        let before = change
                            .before
                            .display()
                            .to_string()
                            .escape_debug()
                            .to_string();
                        let after = change
                            .after
                            .display()
                            .to_string()
                            .escape_debug()
                            .to_string();
                        source
                            .add(egui::Label::new(&before).truncate().selectable(true))
                            .on_hover_text(before);
                        target
                            .add(egui::Label::new(&after).truncate().selectable(true))
                            .on_hover_text(after);
                    });
                    let reasons = change
                        .reasons
                        .iter()
                        .map(|reason| reason.label())
                        .collect::<Vec<_>>()
                        .join(" · ");
                    ui.add(
                        egui::Label::new(egui::RichText::new(&reasons).small().weak()).truncate(),
                    )
                    .on_hover_text(reasons);
                });
            }
        });
}

impl PawCommander {
    pub(super) fn can_preview_compatible_names(&self) -> bool {
        self.settings.split
            && self.tab().path.local().is_some()
            && self.panes[1 - self.active].tab().path.local().is_some()
            && !self.selected().is_empty()
            && !self.conflict_open()
    }

    pub(super) fn open_compatible_names(
        &mut self,
        return_focus: Option<egui::Id>,
        ctx: &egui::Context,
    ) {
        if self.comparison.is_some() || !self.can_preview_compatible_names() {
            return;
        }
        let dialog = CompatibleNamesDialog {
            selected: self.selected(),
            destination: self.panes[1 - self.active].tab().path.clone(),
            kind: TransferKind::Copy,
            preview: Phase::Ready,
        };
        self.comparison = Some(ComparisonDialog::new(
            ComparisonBody::CompatibleNames(Box::new(dialog)),
            return_focus,
            ctx,
        ));
    }
}
