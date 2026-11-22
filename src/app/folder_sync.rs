use super::{
    comparison::{
        show_pair, show_progress, ComparisonBody, ComparisonDialog, ComparisonResponse, Phase,
    },
    sync_profiles::{Direction, Options, ProfilePicker, SyncProfile},
    PawCommander,
};
use crate::{
    file_error::SyncError,
    i18n::text,
    operations::TransferError,
    shortcuts::Command,
    vfs::{
        Exclusions, Location, Reconcile, ReconcileConflict, SyncChange, SyncPlan, SyncReport,
        TwoWayPlan,
    },
};
use eframe::egui::{self, Key};
use std::path::{Path, PathBuf};

pub(super) enum Plan {
    OneWay(Box<SyncPlan>),
    TwoWay(Box<TwoWayPlan>),
}

impl Plan {
    fn can_apply(&self) -> bool {
        match self {
            Self::OneWay(plan) => !plan.rows.is_empty(),
            Self::TwoWay(plan) => plan.can_apply(),
        }
    }
}

struct Preview {
    plan: Plan,
    checked: bool,
}

enum Review {
    Check,
    Remember,
    Keep(PathBuf, usize),
}

pub(super) struct FolderSyncDialog {
    folders: [Location; 2],
    options: Options,
    profile: ProfilePicker,
    phase: Phase<Preview>,
    run: u64,
}

impl FolderSyncDialog {
    fn new(folders: [Location; 2], options: Options, profile: ProfilePicker) -> Self {
        Self {
            folders,
            options,
            profile,
            phase: Phase::Ready,
            run: 0,
        }
    }

    pub(super) fn poll(&mut self) {
        self.phase.poll();
    }

    fn build(&mut self, ctx: &egui::Context) {
        let mut folders = self.folders.clone();
        let options = self.options.clone();
        let direction = options.direction;
        if direction == Direction::RightToLeft {
            folders.swap(0, 1);
        }
        self.run = self.run.wrapping_add(1);
        self.phase = Phase::start(ctx, move |progress| {
            let exclusions = Exclusions::parse(&options.exclusions)?;
            let plan = if direction == Direction::Both {
                Plan::TwoWay(Box::new(TwoWayPlan::build(folders, exclusions, progress)?))
            } else {
                Plan::OneWay(Box::new(SyncPlan::build(
                    folders,
                    options.delete_extra,
                    exclusions,
                    progress,
                )?))
            };
            Ok(Preview {
                plan,
                checked: false,
            })
        });
    }

    fn review(&mut self, ctx: &egui::Context, action: Review) {
        if let Phase::Finished(mut preview) = std::mem::replace(&mut self.phase, Phase::Ready) {
            self.phase = Phase::start(ctx, move |progress| {
                preview.checked = !matches!(action, Review::Keep(..));
                match (&mut preview.plan, action) {
                    (Plan::OneWay(plan), _) => plan.verify(progress)?,
                    (Plan::TwoWay(plan), Review::Check) => plan.verify(progress)?,
                    (Plan::TwoWay(plan), Review::Remember) => plan.remember(progress)?,
                    (Plan::TwoWay(plan), Review::Keep(path, side)) => {
                        plan.choose(path, side, progress)?
                    }
                }
                Ok(preview)
            });
        }
    }

