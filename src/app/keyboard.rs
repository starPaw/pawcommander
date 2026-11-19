use super::actions::{Action, SelectionChange};
use super::bookmarks::bookmark_slot_key;
use super::clipboard::file_clipboard_action;
use super::dialogs::Dialog;
use super::selection::RuleKind;
use super::state::{FolderMode, FolderViewChange, Settings};
use super::PawCommander;
use crate::drag_drop::DragFiles;
use crate::folder_jump::FolderJump;
use crate::fs_ops::TransferKind;
use crate::shortcuts;
use crate::shortcuts::Command;
use crate::tab_drag::DragTab;
use crate::vfs::Location;
use eframe::egui;

impl PawCommander {
    pub(super) fn run_shortcut(&mut self, command: Command, ctx: &egui::Context) {
        let action = match command {
            Command::EditPath => return self.tab_mut().begin_address(),
            Command::Filter => {
                self.tab_mut().focus_filter = true;
                return;
            }
            Command::Search => return self.open_search(),
            Command::Palette => {
                let previous_focus = ctx.memory(|memory| memory.focused());
                return self.open_command_palette(ctx, previous_focus);
            }
            Command::FolderJump => {
                let previous_focus = ctx.memory(|memory| memory.focused());
                self.folder_jump = Some(FolderJump::new(previous_focus));
                Self::restore_focus(ctx, None);
                return;
            }
            Command::FocusToolbar => {
                self.focus_toolbar = true;
                return;
            }
            Command::NewTab => Action::NewTab(self.active),
            Command::CloseTab => Action::CloseTab(self.active, self.panes[self.active].current),
            Command::TogglePinTab => {
                Action::TogglePinTab(self.active, self.panes[self.active].current)
            }
            Command::DuplicateTab => {
                Action::DuplicateTab(self.active, self.panes[self.active].current)
            }
            Command::ReopenTab => Action::ReopenTab(self.active),
            Command::Back => Action::Back,
            Command::Forward => Action::Forward,
            Command::Parent => Action::Up,
            Command::Open => {
                let Some(path) = self.selected().into_iter().next() else {
                    return;
                };
                Action::Open(self.active, path)
            }
            Command::SelectAll | Command::InvertSelection | Command::RestoreSelection => {
                let change = match command {
                    Command::SelectAll => SelectionChange::All,
                    Command::InvertSelection => SelectionChange::Invert,
                    _ => SelectionChange::Restore,
                };
                self.apply_selection(change, ctx);
                return;
            }
            Command::SelectByMask | Command::SelectByExtension => {
                let kind = if command == Command::SelectByMask {
                    RuleKind::Mask
                } else {
                    RuleKind::Extension
                };
                self.remember_dialog_file_focus(ctx);
                self.dialog = Some(Dialog::SelectionRule {
                    command,
                    kind,
                    tab: self.tab().id,
                    location: self.tab().path.clone(),
                    input: String::new(),
                    scan: None,
                });
                self.dialog_focus = true;
                return;
            }
            Command::Hidden => {
                let hidden = !self.folder_view(self.active).hidden;
                self.action(
                    self.folder_view_action(self.active, FolderViewChange::Hidden(hidden)),
                    ctx,
                );
                return;
            }
            Command::Split => return self.toggle_split(),
            Command::OtherPane => {
                if self.settings.split {
                    let previous_focus = self.tab().focused_cursor(ctx);
                    if let Some(id) = previous_focus {
                        ctx.memory_mut(|memory| memory.surrender_focus(id));
                    }
                    self.active = 1 - self.active;
                    if previous_focus.is_some() {
                        let view = self.folder_view(self.active);
                        let tab = self.tab_mut();
                        let visible = tab.visible(view);
                        let path = visible
                            .iter()
                            .map(|&index| &tab.entries[index].path)
                            .find(|path| Some(*path) == tab.cursor.as_ref())
                            .or_else(|| visible.first().map(|&index| &tab.entries[index].path));
                        if let Some(path) = path.cloned() {
                            tab.scroll_to = Some(path.clone());
                            tab.cursor = Some(path);
                            tab.focus_cursor(ctx);
                        }
                    }
                }
                return;
            }
            Command::Preview => {
                self.settings.preview = !self.settings.preview;
                return;
            }
            Command::QuickLook => return self.open_quick_look(ctx),
            Command::EditInZed => {
                let paths = self.selected();
                let [path] = paths.as_slice() else {
                    return;
                };
                if !self
                    .tab()
                    .entries
                    .iter()
                    .any(|entry| entry.path == *path && !entry.is_dir)
                {
                    return;
                }
                if self.tools.zed.is_none() {
                    self.notice = Some((
                        crate::i18n::text("Zed не найден. Установите редактор и выберите Настройки → Инструменты → Проверить снова.").into(),
                        true,
                    ));
                    return;
                }
                Action::OpenInZed(self.active, path.clone())
            }
            Command::Refresh => Action::Refresh,
            Command::Properties => Action::Properties,
            Command::ChangePermissions => Action::ChangePermissions,
            Command::Rename => Action::Rename,
            Command::CreateArchive => Action::CreateArchive,
            Command::ExtractArchive => Action::ExtractArchive,
            Command::VerifyArchive => Action::VerifyArchive,
            Command::CreateChecksumManifest => Action::CreateChecksumManifest,
            Command::VerifyChecksumManifest => Action::VerifyChecksumManifest,
            Command::CompareFolders => Action::CompareFolders,
            Command::CompareFiles => Action::CompareFiles,
            Command::AddToArchive => Action::AddToArchive,
            Command::RemoveFromArchive => Action::RemoveFromArchive,
            Command::NewFolder => Action::NewFolder,
            Command::Trash => Action::Trash,
            Command::CopyToPane => Action::Transfer(TransferKind::Copy),
            Command::MoveToPane => Action::Transfer(TransferKind::Move),
        };
        self.action(action, ctx);
    }

