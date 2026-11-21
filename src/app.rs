//! Application state and frame lifecycle. Feature handlers and widgets live in submodules.

mod actions;
mod address_bar;
mod batch_rename;
mod bookmarks;
mod clipboard;
#[cfg(target_os = "macos")]
mod collections;
mod column_view;
mod comparison;
#[cfg(target_os = "macos")]
mod compatible_names;
#[cfg(target_os = "macos")]
mod custom_actions;
#[cfg(target_os = "macos")]
mod diagnostics;
mod dialogs;
mod directory;
mod directory_compare;
#[cfg(target_os = "macos")]
mod disk_usage;
#[cfg(any(target_os = "windows", test))]
mod drives;
#[cfg(target_os = "macos")]
mod duplicates;
mod entry_widget;
#[cfg(target_os = "macos")]
mod export_file;
#[cfg(target_os = "macos")]
mod extensions;
#[cfg(target_os = "macos")]
mod external_editors;
mod file_compare;
#[cfg(target_os = "macos")]
mod file_dialog;
#[cfg(target_os = "macos")]
mod file_templates;
#[cfg(target_os = "macos")]
mod folder_sync;
mod footer;
#[cfg(target_os = "macos")]
mod git_status;
mod jobs;
mod keyboard;
#[cfg(target_os = "macos")]
mod link_dialog;
mod pane_view;
mod permissions;
mod persistence;
mod pickers;
mod quick_look;
mod search;
mod selection;
#[cfg(target_os = "macos")]
mod settings_transfer;
mod sidebar;
#[cfg(target_os = "macos")]
mod smb_connections;
mod state;
#[cfg(target_os = "macos")]
mod sync_profiles;
mod tab_strip;
mod tabs;
mod toolbar;
mod transfer_limits;
#[cfg(target_os = "macos")]
mod volumes;

use crate::command_palette::CommandPalette;
use crate::directory_tree::DirectoryTree;
use crate::drag_drop::DropTargets;
use crate::file_clipboard::SystemClipboard;
use crate::folder_jump::FolderJump;
use crate::preview::Preview;
use crate::result_history::History;
use crate::search::panel::TabTarget;
use crate::shortcuts::{Command, ShortcutEditor};
use crate::tab_drag::TabDropTargets;
use crate::theme::Palette;
use crate::thumbnails::Thumbnails;
use crate::vfs::{FileEntry, Location};
use crate::{shortcuts, vfs as fs_ops};
use actions::{Action, ColumnNavigation};
use bookmarks::{validate_bookmark_shortcuts, Bookmark, BookmarkEditor, SavedBookmark};
use clipboard::Clipboard;
use comparison::ComparisonDialog;
use dialogs::{ConnectionJob, Dialog, SftpForm};
use directory::DirectoryEntries;
use eframe::egui;
use entry_widget::{entry_widget, EntryOptions, COMPACT_FILE_ROW_HEIGHT};
use jobs::{CutRequest, Job, PendingJob, RetryJob, TransferInteraction, UndoJob};
use pane_view::{list_notice, render_pane, workspace_card, PaneOptions};
pub(crate) use persistence::Startup;
#[cfg(target_os = "macos")]
pub(crate) use persistence::{find_saved_workspace, WorkspaceError};
use quick_look::QuickLook;
use state::{
    FolderMode, FolderView, FolderViewChange, FolderViews, NamedWorkspace, SavedFolderView,
    Settings, WorkspaceSession,
};
pub(crate) use state::{SavedLocation, SavedPath};
use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tabs::{move_tab, Pane, Tab};