    pub(super) fn show(
        &mut self,
        ctx: &egui::Context,
        profiles: &mut Vec<SyncProfile>,
    ) -> ComparisonResponse {
        let running = matches!(self.phase, Phase::Running(_));
        let mut build = false;
        let mut review = None;
        let mut stop = false;
        let mut close = false;
        let mut apply = false;
        let modal = egui::Modal::new(egui::Id::unique("folder_sync")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(200.0, 740.0));
            ui.heading(Command::SyncFolders.label());
            let changed = ui.add_enabled_ui(!running, |ui| {
                let pair_changed = self.profile.show(ui, profiles, &mut self.folders, &mut self.options);
                show_pair(ui, &self.folders, [crate::i18n::key!("Левая папка"), crate::i18n::key!("Правая папка")]);
                let direction = self.options.direction;
                let delete_extra = self.options.delete_extra;
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.options.direction, Direction::LeftToRight, text!("Слева направо"));
                    ui.selectable_value(&mut self.options.direction, Direction::RightToLeft, text!("Справа налево"));
                    ui.selectable_value(&mut self.options.direction, Direction::Both, text!("В обе стороны"));
                });
                if self.options.direction != Direction::Both {
                    ui.checkbox(
                        &mut self.options.delete_extra,
                        text!("Удалять лишние объекты в назначении"),
                    );
                }
                let exclusions_changed = show_exclusions(ui, &mut self.options.exclusions);
                pair_changed || direction != self.options.direction
                    || delete_extra != self.options.delete_extra || exclusions_changed
            }).inner;
            if changed {
                self.phase = Phase::Ready;
            }
            ui.separator();
            match &mut self.phase {
                Phase::Ready => {}
                Phase::Running(worker) => show_progress(ui, worker, crate::i18n::key!("Проверяем папки для плана…")),
                Phase::Finished(preview) => review = show_preview(ui, preview, self.run),
                Phase::Failed(error) => {
                    let height = (ctx.content_rect().height() * 0.30).clamp(90.0, 240.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("sync-plan-error", self.run))
                        .max_height(height)
                        .show(ui, |ui| {
                            let message = egui::RichText::new(error.as_str()).color(ui.visuals().error_fg_color);
                            ui.add(egui::Label::new(message).selectable(true).wrap());
                        });
                }
                Phase::Stopped => {
                    ui.label(text!("Проверка папок остановлена"));
                }
            }
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                if let Phase::Running(worker) = &self.phase {
                    stop = ui
                        .add_enabled(
                            !worker.progress.cancel.is_requested(),
                            egui::Button::new(text!("Остановить")),
                        )
                        .clicked();
                } else {
                    build = ui.button(text!("Построить план")).clicked();
                    if ui
                        .add_enabled(
                            matches!(self.phase, Phase::Finished(_)),
                            egui::Button::new(text!("Проверить план")),
                        )
                        .clicked()
                    {
                        review = Some(Review::Check);
                    }
                    if let Phase::Finished(Preview { plan: Plan::TwoWay(plan), .. }) = &self.phase
                        && plan.can_remember()
                        && ui.button(text!("Сохранить общее состояние")).clicked()
                    {
                        review = Some(Review::Remember);
                    }
                    apply = ui
                        .add_enabled(
                            matches!(&self.phase, Phase::Finished(preview) if preview.plan.can_apply()),
                            egui::Button::new(text!("Применить план")),
                        )
                        .clicked();
                    close = ui.button(text!("Закрыть")).clicked();
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
            && let Phase::Finished(preview) = std::mem::replace(&mut self.phase, Phase::Ready)
        {
            return ComparisonResponse::ApplySync(preview.plan);
        } else if build {
            self.build(ctx);
        } else if let Some(action) = review {
            self.review(ctx, action);
        }
        ComparisonResponse::Open
    }
}

fn show_exclusions(ui: &mut egui::Ui, input: &mut String) -> bool {
    let lines = input.lines().filter(|line| !line.trim().is_empty()).count();
    let title = if lines == 0 {
        text!("Исключения").to_owned()
    } else {
        crate::tr_format!("Exclusions ({lines})", lines = lines)
    };
    egui::CollapsingHeader::new(title)
        .id_salt("sync-exclusions")
        .show(ui, |ui| {
            ui.small(text!("Одно имя или относительный путь на строку. Подстановки * и ? работают внутри имени."));
            ui.add(
                egui::TextEdit::multiline(input)
                    .desired_rows(3)
                    .desired_width(ui.available_width())
                    .char_limit(8192)
                    .hint_text("*.tmp\n.git\ncache/previews"),
            ).changed()
        }).body_returned.unwrap_or_default()
}

