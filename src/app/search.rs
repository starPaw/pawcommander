use super::actions::{Action, ColumnNavigation};
use super::state::{is_dot_file, FolderMode, FolderViewChange};
use super::PawCommander;
use crate::fs_ops::TransferKind;
use crate::search::panel::{ReadyAction, TabTarget};
use crate::vfs::Location;
use eframe::egui;

impl PawCommander {
    pub(super) fn open_search(&mut self) {
        self.search.open(Some(&self.panes[self.active].tab().path));
    }

    pub(super) fn reveal_search_result(&mut self, path: Location, ctx: &egui::Context) {
        let Some(parent) = path.parent() else {
            return;
        };
        self.tab_mut().navigate(parent, ctx);
        // A hidden result needs to be visible in its destination folder.
        if is_dot_file(path.path()) && !self.folder_view(self.active).hidden {
            self.change_folder_view(self.active, FolderViewChange::Hidden(true));
        }
        let tab = self.tab_mut();
        tab.filter.clear();
        tab.selected.clear();
        tab.reveal_after_load = Some(path);
    }

    pub(super) fn show_search_results(&mut self, ui: &mut egui::Ui) {
        let ctx = &ui.ctx().clone();
        if !self.search.panel.is_open() {
            return;
        }
        let target = TabTarget {
            pane: self.active,
            tab: self.tab().id,
            location: self.tab().path.clone(),
        };
        let destination = self
            .settings
            .split
            .then(|| self.panes[1 - self.active].tab().path.clone())
            .filter(|path| !path.is_archive());
        let enabled = !self.keyboard_blocked(ctx);
        let can_copy = !self.conflict_open();
        self.search
            .panel
            .show(ui, target, destination, enabled, can_copy);
    }

    pub(super) fn search_target_is_current(
        &mut self,
        target: &TabTarget,
        ctx: &egui::Context,
    ) -> bool {
        if self.active == target.pane
            && self.tab().id == target.tab
            && self.tab().path == target.location
        {
            return true;
        }
        self.search.panel.reject(
            crate::i18n::text("Активная папка или вкладка изменилась. Повторите действие."),
            ctx,
        );
        false
    }

    pub(super) fn dispatch_search_result(&mut self, action: ReadyAction, ctx: &egui::Context) {
        if self.keyboard_blocked(ctx) {
            self.search.panel.reject(
                crate::i18n::text("Закройте открытое окно или меню и повторите действие."),
                ctx,
            );
            return;
        }
        match action {
            ReadyAction::Open {
                path,
                directory: false,
                target,
            } => {
                if path.remote() || crate::archive::Format::from_path(path.path()).is_some() {
                    if self.search_target_is_current(&target, ctx) {
                        Self::restore_focus(ctx, None);
                        self.action(Action::Open(target.pane, path), ctx);
                    }
                } else if let Location::Local(path) = path {
                    self.search.panel.open_file(path, ctx);
                }
            }
            ReadyAction::Open {
                path,
                directory: true,
                target,
            } => {
                if self.search_target_is_current(&target, ctx) {
                    Self::restore_focus(ctx, None);
                    if self.folder_view(target.pane).mode == FolderMode::Columns {
                        self.navigate_columns(target.pane, ColumnNavigation::Browse(path), ctx);
                    } else {
                        self.tab_mut().navigate(path, ctx);
                    }
                }
            }
            ReadyAction::Reveal { path, target } => {
                if self.search_target_is_current(&target, ctx) {
                    Self::restore_focus(ctx, None);
                    self.reveal_search_result(path, ctx);
                }
            }
            ReadyAction::Copy { paths, destination } => {
                self.start_transfer(
                    ctx,
                    crate::i18n::text("Копирование в соседнюю панель"),
                    paths,
                    destination,
                    TransferKind::Copy,
                    false,
                );
            }
        }
    }
}