pub struct PawCommander {
    panes: Vec<Pane>,
    active: usize,
    settings: Settings,
    folder_views: FolderViews,
    directory_tree: DirectoryTree,
    #[cfg(any(target_os = "windows", test))]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    windows_drives: drives::WindowsDrives,
    bookmarks: Vec<Bookmark>,
    bookmark_menu_open: Option<u64>,
    bookmark_editor: Option<BookmarkEditor>,
    workspaces: Vec<NamedWorkspace>,
    #[cfg(target_os = "macos")]
    sync_profiles: Vec<sync_profiles::SyncProfile>,
    #[cfg(target_os = "macos")]
    collections: crate::vfs::collections::Catalog,
    #[cfg(target_os = "macos")]
    file_templates: crate::vfs::file_templates::Library,
    #[cfg(target_os = "macos")]
    extensions: extensions::Extensions,
    preview: Preview,
    thumbnails: Thumbnails,
    quick_look: Option<QuickLook>,
    quick_look_preview: Option<Preview>,
    properties: Option<crate::properties::Properties>,
    comparison: Option<ComparisonDialog>,
    clipboard: Option<Clipboard>,
    system_clipboard: SystemClipboard,
    dialog: Option<Dialog>,
    dialog_focus: bool,
    dialog_file_focus: Option<(u64, Location)>,
    job: Option<Job>,
    pending_jobs: VecDeque<PendingJob>,
    retry_jobs: VecDeque<RetryJob>,
    undo_jobs: VecDeque<UndoJob>,
    queue_start_failed: bool,
    #[cfg(test)]
    fail_job_spawn: bool,
    active_cut: Option<CutRequest>,
    transfer: Option<TransferInteraction>,
    notice: Option<(String, bool)>,
    history: History,
    recovery: crate::recovery::Recovery,
    show_history: bool,
    show_help: bool,
    shortcut_editor: Option<ShortcutEditor>,
    command_palette: Option<CommandPalette>,
    folder_jump: Option<FolderJump>,
    palette_release_key: Option<egui::Key>,
    palette_focus_to_restore: Option<egui::Id>,
    keyboard_had_focus: bool,
    tab_held: bool,
    pane_key_presses: Vec<(usize, egui::Event)>,
    focus_toolbar: bool,
    show_accessibility: bool,
    accessibility_focus: bool,
    show_list_columns: bool,
    list_columns_focus: bool,
    sftp_form: Option<SftpForm>,
    connection: Option<ConnectionJob>,
    native_drag: crate::native_drag::NativeDrag,
    application_job: Option<Receiver<Result<String, String>>>,
    #[cfg(target_os = "macos")]
    smb: smb_connections::Connections,
    #[cfg(target_os = "macos")]
    volumes: volumes::VolumeBrowser,
    #[cfg(target_os = "macos")]
    embedded_terminal: Option<crate::terminal::embedded::Panel>,
    #[cfg(target_os = "macos")]
    remote_editor: external_editors::Session,
    tools: crate::tools::Tools,
    search: crate::search::Search,
}

impl PawCommander {
    fn tab(&self) -> &Tab {
        self.panes[self.active].tab()
    }

    fn tab_mut(&mut self) -> &mut Tab {
        self.panes[self.active].tab_mut()
    }

    fn folder_view(&self, index: usize) -> FolderView {
        self.panes[index]
            .tab()
            .folder_view(&self.settings, index, &self.folder_views)
    }

    fn folder_view_action(&self, index: usize, change: FolderViewChange) -> Action {
        self.panes[index].tab().folder_view_action(index, change)
    }

    fn selected(&self) -> Vec<Location> {
        self.tab().selected_visible(self.folder_view(self.active))
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        self.preview.refresh();
        self.directory_tree.refresh();
        for pane in &mut self.panes {
            pane.columns.refresh();
            #[cfg(target_os = "macos")]
            pane.git_status.refresh();
            for tab in &mut pane.tabs {
                tab.reload(ctx);
            }
        }
    }
}