fn show_preview(ui: &mut egui::Ui, preview: &Preview, run: u64) -> Option<Review> {
    if preview.checked {
        ui.label(text!("План проверен"));
    }
    let excluded = match &preview.plan {
        Plan::OneWay(plan) => plan.excluded_count(),
        Plan::TwoWay(plan) => plan.excluded_count(),
    };
    if excluded > 0 {
        ui.small(crate::tr_format!(
            "Excluded items: {excluded}",
            excluded = excluded
        ));
    }
    match &preview.plan {
        Plan::OneWay(plan) => {
            show_one_way(ui, plan, run);
            None
        }
        Plan::TwoWay(plan) => show_two_way(ui, plan, run),
    }
}

fn show_one_way(ui: &mut egui::Ui, plan: &SyncPlan, run: u64) {
    if plan.rows.is_empty() {
        ui.label(text!("План пуст"));
        return;
    }
    ui.label(crate::tr_format!(
        "Copies: {} · replacements: {} · deletions: {}",
        plan.copies,
        plan.replacements,
        plan.deletions
    ));
    if plan.deletions > 0 && plan.folders()[1].remote() {
        ui.small(text!("Удаление из папки SFTP необратимо."));
    }
    show_rows(ui, &plan.rows, run, |ui, row, height| {
        let action = match row.change {
            SyncChange::Copy => crate::i18n::text!("Копировать"),
            SyncChange::Replace => crate::i18n::text!("Заменить"),
            SyncChange::Delete => crate::i18n::text!("Удалить"),
        };
        show_action(ui, action, false, 112.0, height);
        show_relative_path(ui, &row.path);
    });
}

fn show_two_way(ui: &mut egui::Ui, plan: &TwoWayPlan, run: u64) -> Option<Review> {
    ui.label(if plan.interrupted() {
        text!("Проверка после прерывания синхронизации")
    } else if plan.has_baseline() {
        text!("Общее состояние сохранено")
    } else {
        text!("Первое сравнение")
    });
    if plan.rows.is_empty() {
        ui.label(text!("План пуст"));
        return None;
    }
    ui.label(crate::tr_format!(
        "Changes: {} · conflicts: {}",
        plan.rows.len() - plan.conflicts,
        plan.conflicts
    ));
    if plan.conflicts > 0 {
        ui.small(text!(
            "Для каждого конфликта выберите версию, которую нужно оставить.",
        ));
    }
    if plan.deletes_remote() {
        ui.small(text!("Удаление из папки SFTP необратимо."));
    }
    let mut choice = None;
    show_rows(ui, &plan.rows, run, |ui, row, height| {
        let (action, conflict) = match row.outcome {
            Reconcile::Change {
                destination,
                change,
            } => (
                match (change, destination == 0) {
                    (SyncChange::Copy, true) => crate::i18n::text!("Копировать налево"),
                    (SyncChange::Copy, false) => crate::i18n::text!("Копировать направо"),
                    (SyncChange::Replace, true) => crate::i18n::text!("Заменить слева"),
                    (SyncChange::Replace, false) => crate::i18n::text!("Заменить справа"),
                    (SyncChange::Delete, true) => crate::i18n::text!("Удалить слева"),
                    (SyncChange::Delete, false) => crate::i18n::text!("Удалить справа"),
                },
                false,
            ),
            Reconcile::Conflict(reason) => (
                match reason {
                    ReconcileConflict::FirstComparison => {
                        crate::i18n::text!("Разные версии при первом сравнении")
                    }
                    ReconcileConflict::BothChanged => {
                        crate::i18n::text!("Конфликт: обе стороны изменены")
                    }
                    ReconcileConflict::EditAndDelete => {
                        crate::i18n::text!("Конфликт: правка и удаление")
                    }
                    ReconcileConflict::Interrupted => {
                        crate::i18n::text!("Различие после прерывания")
                    }
                },
                true,
            ),
        };
        show_action(ui, action, conflict, 245.0, height);
        if conflict {
            for side in 0..2 {
                let label = match (side == 0, plan.missing(&row.path, side)) {
                    (true, false) => crate::i18n::text!("Оставить версию слева"),
                    (false, false) => crate::i18n::text!("Оставить версию справа"),
                    (true, true) => crate::i18n::text!("Принять удаление слева"),
                    (false, true) => crate::i18n::text!("Принять удаление справа"),
                };
                if ui.button(label).clicked() {
                    choice = Some(Review::Keep(row.path.clone(), side));
                }
            }
        }
        show_relative_path(ui, &row.path);
    });
    choice
}

