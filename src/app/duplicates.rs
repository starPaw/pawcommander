use super::{
    comparison::{
        show_progress_with_stop, ComparisonBody, ComparisonDialog, ComparisonResponse, Phase,
    },
    PawCommander,
};
use crate::{
    i18n::text,
    shortcuts::Command,
    vfs::{
        duplicates::{self, SearchResult, TrashPlan},
        Location,
    },
};
use eframe::egui;
use std::collections::BTreeSet;

pub(super) struct DuplicateSearch {
    folder: Location,
    phase: Phase<SearchResult>,
    rows: Vec<(usize, Option<usize>)>,
    keepers: Vec<usize>,
    selected: BTreeSet<(usize, usize)>,
    reviewing: bool,
    focus_start: bool,
    run: u64,
}

impl DuplicateSearch {
    fn new(folder: Location) -> Self {
        Self {
            folder,
            phase: Phase::Ready,
            rows: Vec::new(),
            keepers: Vec::new(),
            selected: BTreeSet::new(),
            reviewing: false,
            focus_start: true,
            run: 0,
        }
    }

    pub(super) fn poll(&mut self) {
        let running = matches!(self.phase, Phase::Running(_));
        self.phase.poll();
        if running && let Phase::Finished(result) = &self.phase {
            self.keepers = vec![0; result.groups.len()];
            self.rows = result
                .groups
                .iter()
                .enumerate()
                .flat_map(|(group, data)| {
                    std::iter::once((group, None))
                        .chain((0..data.members.len()).map(move |member| (group, Some(member))))
                })
                .collect();
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> ComparisonResponse {
        let mut start = false;
        let mut stop = false;
        let mut close = false;
        let mut confirm = false;
        let was_reviewing = self.reviewing;
        let modal = egui::Modal::new(egui::Id::unique("duplicate_search")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(240.0, 760.0));
            ui.heading(Command::FindDuplicates.label());
            let path = self.folder.display().escape_debug().to_string();
            ui.add(egui::Label::new(&path).truncate().selectable(true))
                .on_hover_text(&path);
            ui.separator();
            match &mut self.phase {
                Phase::Ready => {}
                Phase::Running(worker) => show_progress_with_stop(
                    ui,
                    worker,
                    crate::i18n::key!("Ищем дубликаты…"),
                    crate::i18n::key!("Останавливаем поиск дубликатов…"),
                ),
                Phase::Finished(result) => {
                    show_results(
                        ui,
                        result,
                        &self.rows,
                        &mut self.keepers,
                        &mut self.selected,
                        self.run,
                        was_reviewing,
                    );
                }
                Phase::Stopped => {
                    ui.label(text!("Поиск дубликатов остановлен"));
                }
                Phase::Failed(error) => {
                    let message =
                        egui::RichText::new(error.as_str()).color(ui.visuals().error_fg_color);
                    ui.add(egui::Label::new(message).wrap().selectable(true));
                }
            }
            ui.add_space(8.0);
            if was_reviewing {
                let count = self.selected.len();
                ui.label(crate::tr_format!(
                    "Move the marked files to the system Trash? Selected: {count}.",
                    count = count
                ));
                ui.horizontal(|ui| {
                    if ui.button(text!("Назад")).clicked() {
                        self.reviewing = false;
                    }
                    confirm = ui.button(text!("В корзину")).clicked();
                });
            } else {
                ui.horizontal(|ui| {
                    if let Phase::Running(worker) = &self.phase {
                        stop = ui
                            .add_enabled(
                                !worker.progress.cancel.is_requested(),
                                egui::Button::new(text!("Остановить")),
                            )
                            .clicked();
                    } else {
                        let scan = ui.button(text!("Найти дубликаты"));
                        if self.focus_start && !ui.is_sizing_pass() {
                            scan.request_focus();
                            self.focus_start = false;
                        }
                        start = scan.clicked();
                        let count = self.selected.len();
                        let label = crate::tr_format!("Review selected ({count})", count = count);
                        if ui
                            .add_enabled(count > 0, egui::Button::new(label))
                            .clicked()
                        {
                            self.reviewing = true;
                        }
                        close = ui.button(text!("Закрыть")).clicked();
                    }
                });
            }
        });
        if let Phase::Running(worker) = &self.phase {
            if stop || modal.should_close() {
                worker.progress.cancel.request();
            }
        } else if was_reviewing {
            if confirm
                && let Phase::Finished(result) = &self.phase
                && let Some(plan) = result.trash_plan(&self.keepers, &self.selected)
            {
                return ComparisonResponse::TrashDuplicates(Box::new(plan));
            }
            if modal.should_close() {
                self.reviewing = false;
            }
        } else if close || modal.should_close() {
            return ComparisonResponse::Close;
        } else if start {
            self.selected.clear();
            self.rows.clear();
            self.keepers.clear();
            self.run = self.run.wrapping_add(1);
            let folder = self.folder.clone();
            self.phase = Phase::start(ctx, move |progress| duplicates::find(&folder, progress));
        }
        ComparisonResponse::Open
    }
}

