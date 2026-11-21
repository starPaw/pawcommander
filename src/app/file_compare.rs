use super::{
    comparison::{show_pair, show_progress, ComparisonBody, ComparisonDialog, Phase},
    footer::human_size,
    PawCommander,
};
use crate::{
    i18n::text,
    shortcuts::Command,
    theme::Palette,
    vfs::{
        compare_files, DiffKind, DiffRow, FileComparison, FileDifference, Location, TextDifference,
    },
};
use eframe::egui::{self, Key, RichText};
use std::{path::PathBuf, sync::mpsc, thread};

pub(super) struct FileComparisonDialog {
    files: [Location; 2],
    phase: Phase<FileComparison>,
    tool: Option<PathBuf>,
    external: External,
    show_matches: bool,
    run: u64,
}

enum External {
    Ready,
    Opening(mpsc::Receiver<Result<(), String>>),
    HandedOff,
    Failed(String),
}

impl FileComparisonDialog {
    fn new(files: [Location; 2], tool: Option<PathBuf>, ctx: &egui::Context) -> Self {
        let mut comparison = Self {
            files,
            phase: Phase::Ready,
            tool,
            external: External::Ready,
            show_matches: false,
            run: 0,
        };
        comparison.start(ctx);
        comparison
    }

    fn start(&mut self, ctx: &egui::Context) {
        let files = self.files.clone();
        self.run = self.run.wrapping_add(1);
        self.phase = Phase::start(ctx, move |progress| compare_files(&files, progress));
    }

    pub(super) fn poll(&mut self) {
        self.phase.poll();
        let External::Opening(result) = &self.external else {
            return;
        };
        self.external = match result.try_recv() {
            Ok(Ok(())) => External::HandedOff,
            Ok(Err(error)) => External::Failed(error),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                External::Failed(text("Не удалось запустить внешний инструмент сравнения").into())
            }
        };
    }

    fn open_external(&mut self, ctx: &egui::Context) {
        let (Some(tool), Some(left), Some(right)) = (
            self.tool.clone(),
            self.files[0].local(),
            self.files[1].local(),
        ) else {
            return;
        };
        let files = [left.to_owned(), right.to_owned()];
        let (send, result) = mpsc::channel();
        let ctx = ctx.clone();
        self.external = match thread::Builder::new()
            .name("pawcommander-diff-tool".into())
            .spawn(move || {
                crate::tools::launch_diff(&tool, &files, |result| {
                    let _ = send.send(result);
                    ctx.request_repaint();
                });
            }) {
            Ok(_) => External::Opening(result),
            Err(error) => External::Failed(crate::tr_format!(
                "Не удалось запустить внешний инструмент: {error}",
                "Could not launch the external tool: {error}"
            )),
        };
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> bool {
        let mut start = false;
        let mut close = false;
        let mut stop = false;
        let mut external = false;
        let modal = egui::Modal::new(egui::Id::unique("file_comparison")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(200.0, 820.0));
            ui.heading(Command::CompareFiles.label());
            show_pair(ui, &self.files, ["Левая панель", "Правая панель"]);
            ui.separator();
            match &mut self.phase {
                Phase::Ready => {}
                Phase::Running(worker) => show_progress(ui, worker, "Сравниваем файлы…"),
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
            match &self.external {
                External::Opening(_) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(text("Открываем внешний инструмент…"));
                    });
                }
                External::HandedOff => {
                    ui.label(text("Сравнение передано внешнему инструменту"));
                }
                External::Failed(error) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                External::Ready => {}
            }
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                if let Phase::Running(worker) = &self.phase {
                    stop = ui
                        .add_enabled(
                            !worker.progress.cancel.is_requested(),
                            egui::Button::new(text("Остановить")),
                        )
                        .clicked();
                } else {
                    start = ui.button(text("Сравнить снова")).clicked();
                    close = ui.button(text("Закрыть")).clicked();
                }
                let local = self.files.iter().all(|file| file.local().is_some());
                let available =
                    local && self.tool.is_some() && !matches!(self.external, External::Opening(_));
                let response = ui.add_enabled(
                    available,
                    egui::Button::new(text("Открыть во внешнем инструменте")),
                );
                let response = if !local {
                    response.on_disabled_hover_text(text(
                        "Внешнему инструменту нужны два локальных файла",
                    ))
                } else if let Some(tool) = &self.tool {
                    response.on_hover_text(tool.display().to_string())
                } else {
                    response
                        .on_disabled_hover_text(text("Укажите инструмент сравнения в настройках"))
                };
                external = response.clicked();
            });
        });
        if external {
            self.open_external(ctx);
        }
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