fn show_rows<T>(
    ui: &mut egui::Ui,
    rows: &[T],
    run: u64,
    mut display: impl FnMut(&mut egui::Ui, &T, f32),
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
                ui.horizontal(|ui| display(ui, row, row_height));
            }
        });
}

fn show_action(ui: &mut egui::Ui, action: &'static str, conflict: bool, width: f32, height: f32) {
    let mut action = egui::RichText::new(action);
    if conflict {
        action = action.color(ui.visuals().error_fg_color);
    }
    ui.add_sized([width, height], egui::Label::new(action).truncate());
}

fn show_relative_path(ui: &mut egui::Ui, path: &Path) {
    let path = path.to_string_lossy().escape_debug().to_string();
    ui.add(egui::Label::new(path).extend().selectable(true));
}

impl PawCommander {
    pub(super) fn can_open_folder_sync(&self) -> bool {
        !self.conflict_open() && (self.can_compare_folders() || !self.sync_profiles.is_empty())
    }

    pub(super) fn enqueue_folder_sync(&mut self, plan: Plan, ctx: &egui::Context) {
        let (folders, label, direction) = match &plan {
            Plan::OneWay(plan) => (
                plan.folders(),
                crate::i18n::text!("Синхронизация папок"),
                '→',
            ),
            Plan::TwoWay(plan) => (
                plan.folders(),
                crate::i18n::text!("Двусторонняя синхронизация папок"),
                '↔',
            ),
        };
        let [left, right] = folders;
        self.start_typed_stoppable_job(
            ctx,
            label,
            format!("{} {direction} {}", left.display(), right.display()),
            move |progress| {
                let SyncReport { completed, result } = match plan {
                    Plan::OneWay(plan) => plan.apply(progress),
                    Plan::TwoWay(plan) => plan.apply(progress),
                };
                match result {
                    Ok(()) => Ok(crate::tr_format!(
                        "Synchronization complete. Completed: {completed}.",
                        completed = completed
                    )),
                    Err(TransferError::Cancelled) => Ok(crate::tr_format!(
                        "Synchronization stopped. Completed: {completed}.",
                        completed = completed
                    )),
                    Err(error) => Err(SyncError::applying(completed, error)),
                }
            },
        );
    }

    pub(super) fn open_folder_sync(&mut self, return_focus: Option<egui::Id>, ctx: &egui::Context) {
        if self.comparison.is_none() && self.can_open_folder_sync() {
            let current = self.can_compare_folders().then(|| {
                [
                    self.panes[0].tab().path.clone(),
                    self.panes[1].tab().path.clone(),
                ]
            });
            let (folders, options, selected) = if let Some(folders) = &current {
                (folders.clone(), Options::default(), None)
            } else if let Some(saved) =
                self.sync_profiles
                    .iter()
                    .enumerate()
                    .find_map(|(index, profile)| {
                        Some((profile.folders()?, profile.options.clone(), Some(index)))
                    })
            {
                saved
            } else {
                self.notice = Some((
                    text!("Папки профиля несовместимы с этой системой.").into(),
                    true,
                ));
                return;
            };
            self.comparison = Some(ComparisonDialog::new(
                ComparisonBody::Sync(Box::new(FolderSyncDialog::new(
                    folders,
                    options,
                    ProfilePicker::new(current, selected),
                ))),
                return_focus,
                ctx,
            ));
        }
    }
}
