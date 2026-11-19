use super::actions::Action;
use super::PawCommander;
use crate::command_palette::{CommandPalette, PaletteOutcome};
use crate::folder_jump::JumpOutcome;
use crate::shortcuts::Command;
use eframe::egui;

impl PawCommander {
    pub(super) fn open_command_palette(
        &mut self,
        ctx: &egui::Context,
        previous_focus: Option<egui::Id>,
    ) {
        let focused_file = self
            .file_row_focus(ctx)
            .filter(|(pane, file)| Some(self.panes[*pane].tab().entry_id(file)) == previous_focus)
            .map(|(_, file)| file.clone());
        self.command_palette = Some(CommandPalette::new(previous_focus, focused_file));
        Self::restore_focus(ctx, None);
    }

    pub(super) fn restore_focus(ctx: &egui::Context, previous_focus: Option<egui::Id>) {
        ctx.memory_mut(|memory| {
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
            if let Some(id) = previous_focus {
                memory.request_focus(id);
            }
        });
    }

    pub(super) fn guard_picker_close_key(
        &mut self,
        ctx: &egui::Context,
        keys: &[egui::Key],
        key: egui::Key,
    ) {
        if keys.contains(&key) && ctx.input(|input| input.key_down(key)) {
            self.palette_release_key = Some(key);
        }
    }

    pub(super) fn show_command_palette(&mut self, ctx: &egui::Context, keys: &[egui::Key]) {
        let Some(mut palette) = self.command_palette.take() else {
            return;
        };
        let tab = self.tab();
        let focused_file = palette.focused_file.as_ref();
        let selected = self.selected();
        let has_selection =
            !selected.is_empty() && focused_file.is_none_or(|file| selected.contains(file));
        let open_target = if let Some(file) = focused_file {
            tab.visible(self.folder_view(self.active))
                .iter()
                .map(|&index| &tab.entries[index].path)
                .find(|&path| path == file)
        } else {
            selected.first()
        };
        let restore_file_cursor = focused_file.is_some() && open_target.is_some();
        let can_quick_look = self.quick_look_entry(focused_file).is_some();
        let idle = !self.conflict_open();
        let writable = !tab.path.is_archive();
        let file_count = tab.selected_file_count(&tab.visible(self.folder_view(self.active)));
        let outcome = palette.show(
            ctx,
            &self.settings.shortcuts,
            keys,
            |command| match command {
                Command::Back => tab.history_index > 0,
                Command::Forward => tab.history_index + 1 < tab.history.len(),
                Command::Parent => tab.path.parent().is_some(),
                Command::CloseTab => {
                    let pane = &self.panes[self.active];
                    pane.can_close_tab(pane.current)
                }
                Command::ReopenTab => !self.panes[self.active].closed_tabs.is_empty(),
                Command::Open => open_target.is_some_and(|path| {
                    path.local().is_some()
                        || tab
                            .entries
                            .iter()
                            .any(|entry| entry.path == *path && entry.is_dir)
                        || (path.remote() && self.tools.zed.is_some() && self.editor_job.is_none())
                }),
                Command::OtherPane => self.settings.split,
                Command::QuickLook => can_quick_look,
                Command::RestoreSelection => tab.previous_selection.is_some(),
                Command::NewFolder => idle && writable,
                Command::Rename => idle && has_selection && selected.len() == 1,
                Command::Trash | Command::CreateArchive => idle && writable && has_selection,
                Command::ExtractArchive => idle && !writable && has_selection,
                Command::VerifyArchive => {
                    idle && (!writable || has_selection)
                        && self.archive_to_verify(&selected).is_some()
                }
                Command::CreateChecksumManifest => idle && has_selection && file_count.is_some(),
                Command::VerifyChecksumManifest => idle && has_selection && file_count == Some(1),
                Command::CompareFolders => self.can_compare_folders(),
                Command::AddToArchive => {
                    idle && !writable && !self.archive_add_sources().is_empty()
                }
                Command::RemoveFromArchive => idle && !writable && has_selection,
                Command::Properties => {
                    has_selection && selected.iter().all(|path| path.local().is_some())
                }
                Command::CopyToPane | Command::MoveToPane => {
                    idle && writable
                        && self.settings.split
                        && has_selection
                        && !self.panes[1 - self.active].tab().path.is_archive()
                }
                Command::EditInZed => {
                    writable
                        && has_selection
                        && selected.len() == 1
                        && self.tools.zed.is_some()
                        && self.editor_job.is_none()
                        && tab
                            .entries
                            .iter()
                            .any(|entry| !entry.is_dir && entry.path == selected[0])
                }
                _ => true,
            },
        );
        if matches!(outcome, PaletteOutcome::Open) {
            self.command_palette = Some(palette);
            return;
        }
        let close_key = if matches!(outcome, PaletteOutcome::Dismissed) {
            egui::Key::Escape
        } else {
            egui::Key::Enter
        };
        self.guard_picker_close_key(ctx, keys, close_key);
        Self::restore_focus(ctx, palette.previous_focus);
        if let PaletteOutcome::Run(command) = outcome {
            if restore_file_cursor {
                self.tab_mut().cursor = palette.focused_file;
            }
            self.run_pane_action(Action::Shortcut(command), ctx);
        }
        // egui keeps the old modal layer active through the next render and
        // can clear the workspace's focus when drawing the underlying widget.
        self.palette_focus_to_restore = ctx.memory(|memory| memory.focused());
        ctx.request_repaint();
    }

    pub(super) fn show_folder_jump(&mut self, ctx: &egui::Context, keys: &[egui::Key]) {
        let Some(mut jump) = self.folder_jump.take() else {
            return;
        };
        match jump.show(ctx, &self.settings.folder_history, keys) {
            JumpOutcome::Open => {
                self.folder_jump = Some(jump);
                return;
            }
            JumpOutcome::Dismissed => {
                self.guard_picker_close_key(ctx, keys, egui::Key::Escape);
                Self::restore_focus(ctx, jump.previous_focus);
                self.palette_focus_to_restore = jump.previous_focus;
            }
            JumpOutcome::Navigate(location) => {
                self.guard_picker_close_key(ctx, keys, egui::Key::Enter);
                if self.tab().path == location {
                    Self::restore_focus(ctx, jump.previous_focus);
                    self.palette_focus_to_restore = jump.previous_focus;
                } else {
                    Self::restore_focus(ctx, None);
                    self.action(Action::Navigate(self.active, location), ctx);
                }
            }
        }
        ctx.request_repaint();
    }
}