fn show_results(
    ui: &mut egui::Ui,
    result: &SearchResult,
    rows: &[(usize, Option<usize>)],
    keepers: &mut [usize],
    selected: &mut BTreeSet<(usize, usize)>,
    run: u64,
    reviewing: bool,
) {
    let details_top = ui.next_widget_position().y;
    if result.skipped > 0 {
        if !result.groups.is_empty() {
            ui.label(text!(
                "Результаты относятся к файлам, которые удалось проверить."
            ));
        }
        show_skipped(ui, result, run);
    }
    if result.groups.is_empty() {
        ui.label(if result.skipped == 0 {
            text!("Дубликаты не найдены")
        } else {
            text!("Среди проверенных файлов дубликаты не найдены")
        });
        return;
    }
    let details_height = ui.next_widget_position().y - details_top;
    ui.label(text!(
        "Оставьте один файл в каждой группе и отметьте копии для корзины.",
    ));
    let row_height = ui
        .spacing()
        .interact_size
        .y
        .max(ui.text_style_height(&egui::TextStyle::Body));
    let height = (ui.ctx().content_rect().height() * 0.52 - details_height).clamp(90.0, 420.0);
    egui::ScrollArea::both()
        .id_salt(("duplicate-rows", run))
        .max_height(height)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, rows.len(), |ui, range| {
            for (group, member) in rows[range].iter().copied() {
                let data = &result.groups[group];
                ui.push_id((group, member), |ui| {
                    let Some(member) = member else {
                        ui.strong(crate::tr_format!(
                            "{} files · {} bytes",
                            data.members.len(),
                            data.size
                        ));
                        return;
                    };
                    let key = (group, member);
                    let original = keepers[group] == member;
                    let mut checked = selected.contains(&key);
                    let path = data.members[member]
                        .path
                        .to_string_lossy()
                        .escape_debug()
                        .to_string();
                    ui.horizontal(|ui| {
                        let keep = ui.add_enabled(
                            !reviewing,
                            egui::RadioButton::new(original, text!("Оставить")),
                        );
                        if keep.clicked() {
                            keepers[group] = member;
                            selected.remove(&key);
                            checked = false;
                        }
                        let mark = ui.add_enabled(
                            !reviewing && keepers[group] != member,
                            egui::Checkbox::new(&mut checked, text!("В корзину")),
                        );
                        if mark.changed() {
                            if checked {
                                selected.insert(key);
                            } else {
                                selected.remove(&key);
                            }
                        }
                        let label = ui.add(egui::Label::new(path).extend().selectable(true));
                        keep.labelled_by(label.id);
                        mark.labelled_by(label.id);
                    });
                });
            }
        });
}

fn show_skipped(ui: &mut egui::Ui, result: &SearchResult, run: u64) {
    let skipped = result.skipped;
    let summary = crate::tr_format!(
        "Skipped unavailable or changed entries: {skipped}",
        skipped = skipped
    );
    egui::CollapsingHeader::new(summary)
        .id_salt(("duplicate-skipped", run))
        .show(ui, |ui| {
            let shown = result.examples.len();
            if shown < skipped {
                ui.weak(crate::tr_format!(
                    "Showing {shown} of {skipped} skipped entries.",
                    shown = shown,
                    skipped = skipped
                ));
            }
            egui::ScrollArea::vertical()
                .id_salt(("duplicate-skipped-details", run))
                .max_height(125.0)
                .show(ui, |ui| {
                    for (location, error) in &result.examples {
                        let message = crate::i18n::duplicate_search_skip(location, error);
                        ui.add(egui::Label::new(message).wrap().selectable(true));
                    }
                });
        });
}

impl PawCommander {
    pub(super) fn can_find_duplicates(&self) -> bool {
        !self.conflict_open() && self.tab().path.local().is_some()
    }

    pub(super) fn open_duplicates(&mut self, return_focus: Option<egui::Id>, ctx: &egui::Context) {
        if self.comparison.is_none() && self.can_find_duplicates() {
            self.comparison = Some(ComparisonDialog::new(
                ComparisonBody::Duplicates(DuplicateSearch::new(self.tab().path.clone())),
                return_focus,
                ctx,
            ));
        }
    }

    pub(super) fn enqueue_duplicate_trash(&mut self, plan: TrashPlan, ctx: &egui::Context) {
        let label = text!("Перемещение дубликатов в корзину");
        let description = format!("{label} ({}) · {}", plan.count(), plan.root().display());
        self.start_typed_stoppable_job(ctx, label, description, move |progress| {
            let report = plan.apply(progress)?;
            Ok(crate::i18n::duplicate_trash_summary(
                report.completed,
                report.cleanup_warnings,
                report.cancelled,
            ))
        });
    }
}