    pub(super) fn keyboard_blocked(&self, ctx: &egui::Context) -> bool {
        let visible_panes = 1 + usize::from(self.settings.split);
        egui::DragAndDrop::has_payload_of_type::<DragTab>(ctx)
            || egui::DragAndDrop::has_payload_of_type::<DragFiles>(ctx)
            || egui::Popup::is_any_open(ctx)
            // Legacy egui context menus are tracked separately from other popups.
            || self.panes[..visible_panes].iter().any(|pane| {
                pane.tab_context_menu
                    .and_then(|id| ctx.read_response(id))
                    .is_some_and(|response| response.context_menu_opened())
            })
            || self.dialog.is_some()
            || self.properties.is_some()
            || self.comparison.is_some()
            || self.conflict_open()
            || self.sftp_form.is_some()
            || self.show_help
            || self.show_history
            || self.recovery.is_open()
            || self.shortcut_editor.is_some()
            || self.bookmark_editor.is_some()
            || self.bookmark_menu_open.is_some_and(|id| {
                self.bookmarks.iter().any(|bookmark| bookmark.id == id)
            })
            || self.command_palette.is_some()
            || self.quick_look.is_some()
            || self.folder_jump.is_some()
            || self.show_accessibility
            || self.show_list_columns
            || self.native_drag.active()
            || self.search.visible
    }

    pub(super) fn bound_pane_command(&self, chord: egui::KeyboardShortcut) -> Option<Command> {
        self.settings.shortcuts.command(chord).filter(|&command| {
            chord.logical_key != egui::Key::Tab
                || command != Command::OtherPane
                || self.settings.split
        })
    }

