use super::bookmarks::Bookmark;
use super::dialogs::{Dialog, SftpForm};
use super::state::{is_dot_file, FileOrder, FolderMode, FolderViewChange};
use super::tabs::Tab;
use super::{file_name, PawCommander};
use crate::drag_drop::DropRequest;
use crate::fs_ops::TransferKind;
use crate::shortcuts::Command;
use crate::vfs::Location;
use eframe::egui;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::mpsc;

#[derive(Clone)]
pub(super) enum ColumnNavigation {
    Left,
    Right,
    Open(Location),
    Browse(Location),
    Reveal(Location),
}

#[derive(Clone)]
pub(super) enum Action {
    Shortcut(Command),
    MoveSelection {
        delta: isize,
        extend: bool,
    },
    ToggleCursorSelection,
    ClearSelection,
    ColumnStep {
        right: bool,
    },
    Focus(usize),
    FolderView {
        pane: usize,
        tab: u64,
        location: Location,
        change: FolderViewChange,
    },
    ColumnNavigation {
        pane: usize,
        tab: u64,
        location: Location,
        navigation: ColumnNavigation,
    },
    Navigate(usize, Location),
    BookmarkSlot(u8),
    Open(usize, Location),
    OpenInZed(usize, Location),
    Back,
    Forward,
    History(usize, isize),
    Up,
    Refresh,
    NewTab(usize),
    CloseTab(usize, usize),
    TogglePinTab(usize, usize),
    DuplicateTab(usize, usize),
    ReopenTab(usize),
    Copy(bool),
    Paste(bool),
    NewFolder,
    Rename,
    CreateArchive,
    Trash,
    Transfer(TransferKind),
    Bookmark,
    SaveWorkspace,
    OpenWorkspace(usize),
    RenameWorkspace(usize),
    RemoveWorkspace(usize),
    Reveal,
    CopyPath,
    ConnectSftp,
    Properties,
}

pub(super) enum SelectionChange {
    All,
    Invert,
    Restore,
    Matches(BTreeSet<Location>),
}

impl PawCommander {
    pub(super) fn change_folder_view(&mut self, index: usize, change: FolderViewChange) {
        let current = self.folder_view(index);
        let location = self.panes[index].tab().path.clone();
        let view = self.folder_views.entry(location.clone()).or_default();
        match change {
            FolderViewChange::Mode(mode) => {
                view.mode = Some(mode);
                view.grid = None;
            }
            FolderViewChange::Hidden(hidden) => {
                view.hidden = Some(hidden);
                if !hidden {
                    for tab in self.panes.iter_mut().flat_map(|pane| &mut pane.tabs) {
                        if tab.path == location {
                            tab.hide_dot_file_selection();
                        }
                    }
                }
            }
            FolderViewChange::Density(density) => view.density = Some(density),
            FolderViewChange::Sort(key) => {
                view.order = Some(FileOrder {
                    key,
                    descending: current.order.key == key && !current.order.descending,
                });
            }
        }
    }

    pub(super) fn drop_files(&mut self, request: DropRequest, ctx: &egui::Context) {
        let DropRequest {
            sources,
            destination,
            pane,
            kind,
        } = request;
        if sources.is_empty() || !self.drop_enabled() {
            return;
        }
        self.active = pane;
        self.start_transfer(
            ctx,
            if kind == TransferKind::Move {
                crate::i18n::text("Перемещение файлов")
            } else {
                crate::i18n::text("Копирование файлов")
            },
            sources,
            destination,
            kind,
            false,
        );
    }

