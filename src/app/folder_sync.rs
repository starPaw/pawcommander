use super::{
    comparison::{
        show_pair, show_progress, ComparisonBody, ComparisonDialog, ComparisonResponse, Phase,
    },
    PawCommander,
};
use crate::{
    i18n::text,
    shortcuts::Command,
    vfs::{Location, SyncChange, SyncPlan},
};
use eframe::egui::{self, Key};
use std::sync::Arc;

struct Preview {
    plan: Arc<SyncPlan>,
    checked: bool,
}

pub(super) struct FolderSyncDialog {
    folders: [Location; 2],
    left_to_right: bool,
    delete_extra: bool,
    phase: Phase<Preview>,
    run: u64,
}

impl FolderSyncDialog {
    fn new(folders: [Location; 2]) -> Self {
        Self {
            folders,
            left_to_right: true,
            delete_extra: false,
            phase: Phase::Ready,
            run: 0,
        }
    }

    pub(super) fn poll(&mut self) {
        self.phase.poll();
    }

    fn build(&mut self, ctx: &egui::Context) {
        let mut folders = self.folders.clone();
        if !self.left_to_right {
            folders.swap(0, 1);
        }
        let delete_extra = self.delete_extra;
        self.run = self.run.wrapping_add(1);
        self.phase = Phase::start(ctx, move |progress| {
            Ok(Preview {
                plan: Arc::new(SyncPlan::build(folders, delete_extra, progress)?),
                checked: false,
            })
        });
    }

    fn check(&mut self, ctx: &egui::Context) {
        if let Phase::Finished(preview) = &self.phase {
            let plan = Arc::clone(&preview.plan);
            self.phase = Phase::start(ctx, move |progress| {
                plan.verify(progress)?;
                Ok(Preview {
                    plan,
                    checked: true,
                })
            });
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> ComparisonResponse {
        let running = matches!(self.phase, Phase::Running(_));
        let mut build = false;
        let mut check = false;
        let mut stop = false;
        let mut close = false;
        let mut apply = false;
        let modal = egui::Modal::new(egui::Id::unique("folder_sync")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(200.0, 740.0));
            ui.heading(Command::SyncFolders.label());
            show_pair(ui, &self.folders);
            let options = (self.left_to_right, self.delete_extra);
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.left_to_right, true, text("Слева направо"));
                    ui.selectable_value(&mut self.left_to_right, false, text("Справа налево"));
                });
                ui.checkbox(
                    &mut self.delete_extra,
                    text("Удалять лишние объекты в назначении"),
                );
            });
            if options != (self.left_to_right, self.delete_extra) {
                self.phase = Phase::Ready;
            }
            ui.separator();
            match &mut self.phase {
                Phase::Ready => {}
                Phase::Running(worker) => show_progress(ui, worker, "Проверяем папки для плана…"),
                Phase::Finished(preview) => show_preview(ui, preview, self.run),
                Phase::Failed(error) => {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                Phase::Stopped => {
                    ui.label(text("Проверка папок остановлена"));
                }
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
                    build = ui.button(text("Построить план")).clicked();
                    check = ui
                        .add_enabled(
                            matches!(self.phase, Phase::Finished(_)),
                            egui::Button::new(text("Проверить план")),
                        )
                        .clicked();
                    apply = ui
                        .add_enabled(
                            matches!(&self.phase, Phase::Finished(preview) if !preview.plan.rows.is_empty()),
                            egui::Button::new(text("Применить план")),
                        )
                        .clicked();
                    close = ui.button(text("Закрыть")).clicked();
                }
            });
        });
        if let Phase::Running(worker) = &self.phase {
            if stop || ctx.input(|input| input.key_pressed(Key::Escape)) {
                worker.progress.cancel.request();
            }
        } else if close || modal.should_close() {
            return ComparisonResponse::Close;
        } else if apply && let Phase::Finished(preview) = &self.phase {
            return ComparisonResponse::ApplySync(Arc::clone(&preview.plan));
        } else if build {
            self.build(ctx);
        } else if check {
            self.check(ctx);
        }
        ComparisonResponse::Open
    }
}

fn show_preview(ui: &mut egui::Ui, preview: &Preview, run: u64) {
    let plan = &preview.plan;
    if preview.checked {
        ui.label(text("План проверен"));
    }
    if plan.rows.is_empty() {
        ui.label(text("План пуст"));
        return;
    }
    ui.label(crate::tr_format!(
        "Копирования: {} · замены: {} · удаления: {}",
        "Copies: {} · replacements: {} · deletions: {}",
        plan.copies,
        plan.replacements,
        plan.deletions
    ));
    if plan.deletions > 0 && plan.folders()[1].remote() {
        ui.small(text("Удаление из папки SFTP необратимо."));
    }
    let height = (ui.ctx().content_rect().height() * 0.48).clamp(65.0, 380.0);
    let row_height = ui
        .text_style_height(&egui::TextStyle::Body)
        .max(ui.spacing().interact_size.y);
    egui::ScrollArea::both()
        .id_salt(("sync-rows", run))
        .max_height(height)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, plan.rows.len(), |ui, visible| {
            for row in &plan.rows[visible] {
                ui.horizontal(|ui| {
                    let action = match row.change {
                        SyncChange::Copy => "Копировать",
                        SyncChange::Replace => "Заменить",
                        SyncChange::Delete => "Удалить",
                    };
                    ui.add_sized(
                        [112.0, row_height],
                        egui::Label::new(text(action)).truncate(),
                    );
                    let path = row.path.to_string_lossy().escape_debug().to_string();
                    ui.add(egui::Label::new(path).extend().selectable(true));
                });
            }
        });
}

impl PawCommander {
    pub(super) fn enqueue_folder_sync(&mut self, plan: Arc<SyncPlan>, ctx: &egui::Context) {
        let [source, destination] = plan.folders();
        self.start_stoppable_job(
            ctx,
            text("Синхронизация папок"),
            format!("{} → {}", source.display(), destination.display()),
            move |progress| plan.apply(progress),
        );
    }

    pub(super) fn open_folder_sync(&mut self, return_focus: Option<egui::Id>, ctx: &egui::Context) {
        if self.comparison.is_none() && self.can_compare_folders() {
            let folders = [
                self.panes[0].tab().path.clone(),
                self.panes[1].tab().path.clone(),
            ];
            self.comparison = Some(ComparisonDialog::new(
                ComparisonBody::Sync(FolderSyncDialog::new(folders)),
                return_focus,
                ctx,
            ));
        }
    }
}