fn show_result(ui: &mut egui::Ui, result: &FileComparison, show_matches: &mut bool, run: u64) {
    let [left, right] = result.sizes.map(human_size);
    ui.label(crate::tr_format!(
        "Слева {left} · справа {right}",
        "Left {left} · right {right}"
    ));
    if let Some(offset) = result.first_difference {
        ui.label(crate::tr_format!(
            "Первое различие в байтах на 0x{offset:X}",
            "First byte difference at 0x{offset:X}"
        ));
    }
    match &result.difference {
        FileDifference::Equal => {
            ui.label(text("Файлы совпадают побайтно"));
        }
        FileDifference::Binary => {
            ui.label(text("Бинарное сравнение"));
        }
        FileDifference::TextSizeLimit => {
            ui.label(text("Построчное сравнение доступно для файлов до 2 МиБ"));
        }
        FileDifference::TextLineLimit => {
            ui.label(text(
                "Построчное сравнение доступно для файлов до 30 000 строк",
            ));
        }
        FileDifference::Text(diff) => show_text(ui, diff, show_matches, run),
    }
}

fn show_text(ui: &mut egui::Ui, diff: &TextDifference, show_matches: &mut bool, run: u64) {
    let has_changes = diff.added + diff.removed > 0;
    if has_changes {
        let (added, removed) = (diff.added, diff.removed);
        ui.label(crate::tr_format!(
            "Добавлено {added} · удалено {removed}",
            "Added {added} · removed {removed}"
        ));
    } else {
        ui.label(text("Строки текста совпадают"));
    }
    if diff.grouped {
        ui.label(text("Большое изменение показано одним блоком"));
    }
    ui.checkbox(show_matches, text("Показывать совпадения"));
    if !has_changes && !*show_matches {
        return;
    }
    let count = if *show_matches {
        diff.lines.len()
    } else {
        diff.context.len()
    };
    let row_height = ui
        .text_style_height(&egui::TextStyle::Monospace)
        .max(ui.spacing().interact_size.y);
    let height = (ui.ctx().content_rect().height() * 0.50).clamp(65.0, 400.0);
    let colors = Palette::from_ctx(ui.ctx());
    egui::ScrollArea::both()
        .id_salt(("file-diff", run, *show_matches))
        .max_height(height)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, count, |ui, rows| {
            for index in rows {
                let row = if *show_matches {
                    DiffRow::Line(index)
                } else {
                    diff.context[index]
                };
                let value = match row {
                    DiffRow::Gap(count) => RichText::new(crate::tr_format!(
                        "… совпадающих строк {count}",
                        "… {count} matching lines"
                    ))
                    .monospace()
                    .weak(),
                    DiffRow::Line(index) => {
                        let line = &diff.lines[index];
                        let (sign, color) = match line.kind {
                            DiffKind::Same => (' ', colors.ink),
                            DiffKind::Removed => ('-', colors.error),
                            DiffKind::Added => ('+', colors.success),
                        };
                        let left = line
                            .left
                            .map(|number| number.to_string())
                            .unwrap_or_default();
                        let right = line
                            .right
                            .map(|number| number.to_string())
                            .unwrap_or_default();
                        let end = line.text.floor_char_boundary(4_096);
                        let tail = if end < line.text.len() { "…" } else { "" };
                        let value =
                            format!("{left:>5} {right:>5} {sign} {}{tail}", &line.text[..end]);
                        RichText::new(value).monospace().color(color)
                    }
                };
                ui.horizontal(|ui| {
                    ui.set_min_height(row_height);
                    ui.add(egui::Label::new(value).extend().selectable(true));
                });
            }
        });
}

impl PawCommander {
    pub(super) fn files_to_compare(&self) -> Option<[Location; 2]> {
        if !self.settings.split || self.conflict_open() {
            return None;
        }
        let selected = |side: usize| {
            self.panes[side]
                .tab()
                .selected_regular_file(self.folder_view(side))
        };
        Some([selected(0)?, selected(1)?])
    }

    pub(super) fn open_file_comparison(
        &mut self,
        return_focus: Option<egui::Id>,
        ctx: &egui::Context,
    ) {
        if self.comparison.is_none()
            && let Some(files) = self.files_to_compare()
        {
            let tool = self.tools.diff_tool(&self.settings.diff_tool);
            self.comparison = Some(ComparisonDialog::new(
                ComparisonBody::Files(FileComparisonDialog::new(files, tool, ctx)),
                return_focus,
                ctx,
            ));
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
