use super::actions::{Action, ColumnNavigation, SelectionChange};
use super::column_view;
use super::directory::{
    send_progressive_listing, DirectoryEntries, DirectoryLoad, DirectoryLoadMode, DirectoryRead,
    FolderVisit, AUTO_REFRESH_INTERVAL, SFTP_REFRESH_INTERVAL,
};
use super::selection::EntryIdentity;
use super::state::{
    is_dot_file, Density, FileOrder, FolderMode, FolderView, FolderViewChange, FolderViews,
    PaneSession, SavedLocation, Settings, Sort, TabSession,
};
use crate::tab_drag::{clamp_tab_slot, TabDrop};
use crate::vfs as fs_ops;
use crate::vfs::{FileEntry, Location};
use eframe::egui;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Instant;

pub(super) struct Tab {
    pub(super) id: u64,
    pub(super) path: Location,
    pub(super) pinned: bool,
    pub(super) address: String,
    pub(super) edit_address: bool,
    pub(super) focus_address: bool,
    pub(super) address_error: Option<String>,
    pub(super) address_completion: crate::address::Completion,
    pub(super) focus_filter: bool,
    pub(super) filter: String,
    pub(super) entries: DirectoryEntries,
    pub(super) selected: BTreeSet<Location>,
    pub(super) previous_selection: Option<BTreeMap<Location, EntryIdentity>>,
    pub(super) anchor: Option<Location>,
    pub(super) cursor: Option<Location>,
    pub(super) rendered_rows: HashMap<egui::Id, Location>,
    pub(super) scroll_to: Option<Location>,
    pub(super) scroll_offset: f32,
    pub(super) scroll_restore: Option<f32>,
    pub(super) file_scroll_scope: Option<egui::Id>,
    pub(super) file_scroll_geometry: Option<(usize, f32)>,
    pub(super) reveal_after_load: Option<Location>,
    pub(super) column_reveal: Option<Location>,
    pub(super) browse_columns: bool,
    pub(super) history: Vec<Location>,
    pub(super) history_index: usize,
    pub(super) loading: Option<DirectoryLoad>,
    pub(super) has_complete_listing: bool,
    pub(super) folder_visit: FolderVisit,
    pub(super) archive_index: Option<Arc<crate::archive::Index>>,
    pub(super) next_auto_refresh: Instant,
    pub(super) error: Option<String>,
    pub(super) sort: Sort,
    pub(super) descending: bool,
}

