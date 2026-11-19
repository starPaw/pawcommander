use super::{
    comparison::{show_pair, show_progress, ComparisonBody, ComparisonDialog, Phase},
    PawCommander,
};
use crate::{
    i18n::text,
    shortcuts::Command,
    vfs::{compare_folders, Comparison, ComparisonMode, ComparisonStatus, Location},
};
use eframe::egui::{self, Key, RichText};

pub(super) struct DirectoryComparison {
    folders: [Location; 2],
    mode: ComparisonMode,
    phase: Phase<Comparison>,
    show_matches: bool,
    run: u64,
}

impl DirectoryComparison {
    fn new(folders: [Location; 2]) -> Self {
        Self {
            folders,
            mode: ComparisonMode::Metadata,
            phase: Phase::Ready,
            show_matches: false,
            run: 0,
        }
    }

    pub(super) fn poll(&mut self) {
        self.phase.poll();
    }

    fn start(&mut self, ctx: &egui::Context) {
        let folders = self.folders.clone();
        let mode = self.mode;
        self.run = self.run.wrapping_add(1);
        self.phase = Phase::start(ctx, move |progress| {
            compare_folders(&folders, mode, progress)
        });
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> bool {
        let running = matches!(self.phase, Phase::Running(_));
        let mut start = false;
        let mut close = false;
        let mut stop = false;
        let modal = egui::Modal::new(egui::Id::unique("directory_comparison")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(200.0, 740.0));
            ui.heading(Command::CompareFolders.label());
            show_pair(ui, &self.folders);
            let before = self.mode;
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(
                        &mut self.mode,
                        ComparisonMode::Metadata,
                        text("Размер и время изменения"),
                    );
                    ui.selectable_value(
                        &mut self.mode,
                        ComparisonMode::Contents,
                        text("По содержимому"),
                    );
                });
            });
            if before != self.mode {
                self.phase = Phase::Ready;
            }
            ui.separator();
            match &mut self.phase {
                Phase::Ready => {}
                Phase::Running(worker) => show_progress(ui, worker, "Сравниваем папки…"),
                Phase::Finished(result) => {
                    show_result(ui, result, &mut self.show_matches, self.run)
                }
                Phase::Failed(error) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                Phase::Stopped => {
                    ui.label(text("Сравнение остановлено"));
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if let Phase::Running(worker) = &self.phase {
                    stop = ui
                        .add_enabled(
                            !worker.progress.cancel.is_requested(),
                            egui::Button::new(text("Остановить")),
                        )
                        .clicked();
                } else {
                    start = ui.button(text("Сравнить")).clicked();
                    close = ui.button(text("Закрыть")).clicked();
                }
            });
        });
        if let Phase::Running(worker) = &self.phase {
            if stop || ctx.input(|input| input.key_pressed(Key::Escape)) {
                worker.progress.cancel.request();
            }
        } else if close || modal.should_close() {
            return false;
        } else if start {
            self.start(ctx);
        }
        true
    }
}

fn show_result(ui: &mut egui::Ui, result: &Comparison, show_matches: &mut bool, run: u64) {
    ui.label(crate::tr_format!(
        "Только слева: {} · только справа: {} · различаются: {} · ошибки чтения: {} · совпали: {}",
        "Only left: {} · only right: {} · different: {} · read errors: {} · matched: {}",
        result.left_only,
        result.right_only,
        result.changed,
        result.unreadable,
        result.same
    ));
    if result.same > 0 {
        ui.checkbox(show_matches, text("Показывать совпадения"));
    }
    let count = if *show_matches {
        result.rows.len()
    } else {
        result.differences.len()
    };
    if count == 0 {
        ui.label(text("Различий не найдено"));
        return;
    }
    let height = (ui.ctx().content_rect().height() * 0.48).clamp(65.0, 380.0);
    let row_height = ui
        .text_style_height(&egui::TextStyle::Body)
        .max(ui.spacing().interact_size.y);
    egui::ScrollArea::both()
        .max_height(height)
        .id_salt(("comparison-rows", run, *show_matches))
        .auto_shrink([false, true])
        .show_rows(ui, row_height, count, |ui, visible| {
            for index in visible {
                let index = if *show_matches {
                    index
                } else {
                    result.differences[index]
                };
                let row = &result.rows[index];
                let (label, detail) = match &row.status {
                    ComparisonStatus::LeftOnly => ("Только слева", None),
                    ComparisonStatus::RightOnly => ("Только справа", None),
                    ComparisonStatus::Changed => ("Различаются", None),
                    ComparisonStatus::TypeChanged => ("Разные типы", None),
                    ComparisonStatus::Same => ("Совпали", None),
                    ComparisonStatus::Unreadable(reason) => ("Ошибка чтения", Some(reason)),
                };
                ui.horizontal(|ui| {
                    let color = if detail.is_some() {
                        ui.visuals().error_fg_color
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.add_sized(
                        [128.0, row_height],
                        egui::Label::new(RichText::new(text(label)).color(color)).truncate(),
                    );
                    let path = if row.path.as_os_str().is_empty() {
                        text("Корень сравнения").into()
                    } else {
                        row.path.to_string_lossy().escape_debug().to_string()
                    };
                    let value = if let Some(reason) = detail {
                        format!("{path} · {}", reason.escape_debug())
                    } else {
                        path
                    };
                    ui.add(egui::Label::new(value).extend().selectable(true));
                });
            }
        });
}

impl PawCommander {
    pub(super) fn can_compare_folders(&self) -> bool {
        self.settings.split
            && !self.conflict_open()
            && self.panes[..2]
                .iter()
                .all(|pane| !pane.tab().path.is_archive())
    }

    pub(super) fn open_directory_comparison(&mut self, ctx: &egui::Context) {
        if self.comparison.is_none() && self.can_compare_folders() {
            let folders = [
                self.panes[0].tab().path.clone(),
                self.panes[1].tab().path.clone(),
            ];
            self.comparison = Some(ComparisonDialog::new(
                ComparisonBody::Folders(DirectoryComparison::new(folders)),
                ctx,
            ));
        }
    }
}

#[cfg(test)]
mod tests;