impl eframe::App for PawCommander {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe also runs logic while hidden. Drain workers and advance the
        // queue without requiring a visible frame or painting any widgets.
        self.history.poll();
        self.recovery.poll(ctx);
        if let Some(comparison) = &mut self.comparison {
            comparison.poll();
        }
        #[cfg(target_os = "windows")]
        self.windows_drives.poll(ctx);
        self.poll(ctx);
        #[cfg(target_os = "macos")]
        self.accept_file_promises(ctx);
        #[cfg(target_os = "macos")]
        self.poll_smb(ctx);
        #[cfg(target_os = "macos")]
        self.poll_volumes(ctx);
        #[cfg(target_os = "macos")]
        self.poll_external_editors(ctx);
        #[cfg(target_os = "macos")]
        if self
            .embedded_terminal
            .as_mut()
            .is_some_and(crate::terminal::embedded::Panel::poll)
        {
            self.embedded_terminal = None;
            ctx.request_repaint();
        }
        // Hidden windows receive close requests through logic-only ticks too.
        if ctx.input(|i| i.viewport().close_requested()) {
            #[cfg(target_os = "macos")]
            if self.volumes.ejecting() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                return;
            }
            if self.job.is_some() || !self.pending_jobs.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.notice = Some((
                    crate::i18n::text!(
                        "Дождитесь завершения файловой операции, затем закройте окно."
                    )
                    .into(),
                    false,
                ));
            }
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        #[cfg(target_os = "macos")]
        self.native_drag.prepare_input(ctx, input);
        if let Some(accepted) = self.native_drag.finish_input(input)
            && accepted
        {
            self.notice = Some((
                crate::i18n::text!("Файлы переданы принимающему приложению").into(),
                false,
            ));
        }

        self.pane_key_presses.clear();
        if !input.focused {
            self.tab_held = false;
            self.palette_release_key = None;
            return;
        }
        #[cfg(target_os = "macos")]
        clipboard::macos_clipboard_events(&mut input.events);
        // A held key that closed the palette must not activate a control behind it.
        input.events.retain(|event| match *event {
            egui::Event::Key { key, pressed, .. } if self.palette_release_key == Some(key) => {
                if !pressed {
                    self.palette_release_key = None;
                }
                !pressed
            }
            _ => true,
        });
        #[cfg(target_os = "macos")]
        if input.viewport_id == egui::ViewportId::ROOT
            && !self.keyboard_blocked(ctx)
            && self.embedded_terminal.as_mut().is_some_and(|terminal| {
                terminal.capture(ctx, &mut input.events, |key, modifiers| {
                    let chord = shortcuts::chord(key, modifiers);
                    modifiers.mac_cmd
                        && Command::Palette.works_in_text_field(chord)
                        && self.settings.shortcuts.command(chord) == Some(Command::Palette)
                })
            })
        {
            self.tab_held = false;
            return;
        }
        let pane_keys = (!ctx.egui_wants_keyboard_input() || self.file_row_focus(ctx).is_some())
            && !self.keyboard_blocked(ctx);
        let mut pane = self
            .file_row_focus(ctx)
            .map_or(self.active, |(pane, _)| pane);
        let mut index = 0;
        input.events.retain_mut(|event| {
            let event_index = index;
            index += 1;
            match event {
                egui::Event::Key {
                    key: egui::Key::Tab,
                    modifiers,
                    pressed,
                    repeat,
                    ..
                } => {
                    let was_held = self.tab_held;
                    self.tab_held = *pressed;
                    if *pressed
                        && pane_keys
                        && self
                            .bound_pane_command(shortcuts::chord(egui::Key::Tab, *modifiers))
                            .is_some()
                    {
                        // egui uses raw Tab presses for focus traversal before update.
                        // Dispatch this press in order without letting it move widget focus.
                        *repeat |= was_held;
                        if !*repeat
                            && self.settings.split
                            && self.bound_pane_command(shortcuts::chord(egui::Key::Tab, *modifiers))
                                == Some(Command::OtherPane)
                        {
                            pane = 1 - pane;
                        }
                        self.pane_key_presses.push((event_index, event.clone()));
                        return false;
                    }
                }
                egui::Event::Key {
                    key:
                        key @ (egui::Key::ArrowUp
                        | egui::Key::ArrowDown
                        | egui::Key::ArrowLeft
                        | egui::Key::ArrowRight),
                    modifiers,
                    pressed: true,
                    repeat,
                    ..
                } if pane_keys => {
                    let chord = shortcuts::chord(*key, *modifiers);
                    let file_navigation = match key {
                        egui::Key::ArrowUp | egui::Key::ArrowDown => matches!(
                            chord.modifiers,
                            egui::Modifiers::NONE | egui::Modifiers::SHIFT
                        ),
                        _ => {
                            self.folder_view(pane).mode == FolderMode::Columns
                                && chord.modifiers == egui::Modifiers::NONE
                        }
                    };
                    let command = self.bound_pane_command(chord);
                    if file_navigation || command.is_some() {
                        let vertical_navigation = file_navigation
                            && matches!(key, egui::Key::ArrowUp | egui::Key::ArrowDown);
                        if !vertical_navigation
                            && !*repeat
                            && self.settings.split
                            && command == Some(Command::OtherPane)
                        {
                            pane = 1 - pane;
                        }
                        // Arrow navigation moves file selection rather than geometric widget focus.
                        self.pane_key_presses.push((event_index, event.clone()));
                        return false;
                    }
                }
                egui::Event::Key {
                    key,
                    modifiers,
                    pressed: true,
                    repeat: false,
                    ..
                } if pane_keys
                    && self.settings.split
                    && self.bound_pane_command(shortcuts::chord(*key, *modifiers))
                        == Some(Command::OtherPane) =>
                {
                    pane = 1 - pane;
                }
                egui::Event::WindowFocused(false) => self.tab_held = false,
                _ => {}
            }
            true
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        let colors = Palette::from_ctx(ctx);
        self.transfer_stop_shortcut(ctx);
        // Startup findings may arrive while another dialog or picker is open.
        // Suspend those handlers so Enter cannot submit an unrelated operation.
        if self.recovery.is_open() && !self.conflict_open() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label(crate::tr_format!("Close the recovery report to continue."));
            });
            self.recovery.show(ctx);
            #[cfg(target_os = "macos")]
            if let Some(offer) = self.recovery.take_resume_request() {
                self.resume_copy(ctx, offer);
            }
            return;
        }
        if !self.conflict_open()
            && let Some(editor) = &mut self.shortcut_editor
        {
            editor.capture(ctx);
        }
        if self.shortcut_editor.is_none() {
            self.scale_shortcuts(ctx);
        }
        let picker_keys = if (self.command_palette.is_some() || self.folder_jump.is_some())
            && !self.conflict_open()
        {
            CommandPalette::take_keys(ctx)
        } else {
            Vec::new()
        };
        let opening_palette_input = self.keyboard(ctx);
        let quick_look_focus = self.show_quick_look(ctx);
        let mut actions = vec![];
        let mut drop_targets = DropTargets::default();
        let mut tab_targets = TabDropTargets::default();
        self.toolbar(ui, &mut actions);
        self.footer(ui);
        #[cfg(target_os = "macos")]
        {
            let interactive = !self.keyboard_blocked(ctx);
            if let Some(terminal) = &mut self.embedded_terminal {
                terminal.show(ui, interactive);
            }
        }
        self.history.show(ctx, &mut self.show_history);
        if let Some(id) = self.history.retry_requested.take() {
            self.retry_transfer(ctx, &id);
        }
        if let Some(id) = self.history.undo_requested.take() {
            self.undo_operation(ctx, &id);
        }
        self.show_search_results(ui);
        self.sidebar(ui, &mut actions);
        let drop_enabled = self.drop_enabled() && !self.native_drag.active();
        let zed_available = self.tools.zed.is_some();
        if self.settings.preview {
            let selected = self.selected();
            let fingerprint = selected
                .first()
                .and_then(|path| self.tab().entries.iter().find(|entry| &entry.path == path))
                .map(crate::preview::Fingerprint::from);
            let narrow = ctx.content_rect().width() < 900.0;
            egui::Panel::right("preview")
                .default_size(if narrow { 155.0 } else { 260.0 })
                .size_range(if narrow { 145.0..=165.0 } else { 200.0..=380.0 })
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(colors.surface))
                .show(ui, |panel_ui| {
                    let mut card = workspace_card(colors)
                        .inner_margin(12)
                        .outer_margin(egui::Margin {
                            left: 0,
                            right: 8,
                            top: 8,
                            bottom: 8,
                        })
                        .begin(panel_ui);
                    let ui = &mut card.content_ui;
                    ui.set_min_size(ui.available_size());
                    if selected.len() > 1 {
                        ui.label(crate::tr_format!("Selected items: {}", selected.len()));
                    }
                    self.preview.show(ui, selected.first(), fingerprint);
                    card.end(panel_ui);
                });
        }
        self.thumbnails.begin_frame();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(colors.surface).inner_margin(8))
            .show(ui, |ui| {
                let workspace = ui.available_rect_before_wrap();
                let rects = if self.settings.split {
                    self.settings.pane_layout.rects(ui, workspace)
                } else {
                    [workspace, egui::Rect::NOTHING]
                };
                let pane_count = if self.settings.split { 2 } else { 1 };
                for (index, rect) in rects.into_iter().take(pane_count).enumerate() {
                    let mut pane_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .id_salt(("workspace_pane", index))
                            .max_rect(rect)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    pane_ui.set_clip_rect(ui.clip_rect().intersect(rect));
                    render_pane(
                        &mut pane_ui,
                        &mut self.panes[index],
                        PaneOptions {
                            index,
                            active: self.active == index,
                            settings: &self.settings,
                            folder_views: &self.folder_views,
                            idle: drop_enabled,
                            zed_available,
                        },
                        &mut actions,
                        &mut drop_targets,
                        &mut tab_targets,
                        Some(&mut self.thumbnails),
                    );
                }
                ui.advance_cursor_after_rect(workspace);
            });
        self.thumbnails.finish_frame(ctx);
        if self.dialog.is_none()
            && self.comparison.is_none()
            && !self.conflict_open()
            && self.sftp_form.is_none()
            && !self.show_help
            && !self.show_history
            && !self.recovery.is_open()
            && self.shortcut_editor.is_none()
            && self.command_palette.is_none()
            && self.quick_look.is_none()
            && self.folder_jump.is_none()
            && !self.show_accessibility
            && !self.show_list_columns
            && !self.search.visible
        {
            for action in actions {
                self.action(action, ctx);
            }
        }
        let enabled = self.drop_enabled();
        if let Some(drop) = tab_targets.finish(ctx, enabled && !self.native_drag.active()) {
            let destination = drop.destination;
            if (self.settings.split || destination == 0) && move_tab(&mut self.panes, drop, ctx) {
                self.active = destination;
            }
        }
        if let Some(Err(error)) = self.native_drag.handoff(
            ctx,
            frame,
            enabled && self.job.is_none() && self.pending_jobs.is_empty(),
        ) {
            self.notice = Some((error, true));
        }
        if let Some(result) = drop_targets.finish(ctx, enabled) {
            match result {
                Ok(request) => self.drop_files(request, ctx),
                Err(error) => self.notice = Some((error, true)),
            }
        }
        if let Some(action) = self.search.panel.poll(ctx) {
            self.dispatch_search_result(action, ctx);
        }
        if self.dialog.is_none()
            && let Some(transfer) = &mut self.transfer
            && let Some(conflict) = &mut transfer.conflict
        {
            if conflict.show(ctx) {
                transfer.conflict = None;
                if let Some(palette) = &mut self.command_palette {
                    palette.refocus_query();
                }
                if let Some(jump) = &mut self.folder_jump {
                    jump.refocus_query();
                }
                ctx.request_repaint();
            }
            return;
        }
        self.restore_dialog_file_focus(ctx);
        if let Some(id) = quick_look_focus
            && !self.keyboard_blocked(ctx)
            && self.tab().rendered_rows.contains_key(&id)
        {
            ctx.memory_mut(|memory| memory.request_focus(id));
            ctx.request_repaint();
        }
        if let Some(id) = self.palette_focus_to_restore.take()
            && !self.keyboard_blocked(ctx)
            && !ctx.egui_wants_keyboard_input()
        {
            ctx.memory_mut(|memory| memory.request_focus(id));
            ctx.request_repaint();
        }
        let picker_keys = if let Some(events) = opening_palette_input {
            ctx.input_mut(|input| input.events = events);
            CommandPalette::take_keys(ctx)
        } else {
            picker_keys
        };
        if self.command_palette.is_some() {
            self.show_command_palette(ctx, &picker_keys);
            // Query text belongs to the palette, including when the command
            // opens another dialog later in the frame.
            ctx.input_mut(|input| input.events.clear());
        } else {
            self.show_folder_jump(ctx, &picker_keys);
        }
        self.accessibility_dialog(ctx);
        if self.show_list_columns
            && self
                .settings
                .list_columns
                .dialog(ctx, &mut self.list_columns_focus)
        {
            self.show_list_columns = false;
            self.focus_toolbar = true;
            ctx.request_repaint();
        }
        self.dialogs(ctx);
        #[cfg(target_os = "macos")]
        self.show_remote_editor(ctx);
        self.show_comparison(ctx);
        if self
            .properties
            .as_ref()
            .is_some_and(|properties| !properties.show(ctx))
        {
            self.properties = None;
        }
        self.sftp_dialog(ctx);
        if self
            .shortcut_editor
            .as_mut()
            .is_some_and(|editor| editor.show(ctx, &mut self.settings.shortcuts))
        {
            self.shortcut_editor = None;
        }
        if self
            .bookmark_editor
            .as_mut()
            .is_some_and(|editor| editor.show(ctx, &mut self.bookmarks))
        {
            self.bookmark_editor = None;
        }
        if validate_bookmark_shortcuts(&mut self.bookmarks, &self.settings.shortcuts) {
            self.notice = Some((
                crate::i18n::text!("Сочетание избранного очищено: клавиша занята другой командой")
                    .into(),
                false,
            ));
        }
        if let Some(path) = self.search.show(ctx, self.tools.locate.as_deref()) {
            self.reveal_search_result(path, ctx);
        }
        if self.job.is_some()
            || self.application_job.is_some()
            || self.connection.is_some()
            || self
                .panes
                .iter()
                .any(|p| p.tabs.iter().any(Tab::blocking_load))
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        self.keyboard_had_focus =
            ctx.egui_wants_keyboard_input() && self.file_row_focus(ctx).is_none();
    }
    #[cfg(target_os = "macos")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(terminal) = &mut self.embedded_terminal {
            terminal.shutdown();
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // Leave readable paths for older builds and if the session cannot be decoded.
        let mut pinned_tab_counts = [0; 2];
        self.settings.locations = self
            .panes
            .iter()
            .enumerate()
            .map(|(i, pane)| {
                pane.tabs
                    .iter()
                    .filter(|tab| tab.path.has_utf8_paths())
                    .map(|tab| {
                        pinned_tab_counts[i] += usize::from(tab.pinned);
                        tab.path.clone()
                    })
                    .collect()
            })
            .collect();
        self.settings.pinned_tab_counts = pinned_tab_counts;
        self.settings.bookmarks = self
            .bookmarks
            .iter()
            .filter(|bookmark| bookmark.location.has_utf8_paths())
            .map(|bookmark| bookmark.location.clone())
            .collect();
        let saved_bookmarks: Vec<SavedBookmark> =
            self.bookmarks.iter().map(SavedBookmark::from).collect();
        let session = WorkspaceSession {
            panes: std::array::from_fn(|i| self.panes[i].session()),
            active: self.active,
        };
        let saved_views: Vec<_> = self
            .folder_views
            .iter()
            .map(|(location, &view)| SavedFolderView {
                location: location.into(),
                view,
            })
            .collect();
        eframe::set_value(storage, Self::SETTINGS_KEY, &self.settings);
        eframe::set_value(storage, Self::BOOKMARKS_KEY, &saved_bookmarks);
        eframe::set_value(storage, Self::SESSION_KEY, &session);
        eframe::set_value(storage, Self::WORKSPACES_KEY, &self.workspaces);
        #[cfg(target_os = "macos")]
        {
            eframe::set_value(storage, Self::SYNC_PROFILES_KEY, &self.sync_profiles);
            eframe::set_value(storage, Self::COLLECTIONS_KEY, &self.collections);
            eframe::set_value(storage, Self::FILE_TEMPLATES_KEY, &self.file_templates);
            eframe::set_value(
                storage,
                Self::EXTENSIONS_KEY,
                &self.extensions.catalog.installed(),
            );
        }
        eframe::set_value(storage, Self::FOLDER_VIEWS_KEY, &saved_views);
        self.search.save(storage);
    }
}

fn file_name(path: &Location) -> String {
    path.name()
}

#[cfg(test)]
mod tests;