impl Tab {
    pub(super) fn new(path: Location, ctx: &egui::Context) -> Self {
        static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(1);
        let mut tab = Self {
            id: NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed),
            address: path.display().to_string(),
            path: path.clone(),
            pinned: false,
            edit_address: false,
            focus_address: false,
            address_error: None,
            address_completion: Default::default(),
            focus_filter: false,
            filter: String::new(),
            entries: DirectoryEntries::default(),
            selected: BTreeSet::new(),
            previous_selection: None,
            anchor: None,
            cursor: None,
            rendered_rows: HashMap::new(),
            scroll_to: None,
            scroll_offset: 0.0,
            scroll_restore: None,
            file_scroll_scope: None,
            file_scroll_geometry: None,
            reveal_after_load: None,
            column_reveal: None,
            browse_columns: false,
            history: vec![path],
            history_index: 0,
            loading: None,
            has_complete_listing: false,
            folder_visit: FolderVisit::Unseen,
            archive_index: None,
            next_auto_refresh: Instant::now() + AUTO_REFRESH_INTERVAL,
            error: None,
            sort: Sort::default(),
            descending: false,
        };
        tab.reload(ctx);
        tab
    }

    pub(super) fn from_session(saved: TabSession, home: &Path, ctx: &egui::Context) -> Self {
        let path = saved
            .path
            .into_location()
            .unwrap_or_else(|| home.to_path_buf().into());
        let mut tab = Self::new(path, ctx);
        tab.pinned = saved.pinned;
        tab.sort = saved.sort;
        tab.descending = saved.descending;
        tab.browse_columns = saved.browse_columns;
        if saved.scroll_offset.is_finite() && saved.scroll_offset >= 0.0 {
            tab.scroll_restore = Some(saved.scroll_offset);
        }
        if let Some(history) = saved
            .history
            .into_iter()
            .map(SavedLocation::into_location)
            .collect::<Option<Vec<_>>>()
            .filter(|history| history.get(saved.history_index) == Some(&tab.path))
        {
            tab.history = history;
            tab.history_index = saved.history_index;
        }
        tab
    }

    pub(super) fn session(&self) -> TabSession {
        TabSession {
            path: (&self.path).into(),
            pinned: self.pinned,
            history: self.history.iter().map(SavedLocation::from).collect(),
            history_index: self.history_index,
            sort: self.sort,
            descending: self.descending,
            scroll_offset: self.scroll_restore.unwrap_or(self.scroll_offset),
            browse_columns: self.browse_columns,
        }
    }
    pub(super) fn begin_address(&mut self) {
        self.address = self.path.display();
        self.edit_address = true;
        self.focus_address = true;
        self.address_error = None;
        self.address_completion.reset();
    }
    pub(super) fn reload(&mut self, ctx: &egui::Context) {
        self.archive_index = None;
        self.start_load(ctx, false);
    }
    pub(super) fn start_load(&mut self, ctx: &egui::Context, automatic: bool) {
        let mode = if automatic {
            DirectoryLoadMode::Automatic
        } else if self.has_complete_listing {
            DirectoryLoadMode::Reload
        } else {
            DirectoryLoadMode::Progressive
        };
        let (tx, load) = DirectoryLoad::channel(mode);
        let cancelled = load.cancelled.clone();
        let path = self.path.clone();
        let archive_index = self.archive_index.clone();
        let ctx = ctx.clone();
        if !automatic {
            self.error = None;
        }
        if mode == DirectoryLoadMode::Progressive {
            self.entries.replace(Vec::new());
        }
        self.loading = Some(load);
        if let Err(error) = std::thread::Builder::new()
            .name("pawcommander-directory-read".into())
            .spawn(move || {
                let cancelled = || cancelled.load(Ordering::Relaxed);
                let send = |update| {
                    if cancelled() || tx.send(update).is_err() {
                        return false;
                    }
                    ctx.request_repaint();
                    true
                };
                if let Some(archive) = path.archive_source() {
                    let result = match archive_index {
                        Some(index) => Ok(index),
                        None => crate::archive::Index::load(archive, &cancelled).map(Arc::new),
                    }
                    .and_then(|index| Ok((Arc::clone(&index), index.list(&path, &cancelled)?)));
                    send(DirectoryRead::Archive(result));
                    return;
                }
                let result = match mode {
                    DirectoryLoadMode::Progressive => {
                        send_progressive_listing(
                            |visit| fs_ops::visit_directory(&path, &cancelled, visit),
                            send,
                        );
                        return;
                    }
                    DirectoryLoadMode::Reload => {
                        fs_ops::read_directory(&path, &cancelled).map(Some)
                    }
                    DirectoryLoadMode::Automatic => {
                        fs_ops::read_automatic_directory(&path, &cancelled)
                    }
                };
                send(DirectoryRead::Finished(result));
            })
        {
            self.finish_load(Err(crate::tr_format!(
                "Не удалось запустить чтение папки: {error}",
                "Could not start reading folder: {error}"
            )));
        }
    }
    pub(super) fn auto_refresh(&mut self, ctx: &egui::Context, now: Instant) {
        if self.path.is_archive() {
            return;
        }
        if self.loading.is_none() && now >= self.next_auto_refresh {
            self.start_load(ctx, true);
        }
        if self.loading.is_none() {
            ctx.request_repaint_after(self.next_auto_refresh.saturating_duration_since(now));
        }
    }
    pub(super) fn blocking_load(&self) -> bool {
        self.loading.is_some() && !self.automatic_loading()
    }
    pub(super) fn automatic_loading(&self) -> bool {
        self.loading
            .as_ref()
            .is_some_and(|load| load.mode == DirectoryLoadMode::Automatic)
    }
    pub(super) fn finish_load(&mut self, result: Result<Option<Vec<FileEntry>>, String>) {
        let mode = self.loading.take().expect("active directory load").mode;
        match result {
            Ok(None) if mode != DirectoryLoadMode::Progressive => {}
            Ok(entries) => {
                if let Some(entries) = entries {
                    self.entries.replace(entries);
                }
                self.has_complete_listing = true;
                if !self.selected.is_empty() || self.anchor.is_some() || self.cursor.is_some() {
                    let paths: BTreeSet<_> = self
                        .entries
                        .iter()
                        .map(|entry| &entry.path)
                        .filter(|path| {
                            self.selected.contains(*path)
                                || self.anchor.as_ref() == Some(*path)
                                || self.cursor.as_ref() == Some(*path)
                        })
                        .collect();
                    self.selected.retain(|path| paths.contains(path));
                    if self
                        .anchor
                        .as_ref()
                        .is_some_and(|path| !paths.contains(path))
                    {
                        self.anchor = None;
                    }
                    if self
                        .cursor
                        .as_ref()
                        .is_some_and(|path| !paths.contains(path))
                    {
                        self.cursor = None;
                    }
                }
                self.reconcile_previous_selection();
                self.error = None;
                if let Some(path) = self.column_reveal.take()
                    && self.entries.iter().any(|entry| entry.path == path)
                {
                    self.select_revealed(path);
                }
                if let Some(path) = self.reveal_after_load.take() {
                    if self.entries.iter().any(|entry| entry.path == path) {
                        self.select_revealed(path);
                    } else {
                        self.error = Some(
                            crate::i18n::text(
                                "Результат поиска уже отсутствует в папке. Выполните поиск снова.",
                            )
                            .into(),
                        );
                    }
                }
            }
            Err(error) => {
                self.reveal_after_load = None;
                self.column_reveal = None;
                if !self.has_complete_listing && !self.entries.is_empty() {
                    self.error = Some(crate::tr_format!(
                        "Показана часть содержимого папки. {error}",
                        "Showing an incomplete folder listing. {error}"
                    ));
                } else if mode == DirectoryLoadMode::Automatic {
                    self.error = Some(if self.has_complete_listing {
                        crate::tr_format!("Автообновление: {error}. Показан последний прочитанный список; повторим попытку автоматически.", "Auto refresh: {error}. Showing the last successful listing; will retry automatically.")
                    } else {
                        error
                    });
                } else {
                    self.error = Some(error);
                    self.has_complete_listing = false;
                    self.entries.replace(Vec::new());
                    self.selected.clear();
                    self.previous_selection = None;
                    self.anchor = None;
                    self.cursor = None;
                }
            }
        }
        let interval = if self.path.remote() {
            SFTP_REFRESH_INTERVAL
        } else {
            AUTO_REFRESH_INTERVAL
        };
        self.next_auto_refresh = Instant::now() + interval;
    }
    pub(super) fn select_revealed(&mut self, path: Location) {
        self.selected = BTreeSet::from([path.clone()]);
        self.anchor = Some(path.clone());
        self.cursor = Some(path.clone());
        self.scroll_to = Some(path);
    }

    pub(super) fn navigate_column(
        &mut self,
        path: Location,
        reveal: Option<Location>,
        ctx: &egui::Context,
    ) {
        self.navigate(path, ctx);
        self.browse_columns = true;
        self.column_reveal = reveal;
    }

    pub(super) fn navigate(&mut self, path: Location, ctx: &egui::Context) {
        if path == self.path {
            self.reload(ctx);
            return;
        }
        if path.archive_source() != self.path.archive_source() {
            self.archive_index = None;
        }
        self.path = path.clone();
        self.address = path.display().to_string();
        self.history.truncate(self.history_index + 1);
        self.history.push(path);
        self.history_index = self.history.len() - 1;
        self.reset_and_load(ctx);
    }
    pub(super) fn reset_and_load(&mut self, ctx: &egui::Context) {
        self.has_complete_listing = false;
        self.folder_visit = FolderVisit::CountOnOpen;
        self.selected.clear();
        self.previous_selection = None;
        self.anchor = None;
        self.cursor = None;
        self.scroll_to = None;
        self.scroll_offset = 0.0;
        self.scroll_restore = None;
        self.file_scroll_scope = None;
        self.file_scroll_geometry = None;
        self.reveal_after_load = None;
        self.column_reveal = None;
        self.browse_columns = false;
        self.rendered_rows.clear();
        self.filter.clear();
        self.edit_address = false;
        self.address_completion.reset();
        self.start_load(ctx, false);
    }
    pub(super) fn history_go(&mut self, delta: isize, ctx: &egui::Context) {
        let index = self.history_index as isize + delta;
        if index >= 0 && (index as usize) < self.history.len() {
            self.history_index = index as usize;
            if self.history[self.history_index].archive_source() != self.path.archive_source() {
                self.archive_index = None;
            }
            self.path = self.history[self.history_index].clone();
            self.address = self.path.display().to_string();
            self.reset_and_load(ctx);
        }
    }
    // Consume at most one batch per pass so enumeration cannot monopolize the UI.
    pub(super) fn poll(&mut self) -> bool {
        if let Some(load) = &self.loading {
            match load.receiver.try_recv() {
                Ok(DirectoryRead::Batch(entries)) => {
                    self.entries.extend(entries);
                    return true;
                }
                Ok(DirectoryRead::Finished(result)) => {
                    self.finish_load(result);
                }
                Ok(DirectoryRead::Archive(result)) => {
                    let result = result.map(|(index, entries)| {
                        self.archive_index = Some(index);
                        Some(entries)
                    });
                    self.finish_load(result);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.finish_load(Err(crate::i18n::text(
                        "Не удалось прочитать папку: фоновая задача завершилась",
                    )
                    .into()));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        false
    }
    pub(super) fn folder_view(
        &self,
        settings: &Settings,
        pane: usize,
        views: &FolderViews,
    ) -> FolderView {
        let defaults = FolderView {
            mode: if self.browse_columns {
                FolderMode::Columns
            } else {
                FolderMode::from_grid(settings.grid_for(pane))
            },
            hidden: settings.hidden,
            density: Density::default(),
            order: self.legacy_order(),
        };
        views
            .get(&self.path)
            .map_or(defaults, |view| view.resolve(defaults))
    }

    pub(super) fn folder_view_action(&self, pane: usize, change: FolderViewChange) -> Action {
        Action::FolderView {
            pane,
            tab: self.id,
            location: self.path.clone(),
            change,
        }
    }

    pub(super) fn column_action(&self, pane: usize, navigation: ColumnNavigation) -> Action {
        Action::ColumnNavigation {
            pane,
            tab: self.id,
            location: self.path.clone(),
            navigation,
        }
    }

    pub(super) fn legacy_order(&self) -> FileOrder {
        FileOrder {
            key: self.sort,
            descending: self.descending,
        }
    }

    pub(super) fn visible(&self, view: FolderView) -> Rc<Vec<usize>> {
        self.entries.visible(
            view.hidden,
            &self.filter,
            view.order.key,
            view.order.descending,
        )
    }

    pub(super) fn hide_dot_file_selection(&mut self) {
        self.selected.retain(|path| !is_dot_file(path.path()));
        // Keep the cursor while egui still focuses the row from the last render.
        // Further commands in this input batch must respect that focus.
        for target in [
            &mut self.anchor,
            &mut self.scroll_to,
            &mut self.reveal_after_load,
            &mut self.column_reveal,
        ] {
            if target.as_ref().is_some_and(|path| is_dot_file(path.path())) {
                *target = None;
            }
        }
    }
    pub(super) fn choose(&mut self, index: usize, visible: &[usize], modifiers: egui::Modifiers) {
        self.column_reveal = None;
        let path = self.entries[index].path.clone();
        self.cursor = Some(path.clone());
        if modifiers.shift {
            let anchor = self
                .anchor
                .as_ref()
                .and_then(|p| visible.iter().position(|i| &self.entries[*i].path == p));
            let end = visible.iter().position(|i| *i == index).unwrap_or(0);
            if !modifiers.command {
                self.selected.clear();
            }
            for i in &visible[anchor.unwrap_or(end).min(end)..=anchor.unwrap_or(end).max(end)] {
                self.selected.insert(self.entries[*i].path.clone());
            }
            if anchor.is_none() {
                self.anchor = Some(path);
            }
        } else if modifiers.command {
            if !self.selected.remove(&path) {
                self.selected.insert(path.clone());
            }
            self.anchor = Some(path);
        } else {
            self.selected.clear();
            self.selected.insert(path.clone());
            self.anchor = Some(path);
        }
    }

    pub(super) fn entry_id(&self, path: &Location) -> egui::Id {
        egui::Id::unique(("file_entry", self.id, path))
    }

    pub(super) fn file_list_id(&self) -> egui::Id {
        // Stable row IDs need the same accessibility parent in every view.
        egui::Id::unique(("file_list", self.id))
    }

    pub(super) fn focus_cursor(&self, ctx: &egui::Context) {
        if let Some(path) = &self.cursor {
            ctx.memory_mut(|memory| memory.request_focus(self.entry_id(path)));
        }
    }

    pub(super) fn focused_cursor(&self, ctx: &egui::Context) -> Option<egui::Id> {
        let id = self.entry_id(self.cursor.as_ref()?);
        ctx.memory(|memory| memory.has_focus(id)).then_some(id)
    }

    pub(super) fn selected_visible(&self, view: FolderView) -> Vec<Location> {
        self.visible(view)
            .iter()
            .map(|&i| &self.entries[i].path)
            .filter(|path| self.selected.contains(*path))
            .cloned()
            .collect()
    }

    pub(super) fn selected_file_count(&self, visible: &[usize]) -> Option<usize> {
        if self.path.is_archive() {
            return None;
        }
        let mut count = 0;
        for entry in visible
            .iter()
            .map(|&index| &self.entries[index])
            .filter(|entry| self.selected.contains(&entry.path))
        {
            if entry.is_dir || entry.is_symlink {
                return None;
            }
            count += 1;
        }
        (count > 0).then_some(count)
    }

    pub(super) fn remember_selection(&mut self, visible: &[usize]) {
        self.previous_selection = Some(
            visible
                .iter()
                .map(|&index| &self.entries[index])
                .filter(|entry| self.selected.contains(&entry.path))
                .map(|entry| (entry.path.clone(), EntryIdentity::from(entry)))
                .collect(),
        );
    }

    pub(super) fn reconcile_previous_selection(&mut self) {
        let Some(previous) = self
            .previous_selection
            .as_mut()
            .filter(|paths| !paths.is_empty())
        else {
            return;
        };
        let still_present: HashSet<_> = self
            .entries
            .iter()
            .filter(|entry| {
                previous
                    .get(&entry.path)
                    .is_some_and(|identity| *identity == EntryIdentity::from(*entry))
            })
            .map(|entry| &entry.path)
            .collect();
        previous.retain(|path, _| still_present.contains(path));
        if previous.is_empty() {
            self.previous_selection = None;
        }
    }

    pub(super) fn change_selection(&mut self, view: FolderView, change: SelectionChange) {
        let visible = self.visible(view);
        let select_all = matches!(change, SelectionChange::All);
        let restored = if matches!(change, SelectionChange::Restore) {
            let Some(previous) = self.previous_selection.take() else {
                return;
            };
            Some(previous)
        } else {
            None
        };
        let selected = match change {
            SelectionChange::Matches(selected) => selected,
            _ => visible
                .iter()
                .map(|&index| &self.entries[index])
                .filter(|entry| match &change {
                    SelectionChange::All => true,
                    SelectionChange::Invert => !self.selected.contains(&entry.path),
                    SelectionChange::Restore => restored.as_ref().is_some_and(|previous| {
                        previous.get(&entry.path) == Some(&EntryIdentity::from(*entry))
                    }),
                    SelectionChange::Matches(_) => {
                        unreachable!("matched selection already computed")
                    }
                })
                .map(|entry| entry.path.clone())
                .collect(),
        };
        if restored.is_none() && selected != self.selected {
            self.remember_selection(&visible);
        }
        self.selected = selected;
        self.column_reveal = None;
        // Keep the existing navigation cursor and Shift range anchor for Select All.
        if select_all {
            return;
        }
        self.reveal_after_load = None;
        self.cursor = self
            .cursor
            .take()
            .filter(|path| self.selected.contains(path))
            .or_else(|| {
                visible
                    .iter()
                    .map(|&index| &self.entries[index].path)
                    .find(|&path| self.selected.contains(path))
                    .cloned()
            });
        self.anchor = self.cursor.clone();
    }

    pub(super) fn drag_paths(&mut self, index: usize, visible: &[usize]) -> Vec<Location> {
        // Starting a drag on an existing selection must not collapse it.
        if !self.selected.contains(&self.entries[index].path) {
            self.choose(index, visible, egui::Modifiers::NONE);
        }
        visible
            .iter()
            .map(|i| &self.entries[*i].path)
            .filter(|path| self.selected.contains(*path))
            .cloned()
            .collect()
    }

    pub(super) fn move_selection(&mut self, delta: isize, view: FolderView, shift: bool) {
        self.column_reveal = None;
        let visible = self.visible(view);
        if visible.is_empty() {
            return;
        }
        let current = self
            .cursor
            .as_ref()
            .and_then(|path| visible.iter().position(|i| &self.entries[*i].path == path));
        let position = current
            .map(|position| {
                (position as isize + delta).clamp(0, visible.len() as isize - 1) as usize
            })
            .unwrap_or_else(|| if delta < 0 { visible.len() - 1 } else { 0 });
        let index = visible[position];
        self.choose(
            index,
            &visible,
            if shift {
                egui::Modifiers::SHIFT
            } else {
                egui::Modifiers::NONE
            },
        );
        self.scroll_to = Some(self.entries[index].path.clone());
    }
}

pub(super) struct Pane {
    pub(super) tabs: Vec<Tab>,
    pub(super) current: usize,
    pub(super) closed_tabs: VecDeque<(usize, Location)>,
    pub(super) tab_context_menu: Option<egui::Id>,
    pub(super) columns: column_view::Columns,
}

impl Pane {
    pub(super) const MAX_CLOSED_TABS: usize = 16;

    pub(super) fn new(tabs: Vec<Tab>) -> Self {
        Self {
            tabs,
            current: 0,
            closed_tabs: VecDeque::new(),
            tab_context_menu: None,
            columns: column_view::Columns::default(),
        }
    }

    pub(super) fn from_session(saved: PaneSession, home: &Path, ctx: &egui::Context) -> Self {
        let mut tabs: Vec<_> = saved
            .tabs
            .into_iter()
            .map(|tab| Tab::from_session(tab, home, ctx))
            .collect();
        if tabs.is_empty() {
            tabs.push(Tab::new(home.to_path_buf().into(), ctx));
        }
        let mut pane = Self::new(tabs);
        pane.current = saved.current.min(pane.tabs.len() - 1);
        pane
    }

    pub(super) fn session(&self) -> PaneSession {
        PaneSession {
            tabs: self.tabs.iter().map(Tab::session).collect(),
            current: self.current,
        }
    }

    pub(super) fn pinned_count(&self) -> usize {
        self.tabs.iter().take_while(|tab| tab.pinned).count()
    }

    pub(super) fn toggle_pin(&mut self, index: usize) {
        let selected = index == self.current;
        let mut tab = self.remove_tab(index);
        tab.pinned = !tab.pinned;
        let slot = self.pinned_count();
        self.tabs.insert(slot, tab);
        if selected {
            self.current = slot;
        } else if slot <= self.current {
            self.current += 1;
        }
    }

    pub(super) fn can_close_tab(&self, index: usize) -> bool {
        self.tabs.len() > 1 && !self.tabs[index].pinned
    }

    pub(super) fn close_tab(&mut self, index: usize) {
        if self.can_close_tab(index) {
            let tab = self.remove_tab(index);
            if self.closed_tabs.len() == Self::MAX_CLOSED_TABS {
                self.closed_tabs.pop_front();
            }
            self.closed_tabs.push_back((index, tab.path));
        }
    }

    pub(super) fn insert_selected_tab(&mut self, index: usize, tab: Tab) {
        self.tab_mut().address_completion.suspend();
        self.tabs.insert(index, tab);
        self.current = index;
    }

    pub(super) fn reopen_tab(&mut self, ctx: &egui::Context) -> bool {
        let Some((index, path)) = self.closed_tabs.pop_back() else {
            return false;
        };
        let slot = index.clamp(self.pinned_count(), self.tabs.len());
        self.insert_selected_tab(slot, Tab::new(path, ctx));
        true
    }

    pub(super) fn remove_tab(&mut self, index: usize) -> Tab {
        let tab = self.tabs.remove(index);
        if index < self.current {
            self.current -= 1;
        }
        self.current = self.current.min(self.tabs.len().saturating_sub(1));
        tab
    }
    pub(super) fn tab(&self) -> &Tab {
        &self.tabs[self.current]
    }
    pub(super) fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.current]
    }
    pub(super) fn select_tab(&mut self, index: usize) {
        if index != self.current {
            self.tab_mut().address_completion.suspend();
            self.current = index;
        }
    }
}

pub(super) fn move_tab(panes: &mut [Pane], drop: TabDrop, ctx: &egui::Context) -> bool {
    let TabDrop {
        source,
        id,
        destination,
        slot,
    } = drop;
    let Some(index) = panes[source].tabs.iter().position(|tab| tab.id == id) else {
        return false;
    };
    let destination_pane = &panes[destination];
    let slot = clamp_tab_slot(
        slot,
        panes[source].tabs[index].pinned,
        destination_pane.pinned_count(),
        destination_pane.tabs.len(),
    );
    if source == destination && (slot == index || slot == index + 1) {
        return false;
    }
    panes[destination].tab_mut().address_completion.suspend();
    let tab = panes[source].remove_tab(index);
    let slot = slot.saturating_sub(usize::from(source == destination && index < slot));
    let pane = &mut panes[destination];
    let slot = slot.min(pane.tabs.len());
    pane.tabs.insert(slot, tab);
    pane.current = slot;
    if panes[source].tabs.is_empty() {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        panes[source].tabs.push(Tab::new(home.into(), ctx));
    }
    ctx.request_repaint();
    true
}