    pub(super) fn open_in_zed(&mut self, path: Location, text_only: bool, ctx: &egui::Context) {
        if self.editor_job.is_some() {
            return;
        }
        if path.is_archive() {
            self.notice = Some((crate::archive::readonly().into(), true));
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        self.editor_job = Some(rx);
        self.notice = Some((crate::i18n::text("Открытие файла в Zed…").into(), false));
        std::thread::spawn(move || {
            let _ = tx.send(crate::zed::open(&path, text_only));
            ctx.request_repaint();
        });
    }

    pub(super) fn action(&mut self, action: Action, ctx: &egui::Context) {
        if self.tab().path.is_archive()
            && matches!(
                action,
                Action::Copy(_)
                    | Action::Paste(_)
                    | Action::NewFolder
                    | Action::Rename
                    | Action::CreateArchive
                    | Action::Trash
                    | Action::Transfer(_)
                    | Action::Properties
                    | Action::Reveal
            )
        {
            self.notice = Some((crate::archive::readonly().into(), true));
            return;
        }
        match action {
            Action::Shortcut(command) => self.run_shortcut(command, ctx),
            Action::MoveSelection { delta, extend } => {
                let view = self.folder_view(self.active);
                self.tab_mut().move_selection(delta, view, extend);
                self.tab().focus_cursor(ctx);
            }
            Action::ToggleCursorSelection => {
                let view = self.folder_view(self.active);
                let tab = self.tab_mut();
                let visible = tab.visible(view);
                if let Some(index) = visible
                    .iter()
                    .copied()
                    .find(|&index| Some(&tab.entries[index].path) == tab.cursor.as_ref())
                {
                    tab.choose(index, &visible, egui::Modifiers::COMMAND);
                    tab.focus_cursor(ctx);
                }
            }
            Action::ClearSelection => {
                let view = self.folder_view(self.active);
                let tab = self.tab_mut();
                if !tab.selected.is_empty() {
                    tab.remember_selection(&tab.visible(view));
                }
                tab.selected.clear();
                tab.cursor = None;
                tab.anchor = None;
                tab.column_reveal = None;
                tab.filter.clear();
            }
            Action::ColumnStep { right } => {
                if self.folder_view(self.active).mode == FolderMode::Columns {
                    let navigation = if right {
                        ColumnNavigation::Right
                    } else {
                        ColumnNavigation::Left
                    };
                    self.navigate_columns(self.active, navigation, ctx);
                }
            }
            Action::FolderView {
                pane,
                tab,
                location,
                change,
            } => {
                let current = self.panes[pane].tab();
                if current.id == tab && current.path == location {
                    self.change_folder_view(pane, change);
                    self.active = pane;
                    ctx.request_repaint();
                }
            }
            Action::ColumnNavigation {
                pane,
                tab,
                location,
                navigation,
            } => {
                let current = self.panes[pane].tab();
                if current.id == tab
                    && current.path == location
                    && self.folder_view(pane).mode == FolderMode::Columns
                    && (self.settings.split || pane == 0)
                {
                    self.navigate_columns(pane, navigation, ctx);
                }
            }
            Action::ConnectSftp => {
                let mut form = SftpForm::default();
                if let Location::Sftp { endpoint, path } = &self.tab().path {
                    form.host = endpoint.host.clone();
                    form.port = endpoint.port;
                    form.user = endpoint.user.clone();
                    form.key = endpoint
                        .key
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    form.path = path.display().to_string();
                }
                self.sftp_form = Some(form);
            }
            Action::Focus(i) => self.active = i,
            Action::Navigate(i, p) => {
                self.active = i;
                self.panes[i].tab_mut().navigate(p, ctx);
            }
            Action::BookmarkSlot(slot) => {
                if let Some(location) = self
                    .bookmarks
                    .iter()
                    .find(|bookmark| bookmark.shortcut == Some(slot))
                    .map(|bookmark| bookmark.location.clone())
                {
                    self.action(Action::Navigate(self.active, location), ctx);
                }
            }
            Action::SaveWorkspace => {
                self.dialog = Some(Dialog::WorkspaceName {
                    editing: None,
                    name: String::new(),
                });
                self.dialog_focus = true;
            }
            Action::OpenWorkspace(index) => {
                self.open_workspace(index, ctx);
            }
            Action::RenameWorkspace(index) => {
                if let Some(workspace) = self.workspaces.get(index) {
                    self.dialog = Some(Dialog::WorkspaceName {
                        editing: Some(index),
                        name: workspace.name.clone(),
                    });
                    self.dialog_focus = true;
                }
            }
            Action::RemoveWorkspace(index) => {
                if index < self.workspaces.len() {
                    self.workspaces.remove(index);
                }
            }
            Action::OpenInZed(i, path) => {
                self.active = i;
                self.open_in_zed(path, false, ctx);
            }
            Action::Open(i, p) => {
                self.active = i;
                let directory = self.panes[i]
                    .tab()
                    .entries
                    .iter()
                    .any(|e| e.path == p && e.is_dir);
                let browse = if directory {
                    Some(p.clone())
                } else {
                    p.browse_archive()
                };
                if let Some(browse) = browse {
                    if browse.is_archive() {
                        self.notice = None;
                    }
                    if self.folder_view(i).mode == FolderMode::Columns {
                        self.navigate_columns(i, ColumnNavigation::Browse(browse), ctx);
                    } else {
                        self.panes[i].tab_mut().navigate(browse, ctx);
                    }
                } else if p.is_archive() {
                    self.notice = Some((crate::archive::readonly().into(), true));
                } else if p.remote() {
                    self.open_in_zed(p, true, ctx);
                } else if let Some(path) = p.local()
                    && let Err(e) = open::that_detached(path)
                {
                    self.notice = Some((
                        crate::tr_format!(
                            "Не удалось открыть файл: {e}",
                            "Could not open file: {e}"
                        ),
                        true,
                    ));
                }
            }
            Action::History(index, delta) => {
                self.active = index;
                self.navigate_history(index, delta, ctx);
            }
            Action::Back => self.navigate_history(self.active, -1, ctx),
            Action::Forward => self.navigate_history(self.active, 1, ctx),
            Action::Up => {
                if self.folder_view(self.active).mode == FolderMode::Columns {
                    self.navigate_columns(self.active, ColumnNavigation::Left, ctx);
                } else if let Some(parent) = self.tab().path.parent() {
                    self.tab_mut().navigate(parent, ctx);
                }
            }
            Action::Properties => {
                let selected = self.selected();
                if self.properties.is_none()
                    && !selected.is_empty()
                    && selected.iter().all(|path| path.local().is_some())
                {
                    self.properties = Some(crate::properties::Properties::start(
                        selected
                            .into_iter()
                            .filter_map(|path| path.local().map(Path::to_path_buf))
                            .collect(),
                        ctx,
                    ));
                }
            }
            Action::Refresh => {
                self.thumbnails.refresh();
                self.refresh(ctx);
            }
            Action::NewTab(i) => {
                let pane = &mut self.panes[i];
                let path = pane.tab().path.clone();
                pane.insert_selected_tab(pane.tabs.len(), Tab::new(path, ctx));
                self.active = i;
            }
            Action::CloseTab(i, t) => {
                self.panes[i].close_tab(t);
            }
            Action::TogglePinTab(i, t) => self.panes[i].toggle_pin(t),
            Action::DuplicateTab(i, t) => {
                let pane = &mut self.panes[i];
                let tab = Tab::new(pane.tabs[t].path.clone(), ctx);
                let slot = (t + 1).max(pane.pinned_count());
                pane.insert_selected_tab(slot, tab);
                self.active = i;
            }
            Action::ReopenTab(i) => {
                if self.panes[i].reopen_tab(ctx) {
                    self.active = i;
                }
            }
            Action::Copy(cut) => self.copy_files(cut),
            Action::Paste(move_here) if self.drop_enabled() => self.paste_files(move_here, ctx),
            Action::Transfer(kind) if !self.conflict_open() && self.settings.split => {
                let sources = self.selected();
                let destination = self.panes[1 - self.active].tab().path.clone();
                self.start_transfer(
                    ctx,
                    if kind == TransferKind::Move {
                        crate::i18n::text("Перемещение в соседнюю панель")
                    } else {
                        crate::i18n::text("Копирование в соседнюю панель")
                    },
                    sources,
                    destination,
                    kind,
                    false,
                );
            }
            Action::NewFolder if !self.conflict_open() => {
                self.remember_dialog_file_focus(ctx);
                self.dialog = Some(Dialog::NewFolder {
                    parent: self.tab().path.clone(),
                    name: String::new(),
                });
                self.dialog_focus = true;
            }
            Action::Rename if !self.conflict_open() => {
                let selected = self.selected();
                if !selected.is_empty() {
                    self.remember_dialog_file_focus(ctx);
                    self.dialog = Some(if selected.len() == 1 {
                        let source = selected.into_iter().next().unwrap();
                        let name = file_name(&source);
                        Dialog::Rename { source, name }
                    } else {
                        Dialog::BatchRename(Box::new(super::batch_rename::BatchRenameDialog::new(
                            selected,
                        )))
                    });
                    self.dialog_focus = true;
                }
            }
            Action::CreateArchive if !self.conflict_open() => {
                let sources = self.selected();
                if !sources.is_empty() {
                    self.remember_dialog_file_focus(ctx);
                    self.dialog = Some(Dialog::CreateArchive {
                        sources,
                        directory: self.tab().path.clone(),
                        name: crate::i18n::text("Архив").into(),
                        format: crate::archive::Format::Zip,
                    });
                    self.dialog_focus = true;
                }
            }
            Action::Trash if !self.conflict_open() => {
                let paths = self.selected();
                if !paths.is_empty() {
                    self.remember_dialog_file_focus(ctx);
                    self.dialog = Some(Dialog::Trash { paths });
                    self.dialog_focus = true;
                }
            }
            Action::Bookmark => {
                let path = self.tab().path.clone();
                if !self
                    .bookmarks
                    .iter()
                    .any(|bookmark| bookmark.location == path)
                {
                    self.bookmarks.push(Bookmark::new(path));
                }
            }
            Action::Reveal => {
                let location = self
                    .selected()
                    .first()
                    .cloned()
                    .unwrap_or_else(|| self.tab().path.clone());
                let Some(path) = location.local() else {
                    self.notice = Some((
                        crate::i18n::text("SFTP нельзя показать в системном файловом менеджере")
                            .into(),
                        true,
                    ));
                    return;
                };
                #[cfg(target_os = "macos")]
                let result = std::process::Command::new("open")
                    .arg("-R")
                    .arg(path)
                    .spawn()
                    .map(|mut child| {
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                    });
                #[cfg(not(target_os = "macos"))]
                let result = open::that_detached(path.parent().unwrap_or(path));
                if let Err(e) = result {
                    self.notice = Some((
                        crate::tr_format!(
                            "Не удалось показать файл: {e}",
                            "Could not reveal file: {e}"
                        ),
                        true,
                    ));
                }
            }
            Action::CopyPath => {
                let paths = self.selected();
                ctx.copy_text(if paths.is_empty() {
                    self.tab().path.display().to_string()
                } else {
                    paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                });
            }
            _ => {}
        }
    }

    pub(super) fn navigate_columns(
        &mut self,
        pane: usize,
        navigation: ColumnNavigation,
        ctx: &egui::Context,
    ) {
        let tab = self.panes[pane].tab();
        let (directory, selection) = match navigation {
            ColumnNavigation::Left => {
                let Some(parent) = tab.path.parent() else {
                    return;
                };
                let reveal = if !parent.is_archive() {
                    tab.path
                        .archive_source()
                        .map(|file| Location::Local(file.to_owned()))
                } else {
                    None
                };
                (parent, Some(reveal.unwrap_or_else(|| tab.path.clone())))
            }
            ColumnNavigation::Right => {
                let visible = tab.visible(self.folder_view(pane));
                let Some(entry) = visible
                    .iter()
                    .map(|&index| &tab.entries[index])
                    .find(|entry| tab.cursor.as_ref() == Some(&entry.path))
                else {
                    return;
                };
                let directory = if entry.is_dir {
                    entry.path.clone()
                } else if let Some(archive) = entry.path.browse_archive() {
                    archive
                } else {
                    return;
                };
                (directory, None)
            }
            ColumnNavigation::Open(path) => {
                if tab.entries.iter().any(|entry| entry.path == path) {
                    self.action(Action::Open(pane, path), ctx);
                }
                return;
            }
            ColumnNavigation::Browse(directory) => (directory, None),
            ColumnNavigation::Reveal(path) => {
                let Some(parent) = path.parent() else {
                    return;
                };
                (parent, Some(path))
            }
        };
        Self::restore_focus(ctx, None);
        self.active = pane;
        if directory == tab.path {
            return;
        }
        let show_hidden = self
            .folder_views
            .get(&directory)
            .and_then(|view| view.hidden)
            .unwrap_or(self.settings.hidden);
        let selection = selection.filter(|path| show_hidden || !is_dot_file(path.path()));
        self.panes[pane]
            .tab_mut()
            .navigate_column(directory, selection, ctx);
    }

    pub(super) fn navigate_history(&mut self, pane: usize, delta: isize, ctx: &egui::Context) {
        let in_columns = self.folder_view(pane).mode == FolderMode::Columns;
        let tab = self.panes[pane].tab_mut();
        let previous = tab.path.clone();
        let browsing_columns = in_columns || tab.browse_columns;
        tab.history_go(delta, ctx);
        let archive_boundary = |child: &Location, parent: &Location| {
            child.is_archive()
                && parent.local().is_some()
                && child.parent().as_ref() == Some(parent)
        };
        if browsing_columns
            && previous != tab.path
            && (previous.same_backend(&tab.path)
                || archive_boundary(&previous, &tab.path)
                || archive_boundary(&tab.path, &previous))
        {
            tab.browse_columns = true;
        }
    }

    pub(super) fn toggle_split(&mut self) {
        self.settings.split = !self.settings.split;
        if !self.settings.split {
            self.panes[1].tab_mut().address_completion.suspend();
            self.active = 0;
        }
    }

    pub(super) fn apply_selection(&mut self, change: SelectionChange, ctx: &egui::Context) {
        if matches!(change, SelectionChange::Restore) && self.tab().previous_selection.is_none() {
            return;
        }
        let view = self.folder_view(self.active);
        if matches!(change, SelectionChange::All) {
            self.tab_mut().change_selection(view, change);
            return;
        }
        let rule = matches!(change, SelectionChange::Matches(_));
        let focused = self
            .file_row_focus(ctx)
            .is_some_and(|(pane, _)| pane == self.active)
            || self.tab().focused_cursor(ctx).is_some();
        self.tab_mut().change_selection(view, change);
        let tab = self.tab();
        if focused {
            Self::restore_focus(ctx, tab.cursor.as_ref().map(|path| tab.entry_id(path)));
        }
        let modal_row_focus = self
            .dialog_file_focus
            .as_ref()
            .is_some_and(|(id, _)| *id == tab.id);
        if modal_row_focus {
            self.dialog_file_focus = tab.cursor.clone().map(|path| (tab.id, path));
        }
        if focused || modal_row_focus {
            let tab = self.tab_mut();
            tab.scroll_to = tab.cursor.clone();
        }
        if rule {
            self.notice = self.tab().selected.is_empty().then(|| {
                (
                    crate::i18n::text("Нет подходящих видимых объектов").into(),
                    false,
                )
            });
        }
        ctx.request_repaint();
    }

    pub(super) fn run_pane_action(&mut self, action: Action, ctx: &egui::Context) {
        if self.tab().focused_cursor(ctx).is_some() {
            let tab = self.tab();
            if matches!(action, Action::Shortcut(Command::Open)) {
                let cursor = tab.cursor.as_ref();
                if let Some(path) = tab
                    .visible(self.folder_view(self.active))
                    .iter()
                    .map(|&index| &tab.entries[index].path)
                    .find(|path| Some(*path) == cursor)
                {
                    self.action(Action::Open(self.active, path.clone()), ctx);
                }
                return;
            }
            if matches!(
                action,
                Action::Copy(_)
                    | Action::Shortcut(
                        Command::Rename
                            | Command::CreateArchive
                            | Command::Trash
                            | Command::Properties
                            | Command::CopyToPane
                            | Command::MoveToPane
                            | Command::EditInZed
                    )
            ) && !tab
                .cursor
                .as_ref()
                .is_some_and(|path| tab.selected.contains(path))
            {
                // A focus-only move must never act on another file's selection.
                return;
            }
        }
        self.action(action, ctx);
    }
}