    pub(super) fn keyboard(&mut self, ctx: &egui::Context) {
        let pane_key_presses = std::mem::take(&mut self.pane_key_presses);
        if self.keyboard_blocked(ctx) {
            return;
        }
        // egui drops a text field's focus on Escape before this update runs.
        let file_focus = if let Some((pane, path)) = self.file_row_focus(ctx) {
            let path = path.clone();
            self.active = pane;
            self.tab_mut().cursor = Some(path);
            true
        } else {
            false
        };
        let focused = !file_focus
            && (ctx.egui_wants_keyboard_input()
                || (self.keyboard_had_focus
                    && ctx.input(|input| input.key_pressed(egui::Key::Escape))));
        let mut actions = Vec::new();
        let mut pane = self.active;
        ctx.input_mut(|input| {
            let clipboard_enabled = !focused && input.focused;
            if !focused {
                for (index, event) in pane_key_presses {
                    input.events.insert(index.min(input.events.len()), event);
                }
            }
            input.events.retain(|event| {
                let action = if let Some(action) = clipboard_enabled
                    .then(|| file_clipboard_action(event))
                    .flatten()
                {
                    Some(action)
                } else if let egui::Event::Key {
                    key,
                    physical_key,
                    modifiers,
                    pressed: true,
                    repeat,
                    ..
                } = *event
                {
                    let chord = shortcuts::chord(key, modifiers);
                    let physical_key = physical_key.unwrap_or(key);
                    let modifiers = chord.modifiers;
                    if !focused
                        && matches!(modifiers, egui::Modifiers::NONE | egui::Modifiers::SHIFT)
                        && matches!(key, egui::Key::ArrowUp | egui::Key::ArrowDown)
                    {
                        Some(Action::MoveSelection {
                            delta: if key == egui::Key::ArrowUp { -1 } else { 1 },
                            extend: modifiers.shift,
                        })
                    } else if !focused
                        && key == egui::Key::Space
                        && modifiers == egui::Modifiers::SHIFT
                    {
                        if repeat {
                            return false;
                        }
                        Some(Action::ToggleCursorSelection)
                    } else if !focused
                        && self.folder_view(pane).mode == FolderMode::Columns
                        && modifiers == egui::Modifiers::NONE
                        && matches!(key, egui::Key::ArrowLeft | egui::Key::ArrowRight)
                        && self.bound_pane_command(chord).is_none()
                    {
                        if repeat {
                            return false;
                        }
                        Some(Action::ColumnStep {
                            right: key == egui::Key::ArrowRight,
                        })
                    } else if !focused
                        && !repeat
                        && modifiers == egui::Modifiers::NONE
                        && key == egui::Key::Escape
                    {
                        Some(Action::ClearSelection)
                    } else {
                        let bookmark_slot = if !focused
                            && !repeat
                            && modifiers == egui::Modifiers::ALT
                            && self.settings.shortcuts.command(chord).is_none()
                            && self
                                .settings
                                .shortcuts
                                .command(shortcuts::chord(physical_key, modifiers))
                                .is_none()
                        {
                            (1..=9).find(|slot| {
                                bookmark_slot_key(*slot) == Some(physical_key)
                                    && self
                                        .bookmarks
                                        .iter()
                                        .any(|bookmark| bookmark.shortcut == Some(*slot))
                            })
                        } else {
                            None
                        };
                        if let Some(slot) = bookmark_slot {
                            Some(Action::BookmarkSlot(slot))
                        } else {
                            let Some(command) = self
                                .bound_pane_command(chord)
                                .filter(|command| !focused || command.works_in_text_field(chord))
                            else {
                                return true;
                            };
                            if repeat {
                                return false;
                            }
                            Some(Action::Shortcut(command))
                        }
                    }
                } else {
                    None
                };
                if let Some(action) = action {
                    if self.settings.split && matches!(action, Action::Shortcut(Command::OtherPane))
                    {
                        pane = 1 - pane;
                    }
                    actions.push(action);
                    false
                } else {
                    true
                }
            });
        });
        for action in actions {
            if self.keyboard_blocked(ctx) {
                break;
            }
            let moves_widget_focus = matches!(
                action,
                Action::Shortcut(Command::EditPath | Command::Filter | Command::FocusToolbar)
            );
            self.run_pane_action(action, ctx);
            // The requested control takes focus later in this frame, when rendered.
            if moves_widget_focus {
                break;
            }
        }
    }

    pub(super) fn file_row_focus(&self, ctx: &egui::Context) -> Option<(usize, &Location)> {
        let id = ctx.memory(|memory| memory.focused())?;
        self.panes
            .iter()
            .enumerate()
            .take(if self.settings.split { 2 } else { 1 })
            .find_map(|(pane, state)| state.tab().rendered_rows.get(&id).map(|path| (pane, path)))
    }

    pub(super) fn scale_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::gui_zoom::kb_shortcuts::{ZOOM_IN, ZOOM_IN_SECONDARY, ZOOM_OUT, ZOOM_RESET};
        let scales = Settings::UI_SCALES;
        let scale = self.settings.ui_scale;
        let next = ctx.input_mut(|input| {
            if input.consume_shortcut(&ZOOM_RESET) {
                Some(100)
            } else if input.consume_shortcut(&ZOOM_IN) || input.consume_shortcut(&ZOOM_IN_SECONDARY)
            {
                scales.into_iter().find(|&candidate| candidate > scale)
            } else if input.consume_shortcut(&ZOOM_OUT) {
                scales.into_iter().rfind(|&candidate| candidate < scale)
            } else {
                None
            }
        });
        if let Some(next) = next {
            self.settings.set_ui_scale(ctx, next);
        }
    }
}
