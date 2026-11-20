use super::{
    comparison::{
        show_pair, show_progress, ComparisonBody, ComparisonDialog, ComparisonResponse, Phase,
    },
    PawCommander,
};
use crate::{
    i18n::text,
    shortcuts::Command,
    vfs::{Location, Reconcile, ReconcileConflict, SyncChange, SyncPlan, TwoWayPlan},
};
use eframe::egui::{self, Key};
use std::{path::Path, sync::Arc};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    LeftToRight,
    RightToLeft,
    Both,
}

enum Plan {
    OneWay(Arc<SyncPlan>),
    TwoWay(Box<TwoWayPlan>),
}

struct Preview {
    plan: Plan,
    checked: bool,
}

pub(super) struct FolderSyncDialog {
    folders: [Location; 2],
    direction: Direction,
    delete_extra: bool,
    phase: Phase<Preview>,
    run: u64,
}

impl FolderSyncDialog {
    fn new(folders: [Location; 2]) -> Self {
        Self {
            folders,
            direction: Direction::LeftToRight,
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
        let direction = self.direction;
        if direction == Direction::RightToLeft {
            folders.swap(0, 1);
        }
        let delete_extra = self.delete_extra;
        self.run = self.run.wrapping_add(1);
        self.phase = Phase::start(ctx, move |progress| {
            let plan = if direction == Direction::Both {
                Plan::TwoWay(Box::new(TwoWayPlan::build(folders, progress)?))
            } else {
                Plan::OneWay(Arc::new(SyncPlan::build(folders, delete_extra, progress)?))
            };
            Ok(Preview {
                plan,
                checked: false,
            })
        });
    }

    fn check(&mut self, ctx: &egui::Context, remember: bool) {
        if let Phase::Finished(mut preview) = std::mem::replace(&mut self.phase, Phase::Ready) {
            self.phase = Phase::start(ctx, move |progress| {
                match &mut preview.plan {
                    Plan::OneWay(plan) => plan.verify(progress)?,
                    Plan::TwoWay(plan) if remember => plan.remember(progress)?,
                    Plan::TwoWay(plan) => plan.verify(progress)?,
                }
                preview.checked = true;
                Ok(preview)
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
        let mut remember = false;
        let modal = egui::Modal::new(egui::Id::unique("folder_sync")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(200.0, 740.0));
            ui.heading(Command::SyncFolders.label());
            show_pair(ui, &self.folders);
            let options = (self.direction, self.delete_extra);
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.direction, Direction::LeftToRight, text("Слева направо"));
                    ui.selectable_value(&mut self.direction, Direction::RightToLeft, text("Справа налево"));
                    ui.selectable_value(&mut self.direction, Direction::Both, text("В обе стороны"));
                });
                if self.direction != Direction::Both {
                    ui.checkbox(
                        &mut self.delete_extra,
                        text("Удалять лишние объекты в назначении"),
                    );
                }
            });
            if options != (self.direction, self.delete_extra) {
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
                    if self.direction == Direction::Both {
                        if let Phase::Finished(Preview { plan: Plan::TwoWay(plan), .. }) = &self.phase
                            && plan.can_remember()
                        {
                            remember = ui.button(text("Сохранить общее состояние")).clicked();
                        }
                    } else {
                        apply = ui
                            .add_enabled(
                                matches!(&self.phase, Phase::Finished(Preview { plan: Plan::OneWay(plan), .. }) if !plan.rows.is_empty()),
                                egui::Button::new(text("Применить план")),
                            )
                            .clicked();
                    }
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
        } else if apply
            && let Phase::Finished(Preview {
                plan: Plan::OneWay(plan),
                ..
            }) = &self.phase
        {
            return ComparisonResponse::ApplySync(Arc::clone(plan));
        } else if build {
            self.build(ctx);
        } else if check || remember {
            self.check(ctx, remember);
        }
        ComparisonResponse::Open
    }
}

fn show_preview(ui: &mut egui::Ui, preview: &Preview, run: u64) {
    if preview.checked {
        ui.label(text("План проверен"));
    }
    match &preview.plan {
        Plan::OneWay(plan) => show_one_way(ui, plan, run),
        Plan::TwoWay(plan) => show_two_way(ui, plan, run),
    }
}

fn show_one_way(ui: &mut egui::Ui, plan: &SyncPlan, run: u64) {
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
    show_rows(ui, &plan.rows, run, 112.0, |row| {
        let action = match row.change {
            SyncChange::Copy => "Копировать",
            SyncChange::Replace => "Заменить",
            SyncChange::Delete => "Удалить",
        };
        (action, &row.path, false)
    });
}

fn show_two_way(ui: &mut egui::Ui, plan: &TwoWayPlan, run: u64) {
    ui.label(text(if plan.has_baseline() {
        "Общее состояние сохранено"
    } else {
        "Первое сравнение"
    }));
    if plan.rows.is_empty() {
        ui.label(text("План пуст"));
        return;
    }
    ui.label(crate::tr_format!(
        "Изменения: {} · конфликты: {}",
        "Changes: {} · conflicts: {}",
        plan.rows.len() - plan.conflicts,
        plan.conflicts
    ));
    if !plan.has_baseline() {
        ui.small(text(
            "Для первого общего состояния содержимое папок должно совпадать.",
        ));
    }
    show_rows(ui, &plan.rows, run, 245.0, |row| {
        let (action, conflict) = match row.outcome {
            Reconcile::Change {
                destination,
                change,
            } => (
                match (change, destination == 0) {
                    (SyncChange::Copy, true) => "Копировать налево",
                    (SyncChange::Copy, false) => "Копировать направо",
                    (SyncChange::Replace, true) => "Заменить слева",
                    (SyncChange::Replace, false) => "Заменить справа",
                    (SyncChange::Delete, true) => "Удалить слева",
                    (SyncChange::Delete, false) => "Удалить справа",
                },
                false,
            ),
            Reconcile::Conflict(reason) => (
                match reason {
                    ReconcileConflict::FirstComparison => "Разные версии при первом сравнении",
                    ReconcileConflict::BothChanged => "Конфликт: обе стороны изменены",
                    ReconcileConflict::EditAndDelete => "Конфликт: правка и удаление",
                },
                true,
            ),
        };
        (action, &row.path, conflict)
    });
}

fn show_rows<T>(
    ui: &mut egui::Ui,
    rows: &[T],
    run: u64,
    action_width: f32,
    display: impl Fn(&T) -> (&'static str, &Path, bool),
) {
    let height = (ui.ctx().content_rect().height() * 0.48).clamp(65.0, 380.0);
    let row_height = ui
        .text_style_height(&egui::TextStyle::Body)
        .max(ui.spacing().interact_size.y);
    egui::ScrollArea::both()
        .id_salt(("sync-rows", run))
        .max_height(height)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, rows.len(), |ui, visible| {
            for row in &rows[visible] {
                ui.horizontal(|ui| {
                    let (action, path, conflict) = display(row);
                    let mut action = egui::RichText::new(text(action));
                    if conflict {
                        action = action.color(ui.visuals().error_fg_color);
                    }
                    ui.add_sized(
                        [action_width, row_height],
                        egui::Label::new(action).truncate(),
                    );
                    let path = path.to_string_lossy().escape_debug().to_string();
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
