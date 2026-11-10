use crate::conflict::{Answer, ConflictDialog, Decision};
use crate::drag_drop::{DragFiles, DropRequest, DropTargets};
use crate::file_clipboard::{FileList, SystemClipboard};
use crate::fs_ops::TransferKind;
use crate::icons::{self, Icon};
use crate::operations::{Progress, ProgressView};
use crate::preview::Preview;
use crate::tab_drag::{DragTab, TabDrop, TabDropTargets};
use crate::theme::{appearance_menu, Palette, Theme};
use crate::vfs::{self as fs_ops, Endpoint, FileEntry, Location};
use eframe::egui::{self, Color32, FontId, RichText, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    locations: Vec<Vec<Location>>,
    bookmarks: Vec<Location>,
    split: bool,
    preview: bool,
    hidden: bool,
    grid: bool,
    pane_grid: Option<[bool; 2]>,
    theme: Theme,
    language: crate::i18n::Language,
    sftp_profiles: Vec<Location>,
}
impl Settings {
    fn grid_for(&self, index: usize) -> bool {
        self.pane_grid.unwrap_or([self.grid; 2])[index]
    }
    fn set_grid(&mut self, index: usize, grid: bool) {
        self.pane_grid.get_or_insert([self.grid; 2])[index] = grid;
    }
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            locations: vec![],
            bookmarks: vec![],
            split: true,
            preview: true,
            hidden: false,
            grid: false,
            pane_grid: None,
            theme: Theme::Frappe,
            language: crate::i18n::Language::default(),
            sftp_profiles: vec![],
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Name,
    Size,
    Modified,
}
// Poll only displayed local directories. Waiting from completion also bounds
// work on slow volumes; no timer thread, native watcher or SFTP session is held.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

struct Tab {
    id: u64,
    path: Location,
    address: String,
    edit_address: bool,
    focus_address: bool,
    address_error: Option<String>,
    address_completion: crate::address::Completion,
    focus_filter: bool,
    filter: String,
    entries: Vec<FileEntry>,
    selected: BTreeSet<Location>,
    anchor: Option<Location>,
    cursor: Option<Location>,
    scroll_to: Option<Location>,
    reveal_after_load: Option<Location>,
    history: Vec<Location>,
    history_index: usize,
    loading: Option<Receiver<Result<Vec<FileEntry>, String>>>,
    automatic_loading: bool,
    next_auto_refresh: Instant,
    error: Option<String>,
    sort: Sort,
    descending: bool,
}
impl Tab {
    fn new(path: Location, ctx: &egui::Context) -> Self {
        static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(1);
        let mut tab = Self {
            id: NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed),
            address: path.display().to_string(),
            path: path.clone(),
            edit_address: false,
            focus_address: false,
            address_error: None,
            address_completion: Default::default(),
            focus_filter: false,
            filter: String::new(),
            entries: vec![],
            selected: BTreeSet::new(),
            anchor: None,
            cursor: None,
            scroll_to: None,
            reveal_after_load: None,
            history: vec![path],
            history_index: 0,
            loading: None,
            automatic_loading: false,
            next_auto_refresh: Instant::now() + AUTO_REFRESH_INTERVAL,
            error: None,
            sort: Sort::Name,
            descending: false,
        };
        tab.reload(ctx);
        tab
    }
    fn begin_address(&mut self) {
        self.address = self.path.display();
        self.edit_address = true;
        self.focus_address = true;
        self.address_error = None;
        self.address_completion.reset();
    }
    fn reload(&mut self, ctx: &egui::Context) {
        self.start_load(ctx, false);
    }
    fn start_load(&mut self, ctx: &egui::Context, automatic: bool) {
        let (tx, rx) = mpsc::channel();
        let path = self.path.clone();
        let ctx = ctx.clone();
        if !automatic {
            self.error = None;
        }
        self.automatic_loading = automatic;
        self.loading = Some(rx);
        // Replacing the receiver on navigation/manual refresh discards any old
        // directory result, including an automatic read finishing out of order.
        if let Err(error) = std::thread::Builder::new()
            .name("pawcommander-directory-read".into())
            .spawn(move || {
                let _ = tx.send(fs_ops::read_directory(&path));
                ctx.request_repaint();
            })
        {
            self.finish_load(Err(crate::tr_format!(
                "Не удалось запустить чтение папки: {error}",
                "Could not start reading folder: {error}"
            )));
        }
    }
    fn auto_refresh(&mut self, ctx: &egui::Context, now: Instant) {
        if self.path.remote() {
            return;
        }
        if self.loading.is_none() && now >= self.next_auto_refresh {
            self.start_load(ctx, true);
        }
        if self.loading.is_none() {
            ctx.request_repaint_after(self.next_auto_refresh.saturating_duration_since(now));
        }
    }
    fn blocking_load(&self) -> bool {
        self.loading.is_some() && !self.automatic_loading
    }
    fn finish_load(&mut self, result: Result<Vec<FileEntry>, String>) {
        match result {
            Ok(entries) => {
                let paths: BTreeSet<_> = entries.iter().map(|entry| &entry.path).collect();
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
                self.entries = entries;
                self.error = None;
                if let Some(path) = self.reveal_after_load.take() {
                    if self.entries.iter().any(|entry| entry.path == path) {
                        self.selected = BTreeSet::from([path.clone()]);
                        self.anchor = Some(path.clone());
                        self.cursor = Some(path.clone());
                        self.scroll_to = Some(path);
                    } else {
                        self.error = Some(
                            crate::i18n::text("Результат locate уже отсутствует в папке. Возможно, индекс устарел.")
                                .into(),
                        );
                    }
                }
            }
            Err(error) => {
                if self.automatic_loading {
                    self.error = Some(crate::tr_format!("Автообновление: {error}. Показан последний прочитанный список; повторим попытку автоматически.", "Auto refresh: {error}. Showing the last successful listing; will retry automatically."));
                } else {
                    self.error = Some(error);
                    self.entries.clear();
                    self.selected.clear();
                }
            }
        }
        self.loading = None;
        self.automatic_loading = false;
        self.next_auto_refresh = Instant::now() + AUTO_REFRESH_INTERVAL;
    }
    fn navigate(&mut self, path: Location, ctx: &egui::Context) {
        if path == self.path {
            self.reload(ctx);
            return;
        }
        self.path = path.clone();
        self.address = path.display().to_string();
        self.history.truncate(self.history_index + 1);
        self.history.push(path);
        self.history_index = self.history.len() - 1;
        self.reset_and_load(ctx);
    }
    fn reset_and_load(&mut self, ctx: &egui::Context) {
        self.entries.clear();
        self.selected.clear();
        self.anchor = None;
        self.cursor = None;
        self.scroll_to = None;
        self.reveal_after_load = None;
        self.filter.clear();
        self.edit_address = false;
        self.address_completion.reset();
        self.reload(ctx);
    }
    fn history_go(&mut self, delta: isize, ctx: &egui::Context) {
        let index = self.history_index as isize + delta;
        if index >= 0 && (index as usize) < self.history.len() {
            self.history_index = index as usize;
            self.path = self.history[self.history_index].clone();
            self.address = self.path.display().to_string();
            self.reset_and_load(ctx);
        }
    }
    fn poll(&mut self) {
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(result) => {
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
    }
    fn visible(&self, hidden: bool) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        let mut indices: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                (hidden || !e.name.starts_with('.')) && e.name.to_lowercase().contains(&filter)
            })
            .map(|(i, _)| i)
            .collect();
        indices.sort_by(|a, b| {
            let a = &self.entries[*a];
            let b = &self.entries[*b];
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let order = match self.sort {
                    Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    Sort::Size => a.size.cmp(&b.size),
                    Sort::Modified => a.modified.cmp(&b.modified),
                }
                .then_with(|| a.name.cmp(&b.name));
                if self.descending {
                    order.reverse()
                } else {
                    order
                }
            })
        });
        indices
    }
    fn choose(&mut self, index: usize, visible: &[usize], modifiers: egui::Modifiers) {
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

    fn selected_visible(&self, hidden: bool) -> Vec<Location> {
        self.visible(hidden)
            .into_iter()
            .map(|i| &self.entries[i].path)
            .filter(|path| self.selected.contains(*path))
            .cloned()
            .collect()
    }

    fn drag_paths(&mut self, index: usize, visible: &[usize]) -> Vec<Location> {
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

    fn move_selection(&mut self, delta: isize, hidden: bool, shift: bool) {
        let visible = self.visible(hidden);
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
struct Pane {
    tabs: Vec<Tab>,
    current: usize,
}
impl Pane {
    fn remove_tab(&mut self, index: usize) -> Tab {
        let tab = self.tabs.remove(index);
        if index < self.current {
            self.current -= 1;
        }
        self.current = self.current.min(self.tabs.len().saturating_sub(1));
        tab
    }
    fn tab(&self) -> &Tab {
        &self.tabs[self.current]
    }
    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.current]
    }
}
fn move_tab(panes: &mut [Pane], drop: TabDrop, ctx: &egui::Context) -> bool {
    let TabDrop {
        source,
        id,
        destination,
        slot,
    } = drop;
    let Some(index) = panes[source].tabs.iter().position(|tab| tab.id == id) else {
        return false;
    };
    if source == destination && (slot == index || slot == index + 1) {
        return false;
    }
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

#[derive(Clone)]
enum Action {
    Focus(usize),
    SetGrid(usize, bool),
    Navigate(usize, Location),
    Open(usize, Location),
    OpenInZed(usize, Location),
    Back,
    Forward,
    History(usize, isize),
    Up,
    Refresh,
    NewTab(usize),
    CloseTab(usize, usize),
    Copy(bool),
    Paste(bool),
    NewFolder,
    Rename,
    Trash,
    Transfer(TransferKind),
    Bookmark,
    Reveal,
    CopyPath,
    ConnectSftp,
    Properties,
}
struct Clipboard {
    paths: Vec<Location>,
    cut: bool,
    generation: u64,
    origin: ClipboardOrigin,
}

enum ClipboardOrigin {
    // Remember which system selection preceded the app-only copy. A newly copied system file
    // takes precedence, while an unrelated or empty text clipboard leaves SFTP copies usable.
    App(Option<FileList>),
    System(FileList),
}

impl Clipboard {
    fn new(paths: Vec<Location>, cut: bool, origin: ClipboardOrigin) -> Self {
        static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
        Self {
            paths,
            cut,
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            origin,
        }
    }

    fn matches(&self, observed: Option<&FileList>) -> bool {
        match &self.origin {
            ClipboardOrigin::App(previous) => observed.is_none() || previous.as_ref() == observed,
            ClipboardOrigin::System(previous) => Some(previous) == observed,
        }
    }
}

fn file_clipboard_action(event: &egui::Event) -> Option<Action> {
    match event {
        egui::Event::Copy => Some(Action::Copy(false)),
        egui::Event::Cut => Some(Action::Copy(true)),
        // egui-winit intercepts the press and emits Event::Paste only if the OS buffer
        // holds text. The release also arrives for a buffer containing just file URLs.
        egui::Event::Key {
            key: egui::Key::V,
            modifiers,
            pressed: false,
            ..
        } if modifiers.command
            && !modifiers.shift
            && (!modifiers.alt || cfg!(target_os = "macos")) =>
        {
            Some(Action::Paste(modifiers.alt))
        }
        _ => None,
    }
}
enum Dialog {
    NewFolder { parent: Location, name: String },
    Rename { source: Location, name: String },
    Trash { paths: Vec<Location> },
}
enum TransferEvent {
    Conflict(Box<ConflictDialog>),
    Moved(Location),
}
struct TransferInteraction {
    events: Receiver<TransferEvent>,
    cut_generation: Option<u64>,
    conflict: Option<Box<ConflictDialog>>,
}
struct Job {
    label: String,
    started: Instant,
    result: Receiver<Result<String, String>>,
    progress: ProgressView,
}

#[derive(Clone)]
struct SftpForm {
    host: String,
    port: u16,
    user: String,
    path: String,
    key: String,
    password: String,
    passphrase: String,
    use_saved: bool,
    save_key: bool,
    error: Option<String>,
}
impl Default for SftpForm {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            path: ".".into(),
            key: String::new(),
            password: String::new(),
            passphrase: String::new(),
            use_saved: true,
            save_key: false,
            error: None,
        }
    }
}
struct ConnectionJob {
    pane: usize,
    save_profile: bool,
    result: Receiver<Result<Location, String>>,
}

pub struct PawCommander {
    panes: Vec<Pane>,
    active: usize,
    settings: Settings,
    preview: Preview,
    properties: Option<crate::properties::Properties>,
    clipboard: Option<Clipboard>,
    system_clipboard: SystemClipboard,
    dialog: Option<Dialog>,
    dialog_focus: bool,
    job: Option<Job>,
    transfer: Option<TransferInteraction>,
    notice: Option<(String, bool)>,
    show_help: bool,
    sftp_form: Option<SftpForm>,
    connection: Option<ConnectionJob>,
    native_drag: crate::native_drag::NativeDrag,
    editor_job: Option<Receiver<Result<String, String>>>,
    tools: crate::tools::Tools,
    search: crate::search::Search,
}
impl PawCommander {
    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<PathBuf>) -> Self {
        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, "pawcommander.settings"))
            .unwrap_or_default();
        crate::i18n::set_language(settings.language);
        settings.theme.apply(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        if let Some(start) = start {
            let path = if start.is_absolute() {
                start
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| home.clone())
                    .join(start)
            };
            settings.locations = vec![vec![path.clone().into()], vec![path.into()]];
        }
        let panes = (0..2)
            .map(|i| {
                let locations = settings
                    .locations
                    .get(i)
                    .cloned()
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| vec![home.clone().into()]);
                Pane {
                    tabs: locations
                        .into_iter()
                        .map(|p| Tab::new(p, &cc.egui_ctx))
                        .collect(),
                    current: 0,
                }
            })
            .collect();
        Self {
            panes,
            active: 0,
            settings,
            preview: Preview::new(),
            properties: None,
            clipboard: None,
            system_clipboard: SystemClipboard::new(cc),
            dialog: None,
            dialog_focus: false,
            job: None,
            transfer: None,
            notice: None,
            show_help: false,
            sftp_form: None,
            connection: None,
            native_drag: Default::default(),
            editor_job: None,
            tools: crate::tools::Tools::detect(),
            search: Default::default(),
        }
    }
    fn tab(&self) -> &Tab {
        self.panes[self.active].tab()
    }
    fn tab_mut(&mut self) -> &mut Tab {
        self.panes[self.active].tab_mut()
    }
    fn selected(&self) -> Vec<Location> {
        // Actions can run in the same frame as a visibility toggle, before the
        // pane has rendered and removed its now-hidden selection.
        self.tab().selected_visible(self.settings.hidden)
    }
    fn refresh(&mut self, ctx: &egui::Context) {
        self.preview.refresh();
        for pane in &mut self.panes {
            for tab in &mut pane.tabs {
                tab.reload(ctx);
            }
        }
    }
    fn start_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        work: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) {
        self.start_progress_job(ctx, label, |_| work());
    }
    fn start_progress_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        work: impl FnOnce(&mut Progress) -> Result<String, String> + Send + 'static,
    ) {
        if self.job.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let worker_ctx = ctx.clone();
        let (mut reporter, progress) = Progress::tracked();
        let started = Instant::now();
        if let Err(error) = std::thread::Builder::new()
            .name("pawcommander-file-operation".into())
            .spawn(move || {
                let _ = tx.send(work(&mut reporter));
                worker_ctx.request_repaint();
            })
        {
            self.notice = Some((
                crate::tr_format!(
                    "Не удалось запустить операцию: {error}",
                    "Could not start operation: {error}"
                ),
                true,
            ));
            return;
        }
        self.job = Some(Job {
            label: label.into(),
            started,
            result: rx,
            progress,
        });
        self.notice = None;
        ctx.request_repaint();
    }
    fn drop_enabled(&self) -> bool {
        self.job.is_none()
            && self.transfer.is_none()
            && self.dialog.is_none()
            && self.properties.is_none()
            && self.sftp_form.is_none()
            && !self.show_help
            && !self.search.visible
    }

    fn start_transfer(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        sources: Vec<Location>,
        destination: Location,
        kind: TransferKind,
        from_cut_clipboard: bool,
    ) {
        if sources.is_empty() || self.job.is_some() || self.transfer.is_some() {
            return;
        }
        let cut_generation = if from_cut_clipboard {
            self.clipboard
                .as_ref()
                .map(|clipboard| clipboard.generation)
        } else {
            None
        };
        let (events, rx) = mpsc::channel();
        let worker_ctx = ctx.clone();
        self.start_progress_job(ctx, label, move |progress| {
            let summary = fs_ops::transfer_with_conflicts(
                &sources,
                &destination,
                kind,
                progress,
                |conflict| {
                    let (reply, answer) = mpsc::channel();
                    let dialog = ConflictDialog::new(conflict, reply);
                    events
                        .send(TransferEvent::Conflict(Box::new(dialog)))
                        .map_err(|_| crate::i18n::text("Диалог конфликта закрыт"))?;
                    worker_ctx.request_repaint();
                    Ok(answer.recv().unwrap_or(Answer {
                        decision: Decision::Cancel,
                        apply_to_group: false,
                    }))
                },
                |location| {
                    if cut_generation.is_some() {
                        let _ = events.send(TransferEvent::Moved(location.clone()));
                        worker_ctx.request_repaint();
                    }
                },
            )?;
            if summary.cancelled {
                return Ok(crate::i18n::text("Операция отменена до переноса файлов").into());
            }
            let (completed, skipped) = (summary.completed, summary.skipped);
            Ok(crate::tr_format!(
                "Готово: {completed}. Пропущено: {skipped}. {}",
                "Completed: {completed}. Skipped: {skipped}. {}",
                destination.display()
            ))
        });
        if self.job.is_some() {
            self.transfer = Some(TransferInteraction {
                events: rx,
                cut_generation,
                conflict: None,
            });
        }
    }

    fn poll_transfer(&mut self) {
        while let Some(transfer) = &mut self.transfer {
            match transfer.events.try_recv() {
                Ok(TransferEvent::Conflict(dialog)) => transfer.conflict = Some(dialog),
                Ok(TransferEvent::Moved(location)) => {
                    if let Some(generation) = transfer.cut_generation {
                        self.consume_cut_source(&location, generation);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.transfer = None,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
    }

    fn conflict_open(&self) -> bool {
        self.transfer
            .as_ref()
            .is_some_and(|transfer| transfer.conflict.is_some())
    }

    fn drop_files(&mut self, request: DropRequest, ctx: &egui::Context) {
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
    fn open_in_zed(&mut self, path: Location, text_only: bool, ctx: &egui::Context) {
        if self.editor_job.is_some() {
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

    fn poll(&mut self, ctx: &egui::Context) {
        self.poll_transfer();
        if let Some(rx) = &self.editor_job {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text(
                    "Задача открытия в Zed завершилась неожиданно",
                )
                .into())),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.notice = Some(match result {
                    Ok(message) => (message, false),
                    Err(error) => (error, true),
                });
                self.editor_job = None;
            }
        }
        if let Some(job) = &self.connection {
            let result = match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text(
                    "Задача подключения завершилась",
                )
                .into())),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                match result {
                    Ok(location) => {
                        if job.save_profile {
                            self.settings
                                .sftp_profiles
                                .retain(|p| match (p, &location) {
                                    (
                                        Location::Sftp { endpoint: a, .. },
                                        Location::Sftp { endpoint: b, .. },
                                    ) => a != b,
                                    _ => true,
                                });
                            self.settings.sftp_profiles.push(location.clone());
                        }
                        self.panes[job.pane].tab_mut().navigate(location, ctx);
                        self.active = job.pane;
                        self.sftp_form = None;
                    }
                    Err(error) => {
                        if let Some(form) = &mut self.sftp_form {
                            form.error = Some(error);
                        }
                    }
                }
                self.connection = None;
            }
        }
        // Avoid reordering rows under a held pointer or during a transfer.
        let automatic_allowed = self.job.is_none()
            && self.transfer.is_none()
            && !self.native_drag.active()
            && !ctx.input(|input| input.pointer.any_down() || input.pointer.any_released())
            && !egui::DragAndDrop::has_any_payload(ctx);
        let now = Instant::now();
        for (pane_index, pane) in self.panes.iter_mut().enumerate() {
            let displayed = self.settings.split || pane_index == 0;
            for (tab_index, tab) in pane.tabs.iter_mut().enumerate() {
                if !tab.automatic_loading || automatic_allowed {
                    tab.poll();
                }
                if displayed && tab_index == pane.current && !tab.path.remote() {
                    if automatic_allowed {
                        tab.auto_refresh(ctx, now);
                    } else {
                        ctx.request_repaint_after(AUTO_REFRESH_INTERVAL);
                    }
                }
            }
        }
        if let Some(job) = &self.job {
            let result = match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::text(
                    "Фоновая операция неожиданно завершилась. Проверьте файлы в обеих папках.",
                )
                .into())),
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.notice = Some(match result {
                    Ok(s) => (s, false),
                    Err(s) => (s, true),
                });
                self.job = None;
                self.refresh(ctx);
            }
        }
    }
    fn copy_files(&mut self, cut: bool) {
        let paths = self.selected();
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let local_paths: Option<Vec<_>> = paths
            .iter()
            .map(|path| path.local().map(Path::to_path_buf))
            .collect();
        let write_error = if let Some(local_paths) = local_paths {
            match self.system_clipboard.write(&local_paths, cut) {
                Ok(()) => {
                    self.clipboard = None;
                    self.notice = Some((
                        crate::tr_format!(
                            "{} объектов в системном файловом буфере",
                            "{} items in the system file clipboard",
                            count
                        ),
                        false,
                    ));
                    return;
                }
                Err(error) => Some(error),
            }
        } else {
            None
        };
        let observed = self.system_clipboard.read().ok().flatten();
        self.clipboard = Some(Clipboard::new(paths, cut, ClipboardOrigin::App(observed)));
        self.notice = Some(match write_error {
            Some(error) => (
                crate::tr_format!(
                    "Системный буфер недоступен: {error}. Используется буфер PawCommander.",
                    "System clipboard unavailable: {error}. Using the PawCommander clipboard."
                ),
                true,
            ),
            None => (
                crate::tr_format!(
                    "{} объектов в буфере PawCommander (SFTP)",
                    "{} items in the PawCommander clipboard (SFTP)",
                    count
                ),
                false,
            ),
        });
    }

    fn paste_files(&mut self, move_here: bool, ctx: &egui::Context) {
        let observed = match self.system_clipboard.read() {
            Ok(files) => files,
            Err(_)
                if self.clipboard.as_ref().is_some_and(|clipboard| {
                    matches!(clipboard.origin, ClipboardOrigin::App(_))
                }) =>
            {
                None
            }
            Err(error) => {
                self.notice = Some((
                    crate::tr_format!(
                        "Не удалось прочитать системный файловый буфер: {error}",
                        "Could not read system file clipboard: {error}"
                    ),
                    true,
                ));
                return;
            }
        };
        if !self
            .clipboard
            .as_ref()
            .is_some_and(|clipboard| clipboard.matches(observed.as_ref()))
        {
            self.clipboard = observed.map(|files| {
                let paths = files.paths.iter().cloned().map(Location::from).collect();
                Clipboard::new(paths, files.cut, ClipboardOrigin::System(files))
            });
        }
        let Some(clipboard) = self
            .clipboard
            .as_ref()
            .filter(|clipboard| !clipboard.paths.is_empty())
        else {
            self.notice = Some((
                crate::i18n::text("В файловом буфере нет объектов для вставки").into(),
                false,
            ));
            return;
        };
        let cut = clipboard.cut || move_here;
        let kind = if cut {
            TransferKind::Move
        } else {
            TransferKind::Copy
        };
        let label = if cut {
            crate::i18n::text("Перемещение файлов")
        } else {
            crate::i18n::text("Копирование файлов")
        };
        self.start_transfer(
            ctx,
            label,
            clipboard.paths.clone(),
            self.tab().path.clone(),
            kind,
            cut,
        );
    }

    fn consume_cut_source(&mut self, source: &Location, generation: u64) {
        if let Some(clipboard) = self
            .clipboard
            .as_mut()
            .filter(|clipboard| clipboard.generation == generation)
        {
            clipboard.paths.retain(|path| {
                !path.same_backend(source) || !path.path().starts_with(source.path())
            });
        }
    }

    fn action(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::SetGrid(index, grid) => {
                self.settings.set_grid(index, grid);
                self.active = index;
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
            Action::OpenInZed(i, path) => {
                self.active = i;
                self.open_in_zed(path, false, ctx);
            }
            Action::Open(i, p) => {
                self.active = i;
                if self.panes[i]
                    .tab()
                    .entries
                    .iter()
                    .any(|e| e.path == p && e.is_dir)
                {
                    self.panes[i].tab_mut().navigate(p, ctx);
                } else if p.remote() {
                    self.open_in_zed(p, true, ctx);
                } else if let Err(e) = open::that_detached(p.path()) {
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
                self.panes[index].tab_mut().history_go(delta, ctx);
            }
            Action::Back => self.tab_mut().history_go(-1, ctx),
            Action::Forward => self.tab_mut().history_go(1, ctx),
            Action::Up => {
                if let Some(parent) = self.tab().path.parent() {
                    self.tab_mut().navigate(parent, ctx);
                }
            }
            Action::Properties => {
                let selected = self.selected();
                if self.properties.is_none()
                    && !selected.is_empty()
                    && selected.iter().all(|path| !path.remote())
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
            Action::Refresh => self.refresh(ctx),
            Action::NewTab(i) => {
                let path = self.panes[i].tab().path.clone();
                self.panes[i].tabs.push(Tab::new(path, ctx));
                self.panes[i].current = self.panes[i].tabs.len() - 1;
                self.active = i;
            }
            Action::CloseTab(i, t) => {
                if self.panes[i].tabs.len() > 1 {
                    self.panes[i].remove_tab(t);
                }
            }
            Action::Copy(cut) => self.copy_files(cut),
            Action::Paste(move_here) if self.drop_enabled() => self.paste_files(move_here, ctx),
            Action::Transfer(kind) if self.job.is_none() && self.settings.split => {
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
            Action::NewFolder if self.job.is_none() => {
                self.dialog = Some(Dialog::NewFolder {
                    parent: self.tab().path.clone(),
                    name: String::new(),
                });
                self.dialog_focus = true;
            }
            Action::Rename if self.job.is_none() => {
                let selected = self.selected();
                if selected.len() == 1 {
                    let source = selected[0].clone();
                    let name = file_name(&source);
                    self.dialog = Some(Dialog::Rename { source, name });
                    self.dialog_focus = true;
                }
            }
            Action::Trash if self.job.is_none() => {
                let paths = self.selected();
                if !paths.is_empty() {
                    self.dialog = Some(Dialog::Trash { paths });
                }
            }
            Action::Bookmark => {
                let path = self.tab().path.clone();
                if !self.settings.bookmarks.contains(&path) {
                    self.settings.bookmarks.push(path);
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
    fn open_search(&mut self) {
        self.tools = crate::tools::Tools::detect();
        self.search.open();
    }

    fn reveal_search_result(&mut self, path: PathBuf, ctx: &egui::Context) {
        let Some(parent) = path.parent() else {
            return;
        };
        // A result can be hidden. Make it visible before selecting it.
        if path
            .components()
            .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
        {
            self.settings.hidden = true;
        }
        let tab = self.tab_mut();
        tab.navigate(parent.to_path_buf().into(), ctx);
        tab.filter.clear();
        tab.selected.clear();
        tab.reveal_after_load = Some(path.into());
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if egui::DragAndDrop::has_payload_of_type::<DragTab>(ctx)
            || self.dialog.is_some()
            || self.properties.is_some()
            || self.conflict_open()
            || self.sftp_form.is_some()
            || self.show_help
            || self.native_drag.active()
            || self.search.visible
        {
            return;
        }
        let command = egui::Modifiers::COMMAND;
        if ctx.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                command | egui::Modifiers::SHIFT,
                egui::Key::F,
            ))
        }) {
            self.open_search();
            return;
        }
        if ctx
            .input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(command, egui::Key::L)))
        {
            let tab = self.tab_mut();
            tab.begin_address();
            return;
        }
        if ctx
            .input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(command, egui::Key::F)))
        {
            self.tab_mut().focus_filter = true;
            return;
        }
        if ctx.wants_keyboard_input() {
            return;
        }
        let mut actions = vec![];
        ctx.input_mut(|i| {
            i.events.retain(|event| {
                if let Some(action) = file_clipboard_action(event) {
                    actions.push(action);
                    false
                } else {
                    true
                }
            });
            for (key, action) in [
                (egui::Key::T, Action::NewTab(self.active)),
                (
                    egui::Key::W,
                    Action::CloseTab(self.active, self.panes[self.active].current),
                ),
            ] {
                if i.consume_shortcut(&egui::KeyboardShortcut::new(command, key)) {
                    actions.push(action);
                }
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(command, egui::Key::A)) {
                let hidden = self.settings.hidden;
                let tab = self.tab_mut();
                tab.selected = tab
                    .visible(hidden)
                    .iter()
                    .map(|i| tab.entries[*i].path.clone())
                    .collect();
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(command, egui::Key::H)) {
                self.settings.hidden = !self.settings.hidden;
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                command | egui::Modifiers::SHIFT,
                egui::Key::N,
            )) {
                actions.push(Action::NewFolder);
            }
            for (key, action) in [
                (egui::Key::ArrowLeft, Action::Back),
                (egui::Key::ArrowRight, Action::Forward),
                (egui::Key::ArrowUp, Action::Up),
            ] {
                if i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::ALT, key)) {
                    actions.push(action);
                }
            }
            if !i.modifiers.alt && !i.modifiers.command && !i.modifiers.ctrl {
                let modifiers = if i.modifiers.shift {
                    egui::Modifiers::SHIFT
                } else {
                    egui::Modifiers::NONE
                };
                for (key, delta) in [(egui::Key::ArrowUp, -1), (egui::Key::ArrowDown, 1)] {
                    if i.consume_key(modifiers, key) {
                        let hidden = self.settings.hidden;
                        self.tab_mut()
                            .move_selection(delta, hidden, modifiers.shift);
                    }
                }
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::ALT,
                egui::Key::Enter,
            )) {
                actions.push(Action::Properties);
            }
            if i.key_pressed(egui::Key::F2) {
                actions.push(Action::Rename);
            }
            if i.key_pressed(egui::Key::F3) {
                self.settings.split = !self.settings.split;
                if !self.settings.split {
                    self.active = 0;
                }
            }
            if i.key_pressed(egui::Key::F5) {
                actions.push(Action::Refresh);
            }
            if i.key_pressed(egui::Key::Delete)
                || (i.modifiers.command && i.key_pressed(egui::Key::Backspace))
            {
                actions.push(Action::Trash);
            }
            if !i.modifiers.alt && i.key_pressed(egui::Key::Enter) {
                if let Some(p) = self.selected().first() {
                    actions.push(Action::Open(self.active, p.clone()));
                }
            }
            if i.key_pressed(egui::Key::Escape) {
                self.tab_mut().selected.clear();
                self.tab_mut().cursor = None;
                self.tab_mut().anchor = None;
                self.tab_mut().filter.clear();
            }
            if i.key_pressed(egui::Key::Tab) && self.settings.split {
                self.active = 1 - self.active;
            }
        });
        for action in actions {
            self.action(action, ctx);
        }
    }
    fn toolbar(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let colors = Palette::from_ctx(ctx);
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::new().fill(colors.surface).inner_margin(egui::Margin::symmetric(12, 4)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    if cfg!(target_os = "macos") {
                        // Keep the native close, minimize and zoom controls clear.
                        ui.add_space((88.0 / ctx.zoom_factor() - 12.0).max(0.0));
                    }
                    let tab = self.tab();
                    for (icon, tip, enabled, action) in [
                        (Icon::Back, crate::i18n::text("Назад · Alt+←"), tab.history_index > 0, Action::Back),
                        (Icon::Forward, crate::i18n::text("Вперёд · Alt+→"), tab.history_index + 1 < tab.history.len(), Action::Forward),
                        (Icon::Up, crate::i18n::text("Родительская папка · Alt+↑"), tab.path.parent().is_some(), Action::Up),
                        (Icon::Refresh, crate::i18n::text("Обновить · F5"), true, Action::Refresh),
                    ] {
                        if ui.add_enabled_ui(enabled, |ui| icons::button(ui, icon, "", false)).inner.on_hover_text(tip).clicked() {
                            actions.push(action);
                        }
                    }
                    ui.separator();
                    let search_width = (ui.available_width() - 395.0).clamp(icons::TOOLBAR_BUTTON_SIZE, 420.0);
                    let search_label = if search_width >= 150.0 {
                        crate::i18n::text("Поиск файлов…")
                    } else if search_width >= 95.0 {
                        crate::i18n::text("Поиск…")
                    } else {
                        ""
                    };
                    let search_enabled = self.dialog.is_none() && self.sftp_form.is_none() && !self.show_help && !self.native_drag.active();
                    if ui.add_enabled_ui(search_enabled, |ui| icons::button_width(ui, Icon::Search, search_label, false, search_width)).inner
                        .on_hover_text(crate::i18n::text("Суперпоиск по индексу locate · ⌘/Ctrl+Shift+F")).clicked() {
                        self.open_search();
                    }
                    if ui.add_enabled_ui(self.job.is_none(), |ui| icons::button(ui, Icon::Server, "SFTP", self.tab().path.remote())).inner
                        .on_hover_text(crate::i18n::text("Подключиться по SFTP")).clicked() {
                        actions.push(Action::ConnectSftp);
                    }
                    for (icon, tip, enabled, action) in [
                        (Icon::NewFolder, crate::i18n::text("Создать папку · ⌘/Ctrl+Shift+N"), self.job.is_none(), Action::NewFolder),
                        (Icon::Copy, crate::i18n::text("Копировать · ⌘/Ctrl+C"), !self.tab().selected.is_empty(), Action::Copy(false)),
                        (Icon::Paste, crate::i18n::text("Вставить · ⌘/Ctrl+V"), self.job.is_none(), Action::Paste(false)),
                    ] {
                        if ui.add_enabled_ui(enabled, |ui| icons::button(ui, icon, "", false)).inner.on_hover_text(tip).clicked() {
                            actions.push(action);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let menu_id = ui.id().with("settings_menu");
                        let button = icons::button(ui, Icon::Settings, crate::i18n::text("Настройки"), false);
                        let mut menu = egui::menu::BarState::load(ctx, menu_id);
                        menu.bar_menu(&button, |ui| {
                            if ui.button(crate::i18n::text("О PawCommander / клавиши")).clicked() { self.show_help = true; ui.close_menu(); }
                            ui.menu_button("Язык / Language", |ui| {
                                for (language, label) in [
                                    (crate::i18n::Language::English, "English"),
                                    (crate::i18n::Language::Russian, "Русский"),
                                ] {
                                    if ui.selectable_value(&mut self.settings.language, language, label).changed() {
                                        crate::i18n::set_language(language);
                                        self.preview.refresh();
                                        ctx.request_repaint();
                                    }
                                }
                            });
                            ui.separator();
                            appearance_menu(ui, &mut self.settings.theme);
                            ui.separator();
                            ui.checkbox(&mut self.settings.hidden, crate::i18n::text("Скрытые файлы"));
                            ui.menu_button(crate::i18n::text("Инструменты"), |ui| {
                                for (name, path) in [("locate", &self.tools.locate), ("Zed", &self.tools.zed)] {
                                    if let Some(path) = path {
                                        ui.label(crate::tr_format!("{name}: найден", "{name}: found")).on_hover_text(path.display().to_string());
                                    } else {
                                        ui.colored_label(ui.visuals().warn_fg_color, crate::tr_format!("{name}: не найден", "{name}: not found"));
                                    }
                                }
                                ui.small(crate::i18n::text("locate: установите locate/plocate и создайте индекс средствами ОС."));
                                ui.small(crate::i18n::text("Zed: установите редактор; для нестандартного пути — cli: install cli binary."));
                                if ui.button(crate::i18n::text("Проверить снова")).clicked() { self.tools = crate::tools::Tools::detect(); }
                            });
                        });
                        menu.store(ctx, menu_id);
                        if icons::button(ui, Icon::Info, "", self.settings.preview).on_hover_text(crate::i18n::text("Панель сведений")).clicked() {
                            self.settings.preview = !self.settings.preview;
                        }
                        if icons::button(ui, Icon::Split, "", self.settings.split).on_hover_text(crate::i18n::text("Две панели · F3")).clicked() {
                            self.settings.split = !self.settings.split;
                            if !self.settings.split { self.active = 0; }
                        }
                        if cfg!(target_os = "macos") && ui.available_width() > 0.0 {
                            let (_, drag) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), icons::TOOLBAR_BUTTON_SIZE),
                                Sense::drag(),
                            );
                            if drag.drag_started_by(egui::PointerButton::Primary) {
                                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                            }
                        }
                    });
                });
            });
    }
    fn sidebar(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let colors = Palette::from_ctx(ctx);
        egui::SidePanel::left("places")
            .exact_width(204.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(colors.surface))
            .show(ctx, |panel_ui| {
                let mut card = workspace_card(colors)
                    .inner_margin(egui::Margin::symmetric(12, 14))
                    .outer_margin(egui::Margin {
                        left: 8,
                        right: 0,
                        top: 8,
                        bottom: 8,
                    })
                    .begin(panel_ui);
                let ui = &mut card.content_ui;
                ui.set_min_size(ui.available_size());
                let footer_height = 66.0;
                let places_height = (ui.available_height() - footer_height).max(0.0);
                egui::ScrollArea::vertical()
                    .id_salt("places_scroll")
                    .max_height(places_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let current = self.tab().path.clone();
                        egui::CollapsingHeader::new(crate::i18n::text("Ваши файлы"))
                            .id_salt("places_group")
                            .default_open(true)
                            .show_unindented(ui, |ui| {
                                let places = [
                                    (crate::i18n::text("Домашняя"), dirs::home_dir()),
                                    (crate::i18n::text("Рабочий стол"), dirs::desktop_dir()),
                                    (crate::i18n::text("Документы"), dirs::document_dir()),
                                    (crate::i18n::text("Загрузки"), dirs::download_dir()),
                                    (crate::i18n::text("Изображения"), dirs::picture_dir()),
                                    (crate::i18n::text("Музыка"), dirs::audio_dir()),
                                    (crate::i18n::text("Видео"), dirs::video_dir()),
                                ];
                                for (label, path) in places {
                                    if let Some(path) = path {
                                        if place_button(
                                            ui,
                                            label,
                                            current.local() == Some(path.as_path()),
                                        ) {
                                            actions
                                                .push(Action::Navigate(self.active, path.into()));
                                        }
                                    }
                                }
                            });
                        ui.add_space(6.0);
                        egui::CollapsingHeader::new(crate::i18n::text("Подключения"))
                            .id_salt("remote_group")
                            .default_open(true)
                            .show_unindented(ui, |ui| {
                                if place_button(
                                    ui,
                                    crate::i18n::text("SFTP · Подключиться"),
                                    current.remote(),
                                ) {
                                    actions.push(Action::ConnectSftp);
                                }
                                for profile in &self.settings.sftp_profiles {
                                    if place_button(ui, &profile.name(), current == *profile) {
                                        actions
                                            .push(Action::Navigate(self.active, profile.clone()));
                                    }
                                }
                            });
                        ui.add_space(6.0);
                        egui::CollapsingHeader::new(crate::i18n::text("Избранное"))
                            .id_salt("bookmarks_group")
                            .default_open(true)
                            .show_unindented(ui, |ui| {
                                let mut remove = None;
                                for (i, path) in self.settings.bookmarks.iter().enumerate() {
                                    let response = ui
                                        .add_sized(
                                            [ui.available_width(), 29.0],
                                            egui::Button::new(format!("      {}", file_name(path)))
                                                .selected(current == *path),
                                        )
                                        .on_hover_text(path.display().to_string());
                                    icons::paint(
                                        ui,
                                        egui::Rect::from_center_size(
                                            response.rect.left_center() + Vec2::new(16.0, 0.0),
                                            Vec2::splat(18.0),
                                        ),
                                        Icon::Star,
                                        colors.yellow,
                                    );
                                    if response.clicked() {
                                        actions.push(Action::Navigate(self.active, path.clone()));
                                    }
                                    response.context_menu(|ui| {
                                        if ui
                                            .button(crate::i18n::text("Убрать из избранного"))
                                            .clicked()
                                        {
                                            remove = Some(i);
                                            ui.close_menu();
                                        }
                                    });
                                }
                                if let Some(i) = remove {
                                    self.settings.bookmarks.remove(i);
                                }
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(crate::i18n::text("+ Текущая папка"))
                                                .color(colors.muted),
                                        )
                                        .frame(false),
                                    )
                                    .clicked()
                                {
                                    actions.push(Action::Bookmark);
                                }
                            });
                        ui.add_space(6.0);
                        egui::CollapsingHeader::new(crate::i18n::text("Устройства"))
                            .id_salt("devices_group")
                            .default_open(true)
                            .show_unindented(ui, |ui| {
                                #[cfg(not(target_os = "windows"))]
                                if place_button(
                                    ui,
                                    crate::i18n::text("Файловая система"),
                                    current.local() == Some(Path::new("/")),
                                ) {
                                    actions.push(Action::Navigate(
                                        self.active,
                                        PathBuf::from("/").into(),
                                    ));
                                }
                                #[cfg(target_os = "macos")]
                                if place_button(
                                    ui,
                                    crate::i18n::text("Тома и диски"),
                                    current.local() == Some(Path::new("/Volumes")),
                                ) {
                                    actions.push(Action::Navigate(
                                        self.active,
                                        PathBuf::from("/Volumes").into(),
                                    ));
                                }
                                #[cfg(target_os = "linux")]
                                if place_button(
                                    ui,
                                    crate::i18n::text("Носители"),
                                    current.local() == Some(Path::new("/media")),
                                ) {
                                    actions.push(Action::Navigate(
                                        self.active,
                                        PathBuf::from("/media").into(),
                                    ));
                                }
                                #[cfg(target_os = "windows")]
                                for drive in b'A'..=b'Z' {
                                    let path = PathBuf::from(format!("{}:\\", drive as char));
                                    if path.exists()
                                        && place_button(
                                            ui,
                                            &crate::tr_format!(
                                                "Диск {}:",
                                                "Drive {}:",
                                                drive as char
                                            ),
                                            current.local() == Some(path.as_path()),
                                        )
                                    {
                                        actions.push(Action::Navigate(self.active, path.into()));
                                    }
                                }
                            });
                    });
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new("PawCommander  /  0.1")
                            .size(10.0)
                            .color(colors.muted),
                    );
                    ui.add_space(10.0);
                    ui.checkbox(
                        &mut self.settings.hidden,
                        crate::i18n::text("Скрытые файлы"),
                    )
                    .on_hover_text("⌘/Ctrl+H");
                });
                card.end(panel_ui);
            });
    }
    fn footer(&mut self, ctx: &egui::Context) {
        let colors = Palette::from_ctx(ctx);
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(colors.bg)
                    .inner_margin(egui::Margin::symmetric(18, 10)),
            )
            .show(ctx, |ui| {
                if let Some(job) = &mut self.job {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(crate::tr_format!(
                            "Файловые операции: {} · {} с",
                            "File operations: {} · {} s",
                            job.label,
                            job.started.elapsed().as_secs()
                        ));
                    });
                    operation_details(ui, job.progress.snapshot());
                    if let Some((message, _)) = &self.notice {
                        ui.label(RichText::new(message).color(colors.muted));
                    }
                } else if let Some((message, error)) = self.notice.clone() {
                    ui.horizontal_wrapped(|ui| {
                        if ui.small_button("×").clicked() {
                            self.notice = None;
                        }
                        ui.colored_label(
                            if error { colors.error } else { colors.success },
                            message,
                        );
                    });
                } else {
                    ui.horizontal(|ui| {
                        let visible = self.tab().visible(self.settings.hidden).len();
                        ui.label(
                            RichText::new(crate::tr_format!(
                                "{} объектов  ·  Выбрано: {}",
                                "{} items  ·  Selected: {}",
                                visible,
                                self.tab().selected.len()
                            ))
                            .color(colors.muted),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(crate::i18n::text(
                                    "Двойной щелчок — открыть   ·   F2 — переименовать",
                                ))
                                .size(11.0)
                                .color(colors.muted),
                            );
                        });
                    });
                }
            });
    }
    fn sftp_dialog(&mut self, ctx: &egui::Context) {
        let colors = Palette::from_ctx(ctx);
        let Some(form) = &mut self.sftp_form else {
            return;
        };
        let busy = self.connection.is_some() || self.job.is_some();
        let mut submit = false;
        let mut cancel = false;
        let mut forget = false;
        egui::Modal::new(egui::Id::new("sftp_connect")).show(ctx, |ui| {
            ui.set_width(470.0);
            ui.heading(crate::i18n::text("Подключение SFTP"));
            ui.add_space(10.0);
            ui.add_enabled_ui(!busy, |ui| {
                egui::ComboBox::from_id_salt("saved_sftp_profiles").selected_text(crate::i18n::text("Сохранённые подключения")).show_ui(ui, |ui| {
                    for profile in &self.settings.sftp_profiles {
                        if ui.selectable_label(false, profile.display()).clicked() {
                            if let Location::Sftp { endpoint, path } = profile {
                                form.host = endpoint.host.clone(); form.port = endpoint.port; form.user = endpoint.user.clone();
                                form.key = endpoint.key.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                                form.path = path.display().to_string(); form.password.clear(); form.passphrase.clear();
                                form.use_saved = true; form.save_key = false; form.error = None;
                            }
                        }
                    }
                });
                egui::Grid::new("sftp_fields").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label(crate::i18n::text("Сервер")); ui.text_edit_singleline(&mut form.host); ui.end_row();
                    ui.label(crate::i18n::text("Порт")); ui.add(egui::DragValue::new(&mut form.port).range(1..=65535)); ui.end_row();
                    ui.label(crate::i18n::text("Пользователь")); ui.text_edit_singleline(&mut form.user); ui.end_row();
                    ui.label(crate::i18n::text("Начальная папка")); ui.text_edit_singleline(&mut form.path); ui.end_row();
                    ui.label(crate::i18n::text("Пароль")); ui.add(egui::TextEdit::singleline(&mut form.password).password(true)); ui.end_row();
                    ui.label(crate::i18n::text("Приватный ключ")); ui.add(egui::TextEdit::singleline(&mut form.key).hint_text(crate::i18n::text("Путь к ключу (необязательно)"))); ui.end_row();
                    ui.label(crate::i18n::text("Пароль ключа")); ui.add(egui::TextEdit::singleline(&mut form.passphrase).password(true)); ui.end_row();
                });
                ui.add_space(10.0);
                ui.checkbox(&mut form.use_saved, crate::i18n::text("Использовать сохранённый ключ и фразу"));
                ui.checkbox(&mut form.save_key, crate::i18n::text("Сохранить ключ и фразу после подключения"));
                ui.weak(crate::i18n::text("Ключ и его фраза сохраняются в системном хранилище секретов. Пароль аккаунта не сохраняется. При сохранении читается выбранный файл ключа. ~/.ssh/config не читается."));
                forget = ui.button(crate::i18n::text("Удалить сохранённый секрет")).clicked();
                ui.weak(crate::i18n::text("Ключ сервера должен быть проверен и сохранён в ~/.ssh/known_hosts через OpenSSH."));
                if let Some(error) = &form.error { ui.colored_label(colors.error, error); }
                ui.horizontal(|ui| {
                    cancel = ui.button(crate::i18n::text("Отмена")).clicked();
                    submit = ui.add_enabled(!form.host.trim().is_empty() && !form.user.trim().is_empty(), egui::Button::new(crate::i18n::text("Подключиться"))).clicked();
                });
            });
            if busy { ui.horizontal(|ui| { ui.spinner(); ui.label(crate::i18n::text("Подключение…")); }); }
        });
        if submit {
            let endpoint = Endpoint {
                host: form.host.trim().into(),
                port: form.port,
                user: form.user.trim().into(),
                key: if form.key.is_empty() {
                    None
                } else {
                    Some(if let Some(rest) = form.key.strip_prefix("~/") {
                        dirs::home_dir().unwrap_or_default().join(rest)
                    } else {
                        PathBuf::from(&form.key)
                    })
                },
            };
            let path = form.path.clone();
            let password = std::mem::take(&mut form.password);
            let passphrase = std::mem::take(&mut form.passphrase);
            form.error = None;
            let (tx, rx) = mpsc::channel();
            self.connection = Some(ConnectionJob {
                pane: self.active,
                save_profile: form.save_key,
                result: rx,
            });
            let use_saved = form.use_saved;
            let save_key = form.save_key;
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(fs_ops::connect_location(
                    endpoint, &path, password, passphrase, use_saved, save_key,
                ));
                ctx.request_repaint();
            });
        } else if cancel {
            self.sftp_form = None;
        } else if forget {
            let endpoint = Endpoint {
                host: form.host.trim().into(),
                port: form.port,
                user: form.user.trim().into(),
                key: if form.key.is_empty() {
                    None
                } else {
                    Some(if let Some(rest) = form.key.strip_prefix("~/") {
                        dirs::home_dir().unwrap_or_default().join(rest)
                    } else {
                        PathBuf::from(&form.key)
                    })
                },
            };
            form.passphrase.clear();
            self.sftp_form = None;
            self.start_job(ctx, crate::i18n::text("Удаление сохранённого секрета"), move || {
                crate::credentials::forget(&endpoint)?;
                Ok(crate::i18n::text("Сохранённый ключ и фраза удалены. Исходный файл ключа и открытое SSH-соединение сохранены.").into())
            });
        }
    }
    fn dialogs(&mut self, ctx: &egui::Context) {
        let colors = Palette::from_ctx(ctx);
        if self.show_help {
            egui::Window::new(crate::i18n::text("О PawCommander")).collapsible(false).resizable(false)
                .vscroll(true).max_height((ctx.screen_rect().height() - 64.0).max(100.0))
                .open(&mut self.show_help).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(icons::logo(96.0));
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.heading(crate::i18n::text("Ваши файлы. Две панели."));
                        ui.label(crate::i18n::text("Файловый менеджер на Rust, вдохновлённый Dolphin."));
                    });
                });
                ui.add_space(12.0);
                egui::Grid::new("shortcuts").min_row_height(22.0).spacing([30.0, 6.0]).show(ui, |ui| {
                    for (key, action) in [("⌘/Ctrl+L", crate::i18n::text("Ввести путь")), ("⌘/Ctrl+F", crate::i18n::text("Фильтр текущей папки")), ("⌘/Ctrl+T / W", crate::i18n::text("Новая / закрыть вкладку")), ("⌘/Ctrl+C / X / V", crate::i18n::text("Копировать / вырезать / вставить")), ("⌘/Ctrl+A", crate::i18n::text("Выделить всё")), ("⌘/Ctrl+Shift+N", crate::i18n::text("Создать папку")), ("⌘/Ctrl+H", crate::i18n::text("Скрытые файлы")), ("Alt+← / → / ↑", crate::i18n::text("Назад / вперёд / вверх")), ("F2 / F3 / F5", crate::i18n::text("Переименовать / две панели / обновить")), ("Delete / ⌘+Backspace", crate::i18n::text("В корзину с подтверждением")), (crate::i18n::text("Shift / ⌘/Ctrl + щелчок"), crate::i18n::text("Выделение диапазона / нескольких файлов"))] {
                        ui.monospace(key); ui.label(action); ui.end_row();
                    }
                });
                ui.add_space(14.0); ui.label(crate::i18n::text("Буфер файлов работает внутри PawCommander. «Копировать путь» использует системный буфер."));
                ui.label(crate::i18n::text("При совпадении имён выберите действие в диалоге. Удаление SFTP — навсегда с подтверждением; локальные файлы отправляются в корзину."));
            });
        }
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let mut cancel = false;
        let mut confirm = false;
        let title = match &dialog {
            Dialog::NewFolder { .. } => crate::i18n::text("Новая папка"),
            Dialog::Rename { .. } => crate::i18n::text("Переименовать"),
            Dialog::Trash { paths } => {
                if paths.iter().any(Location::remote) {
                    crate::i18n::text("Удалить с сервера навсегда?")
                } else {
                    crate::i18n::text("Отправить в корзину?")
                }
            }
        };
        // The modal blocks file interactions while a concrete destination is being confirmed.
        egui::Modal::new(egui::Id::new("file_dialog")).show(ctx, |ui| {
            ui.set_width(430.0);
            ui.heading(title);
            ui.add_space(12.0);
            match &mut dialog {
                Dialog::NewFolder { parent, name } => {
                    ui.label(RichText::new(parent.display().to_string()).color(colors.muted));
                    let response = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text(crate::i18n::text("Имя папки"))
                            .desired_width(f32::INFINITY),
                    );
                    if self.dialog_focus {
                        response.request_focus();
                    }
                }
                Dialog::Rename { source, name } => {
                    ui.label(RichText::new(source.display().to_string()).color(colors.muted));
                    let response =
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    if self.dialog_focus {
                        response.request_focus();
                    }
                }
                Dialog::Trash { paths } => {
                    ui.label(crate::tr_format!("Выбрано объектов: {}", "Selected items: {}", paths.len()));
                    for p in paths.iter().take(6) {
                        ui.label(file_name(p));
                    }
                    if paths.len() > 6 {
                        ui.label(crate::tr_format!("…и ещё {}", "…and {} more", paths.len() - 6));
                    }
                    ui.add_space(8.0);
                    ui.label(if paths.iter().any(Location::remote) { crate::i18n::text("Файлы и содержимое папок будут удалены с сервера без возможности восстановления из корзины.") } else { crate::i18n::text("Их можно будет восстановить из системной корзины.") });
                }
            }
            self.dialog_focus = false;
            ui.add_space(16.0);
            let valid = match &dialog {
                Dialog::NewFolder { name, .. } | Dialog::Rename { name, .. } => {
                    !name.trim().is_empty()
                }
                Dialog::Trash { .. } => true,
            };
            ui.horizontal(|ui| {
                cancel = ui.button(crate::i18n::text("Отмена")).clicked();
                let label = if matches!(dialog, Dialog::Trash { .. }) {
                    if matches!(&dialog, Dialog::Trash { paths } if paths.iter().any(Location::remote)) { crate::i18n::text("Удалить навсегда") } else { crate::i18n::text("В корзину") }
                } else {
                    crate::i18n::text("Сохранить")
                };
                confirm = ui
                    .add_enabled(
                        valid,
                        egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(Color32::from_rgb(45, 105, 220)),
                    )
                    .clicked();
            });
            // Enter is only an editing shortcut; trash always requires its explicit button.
            if !matches!(dialog, Dialog::Trash { .. })
                && valid
                && ctx.input(|i| i.key_pressed(egui::Key::Enter))
            {
                confirm = true;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
        });
        if confirm && !cancel {
            match dialog {
                Dialog::NewFolder { parent, name } => self.start_progress_job(
                    ctx,
                    crate::i18n::text("Создание папки"),
                    move |progress| {
                        progress.item(&parent.path().join(&name));
                        fs_ops::create_directory(&parent, &name).map(|p| {
                            crate::tr_format!(
                                "Папка создана: {}",
                                "Folder created: {}",
                                p.display()
                            )
                        })
                    },
                ),
                Dialog::Rename { source, name } => self.start_progress_job(
                    ctx,
                    crate::i18n::text("Переименование"),
                    move |progress| {
                        progress.item(source.path());
                        fs_ops::rename_entry(&source, &name).map(|p| {
                            crate::tr_format!("Переименовано: {}", "Renamed: {}", p.display())
                        })
                    },
                ),
                Dialog::Trash { paths } => self.start_progress_job(
                    ctx,
                    crate::i18n::text("Удаление файлов"),
                    move |progress| {
                        fs_ops::trash_entries_with_progress(&paths, progress).map(|n| {
                            crate::tr_format!("Обработано объектов: {n}", "Items processed: {n}")
                        })
                    },
                ),
            }
        } else if !cancel {
            self.dialog = Some(dialog);
        }
    }
}
impl eframe::App for PawCommander {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, input: &mut egui::RawInput) {
        if let Some(accepted) = self.native_drag.finish_input(input) {
            if accepted {
                self.notice = Some((
                    crate::i18n::text("Файлы переданы принимающему приложению").into(),
                    false,
                ));
            }
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let colors = Palette::from_ctx(ctx);
        self.poll(ctx);
        if self.job.is_some() && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.notice = Some((
                crate::i18n::text("Дождитесь завершения файловой операции, затем закройте окно.")
                    .into(),
                false,
            ));
        }
        self.keyboard(ctx);
        let mut actions = vec![];
        let mut drop_targets = DropTargets::default();
        let mut tab_targets = TabDropTargets::default();
        self.toolbar(ctx, &mut actions);
        self.footer(ctx);
        self.sidebar(ctx, &mut actions);
        let drop_enabled = self.drop_enabled() && !self.native_drag.active();
        let zed_available = self.tools.zed.is_some();
        if self.settings.preview {
            let selected = self.selected();
            let fingerprint = selected
                .first()
                .and_then(|path| self.tab().entries.iter().find(|entry| &entry.path == path))
                .map(|entry| (entry.size, entry.modified));
            egui::SidePanel::right("preview")
                .default_width(260.0)
                .width_range(200.0..=380.0)
                .show_separator_line(false)
                .frame(egui::Frame::new().fill(colors.surface))
                .show(ctx, |panel_ui| {
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
                        ui.label(crate::tr_format!(
                            "Выбрано объектов: {}",
                            "Selected items: {}",
                            selected.len()
                        ));
                    }
                    self.preview.show(ui, selected.first(), fingerprint);
                    card.end(panel_ui);
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(colors.surface).inner_margin(8))
            .show(ctx, |ui| {
                if self.settings.split {
                    ui.columns(2, |cols| {
                        for (index, col) in cols.iter_mut().enumerate() {
                            render_pane(
                                col,
                                &mut self.panes[index],
                                PaneOptions {
                                    index,
                                    active: self.active == index,
                                    settings: &self.settings,
                                    idle: drop_enabled,
                                    zed_available,
                                },
                                &mut actions,
                                &mut drop_targets,
                                &mut tab_targets,
                            );
                        }
                    });
                } else {
                    render_pane(
                        ui,
                        &mut self.panes[0],
                        PaneOptions {
                            index: 0,
                            active: true,
                            settings: &self.settings,
                            idle: drop_enabled,
                            zed_available,
                        },
                        &mut actions,
                        &mut drop_targets,
                        &mut tab_targets,
                    );
                }
            });
        if self.dialog.is_none()
            && !self.conflict_open()
            && self.sftp_form.is_none()
            && !self.show_help
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
        if let Some(Err(error)) = self.native_drag.handoff(ctx, frame, enabled) {
            self.notice = Some((error, true));
        }
        if let Some(result) = drop_targets.finish(ctx, enabled) {
            match result {
                Ok(request) => self.drop_files(request, ctx),
                Err(error) => self.notice = Some((error, true)),
            }
        }
        if let Some(transfer) = &mut self.transfer {
            if let Some(conflict) = &mut transfer.conflict {
                if conflict.show(ctx) {
                    transfer.conflict = None;
                }
                return;
            }
        }
        self.dialogs(ctx);
        if self
            .properties
            .as_ref()
            .is_some_and(|properties| !properties.show(ctx))
        {
            self.properties = None;
        }
        self.sftp_dialog(ctx);
        if let Some(path) = self.search.show(ctx, self.tools.locate.as_deref()) {
            self.reveal_search_result(path, ctx);
        }
        if self.job.is_some()
            || self.editor_job.is_some()
            || self.connection.is_some()
            || self
                .panes
                .iter()
                .any(|p| p.tabs.iter().any(Tab::blocking_load))
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.locations = self
            .panes
            .iter()
            .map(|p| p.tabs.iter().map(|t| t.path.clone()).collect())
            .collect();
        eframe::set_value(storage, "pawcommander.settings", &self.settings);
    }
}

struct PaneOptions<'a> {
    index: usize,
    active: bool,
    settings: &'a Settings,
    idle: bool,
    zed_available: bool,
}

fn workspace_border(colors: Palette) -> Color32 {
    colors.line.lerp_to_gamma(colors.muted, 0.35)
}

fn workspace_card(colors: Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(colors.bg)
        .corner_radius(12)
        .stroke(Stroke::new(1.0, workspace_border(colors)))
}

fn tab_strip(
    ui: &mut egui::Ui,
    pane: &mut Pane,
    index: usize,
    actions: &mut Vec<Action>,
    targets: &mut DropTargets,
    tab_targets: &mut TabDropTargets,
    idle: bool,
) -> (egui::Rect, Option<egui::Rect>) {
    const HEIGHT: f32 = 34.0;
    const PLUS_WIDTH: f32 = 35.0;
    const GAP: f32 = 2.0;
    let colors = Palette::from_ctx(ui.ctx());
    let border = workspace_border(colors);
    let active_id = ui.id().with("last_active_tab");
    let tab_id = pane.tab().id;
    let changed = ui.data_mut(|data| {
        let previous = data.get_temp::<(u64, usize)>(active_id);
        let current = (tab_id, pane.current);
        data.insert_temp(active_id, current);
        previous != Some(current)
    });
    let mut visible_tabs = Vec::new();
    let mut active_tab = None;
    let strip = ui
        .horizontal(|ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing.x = GAP;
            let available = (ui.available_width() - PLUS_WIDTH - GAP).max(20.0);
            let scroll = egui::ScrollArea::horizontal()
                .id_salt("tabs")
                .max_width(available)
                .drag_to_scroll(false)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .auto_shrink([true, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    let viewport = ui.clip_rect();
                    let dragging = egui::DragAndDrop::has_payload_of_type::<DragTab>(ui.ctx());
                    let group = ui.horizontal(|ui| {
                        for i in 0..pane.tabs.len() {
                            let tab = &pane.tabs[i];
                            let path = &tab.path;
                            let selected = pane.current == i;
                            let name = file_name(path);
                            let text = ui.painter().layout_no_wrap(
                                name.clone(),
                                FontId::proportional(13.0),
                                colors.ink,
                            );
                            let width = (text.size().x + 66.0).clamp(108.0, 182.0);
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::new(width, HEIGHT), Sense::hover());
                            let clipped = rect.intersect(viewport);
                            if clipped.width() > 0.0 {
                                visible_tabs.push((i, clipped));
                            }
                            let close_rect = egui::Rect::from_center_size(
                                rect.right_center() - Vec2::new(16.0, 0.0),
                                Vec2::splat(24.0),
                            );
                            let body = if pane.tabs.len() > 1 {
                                egui::Rect::from_min_max(
                                    rect.min,
                                    egui::pos2(close_rect.left(), rect.bottom()),
                                )
                            } else {
                                rect
                            };
                            let response = ui.interact(
                                body,
                                ui.id().with(("tab", tab.id)),
                                Sense::click_and_drag(),
                            );
                            response.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::SelectableLabel,
                                    true,
                                    selected,
                                    &name,
                                )
                            });
                            if selected {
                                active_tab = Some(rect);
                            } else {
                                let fill = if response.hovered() {
                                    colors.bg.lerp_to_gamma(colors.raised, 0.5)
                                } else {
                                    colors.surface
                                };
                                let mut outline = rect;
                                if i == 0 {
                                    outline.min.x -= 1.0;
                                }
                                paint_tab(ui, outline, fill, border);
                            }
                            let icon = if path.remote() {
                                Icon::Server
                            } else {
                                Icon::Folder
                            };
                            icons::paint(
                                ui,
                                egui::Rect::from_center_size(
                                    rect.left_center() + Vec2::new(17.0, 0.0),
                                    Vec2::splat(19.0),
                                ),
                                icon,
                                icons::tint(icon, colors),
                            );
                            let text_rect = egui::Rect::from_min_max(
                                rect.min + Vec2::new(32.0, 0.0),
                                egui::pos2(body.right() - 3.0, rect.bottom()),
                            )
                            .intersect(ui.clip_rect());
                            let text = if text.size().x > body.width() - 35.0 {
                                let mut job = egui::text::LayoutJob::simple(
                                    name.clone(),
                                    FontId::proportional(13.0),
                                    colors.ink,
                                    body.width() - 35.0,
                                );
                                job.wrap.max_rows = 1;
                                job.wrap.break_anywhere = true;
                                ui.painter().layout_job(job)
                            } else {
                                text
                            };
                            ui.painter().with_clip_rect(text_rect).galley(
                                egui::pos2(
                                    rect.left() + 32.0,
                                    rect.center().y - text.size().y / 2.0,
                                ),
                                text,
                                colors.ink,
                            );
                            targets.add(ui, rect, index, path, true);
                            if changed && selected {
                                response.scroll_to_me(Some(egui::Align::Center));
                            }
                            if idle && response.drag_started_by(egui::PointerButton::Primary) {
                                egui::DragAndDrop::set_payload(
                                    ui.ctx(),
                                    DragTab {
                                        id: tab.id,
                                        pane: index,
                                        label: name.clone(),
                                    },
                                );
                            } else if response.clicked() {
                                pane.current = i;
                                actions.push(Action::Focus(index));
                            }
                            if !dragging {
                                response
                                    .on_hover_text(path.display())
                                    .on_hover_cursor(egui::CursorIcon::Grab);
                            }
                            if pane.tabs.len() > 1 {
                                let close = ui.interact(
                                    close_rect,
                                    ui.id().with(("close_tab", tab.id)),
                                    Sense::click(),
                                );
                                close.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::Button,
                                        true,
                                        crate::tr_format!(
                                            "Закрыть вкладку {name}",
                                            "Close tab {name}"
                                        ),
                                    )
                                });
                                if close.hovered() {
                                    ui.painter().rect_filled(close_rect, 5, colors.selection);
                                }
                                icons::paint(ui, close_rect.shrink(5.0), Icon::Close, colors.muted);
                                if close.clicked() {
                                    actions.push(Action::CloseTab(index, i));
                                }
                            }
                        }
                    });
                    // Scroll overflowing strips while hovering an edge with a tab.
                    if dragging {
                        let pointer = ui.input(|i| {
                            i.pointer
                                .primary_down()
                                .then(|| i.pointer.hover_pos())
                                .flatten()
                        });
                        if let Some(pos) = pointer.filter(|pos| viewport.contains(*pos)) {
                            let delta = if pos.x < viewport.left() + 18.0
                                && group.response.rect.left() < viewport.left()
                            {
                                12.0
                            } else if pos.x > viewport.right() - 18.0
                                && group.response.rect.right() > viewport.right()
                            {
                                -12.0
                            } else {
                                0.0
                            };
                            if delta != 0.0 {
                                ui.scroll_with_delta_animation(
                                    egui::vec2(delta, 0.0),
                                    egui::style::ScrollAnimation::none(),
                                );
                                ui.ctx().request_repaint_after(Duration::from_millis(16));
                            }
                        }
                    }
                });
            active_tab = active_tab
                .map(|rect| rect.intersect(scroll.inner_rect))
                .filter(|rect| rect.is_positive());
            let (plus, response) =
                ui.allocate_exact_size(Vec2::new(PLUS_WIDTH, HEIGHT), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    true,
                    crate::i18n::text("Новая вкладка"),
                )
            });
            paint_tab(
                ui,
                plus,
                if response.hovered() {
                    colors.raised
                } else {
                    colors.surface
                },
                border,
            );
            icons::paint(
                ui,
                egui::Rect::from_center_size(plus.center(), Vec2::splat(18.0)),
                Icon::Plus,
                colors.ink,
            );
            if response
                .on_hover_text(crate::i18n::text("Новая вкладка · ⌘/Ctrl+T"))
                .clicked()
            {
                actions.push(Action::NewTab(index));
            }
        })
        .response
        .rect;
    tab_targets.add_strip(ui, index, strip, pane.tabs.len(), visible_tabs);
    (strip, active_tab)
}

fn paint_tab(ui: &egui::Ui, rect: egui::Rect, fill: Color32, line: Color32) {
    ui.painter().rect(
        rect,
        egui::CornerRadius {
            nw: 9,
            ne: 9,
            sw: 0,
            se: 0,
        },
        fill,
        Stroke::new(1.0, line),
        egui::StrokeKind::Inside,
    );
}

fn paint_pane_outline(
    painter: &egui::Painter,
    background: egui::layers::ShapeIdx,
    body: egui::Rect,
    active_tab: Option<egui::Rect>,
    fill: Color32,
    stroke: Stroke,
) {
    use egui::epaint::tessellator::path::add_circle_quadrant;
    use egui::{pos2, Shape};

    const PANE_RADIUS: u8 = 12;
    let body_corners = egui::CornerRadius {
        nw: 0,
        ..PANE_RADIUS.into()
    };
    let Some(mut tab) = active_tab else {
        painter.set(background, Shape::rect_filled(body, body_corners, fill));
        painter.rect_stroke(body, body_corners, stroke, egui::StrokeKind::Middle);
        return;
    };
    let flush_left = tab.left() - body.left() <= 1.0;
    if flush_left {
        tab.min.x = body.left();
    }
    let tab_radius = 9.0_f32.min((tab.width() / 2.0 - 0.5).max(0.0)) as u8;
    tab.max.y = body.top() + 1.0;
    // Fill the two rectangles separately because the shared outline is concave.
    painter.set(
        background,
        Shape::Vec(vec![
            Shape::rect_filled(body, body_corners, fill),
            Shape::rect_filled(
                tab,
                egui::CornerRadius {
                    nw: tab_radius,
                    ne: tab_radius,
                    sw: 0,
                    se: 0,
                },
                fill,
            ),
        ]),
    );

    let mut points = Vec::new();
    let r = f32::from(tab_radius);
    let p = f32::from(PANE_RADIUS);
    add_circle_quadrant(&mut points, tab.left_top() + Vec2::new(r, r), r, 2.0);
    add_circle_quadrant(&mut points, tab.right_top() + Vec2::new(-r, r), r, 3.0);
    points.push(pos2(tab.right(), body.top()));
    add_circle_quadrant(&mut points, body.right_top() + Vec2::new(-p, p), p, 3.0);
    add_circle_quadrant(&mut points, body.right_bottom() - Vec2::splat(p), p, 0.0);
    add_circle_quadrant(&mut points, body.left_bottom() + Vec2::new(p, -p), p, 1.0);
    if !flush_left {
        points.push(body.left_top());
        points.push(pos2(tab.left(), body.top()));
    }
    painter.add(Shape::closed_line(points, stroke));
}

fn address_bar(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    index: usize,
    actions: &mut Vec<Action>,
    targets: &mut DropTargets,
) {
    let colors = Palette::from_ctx(ui.ctx());
    let editing = tab.edit_address;
    let fill = colors.bg.lerp_to_gamma(colors.raised, 0.25);
    let frame = egui::Frame::new()
        .fill(fill)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(6, 1))
        .stroke(Stroke::new(
            1.0,
            if tab.address_error.is_some() {
                colors.error
            } else if editing {
                colors.blue
            } else {
                workspace_border(colors)
            },
        ));
    let row_size = Vec2::new(ui.available_width(), icons::COMPACT_SIZE + 4.0);
    ui.allocate_ui_with_layout(
        row_size,
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (icon, label, delta, enabled) in [
                (
                    Icon::Back,
                    crate::i18n::text("Назад"),
                    -1,
                    tab.history_index > 0,
                ),
                (
                    Icon::Forward,
                    crate::i18n::text("Вперёд"),
                    1,
                    tab.history_index + 1 < tab.history.len(),
                ),
            ] {
                let button = ui
                    .add_enabled_ui(enabled, |ui| icons::compact_button(ui, icon, label, false))
                    .inner;
                if button.clicked() {
                    actions.push(Action::History(index, delta));
                }
            }
            ui.add_space(6.0);
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let spacing = ui.spacing_mut();
                    spacing.item_spacing.x = 3.0;
                    spacing.interact_size.y = icons::COMPACT_SIZE;
                    let trailing = icons::COMPACT_SIZE + spacing.item_spacing.x;
                    if editing {
                        edit_address(ui, tab, index, trailing, actions);
                    } else {
                        let width = (ui.available_width() - trailing).max(40.0);
                        let scroll = egui::ScrollArea::horizontal()
                            .id_salt("breadcrumbs")
                            .max_width(width)
                            .auto_shrink([false, true])
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            )
                            .stick_to_right(true)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let home = dirs::home_dir();
                                    let ancestors: Vec<_> = tab.path.ancestors().collect();
                                    for path in ancestors.iter().rev() {
                                        let current = *path == tab.path;
                                        let home = home
                                            .as_deref()
                                            .is_some_and(|home| path.local() == Some(home));
                                        let (label, icon) = if home {
                                            (crate::i18n::text("Домой").into(), Some(Icon::Home))
                                        } else if path.parent().is_none() && !path.remote() {
                                            (
                                                if cfg!(windows) {
                                                    file_name(path)
                                                } else {
                                                    crate::i18n::text("Диск").into()
                                                },
                                                Some(Icon::Drive),
                                            )
                                        } else {
                                            (file_name(path), None)
                                        };
                                        let response =
                                            breadcrumb(ui, &label, icon, current, colors);
                                        targets.add(ui, response.rect, index, path, true);
                                        if response.clicked() {
                                            actions.push(Action::Navigate(index, (*path).clone()));
                                        }
                                        response.on_hover_text(crate::tr_format!(
                                            "Перейти в {}",
                                            "Go to {}",
                                            path.display()
                                        ));
                                        if !current {
                                            let (r, _) = ui.allocate_exact_size(
                                                Vec2::new(8.0, 20.0),
                                                Sense::hover(),
                                            );
                                            ui.painter().add(egui::Shape::line(
                                                vec![
                                                    r.center() + Vec2::new(-2.0, -4.0),
                                                    r.center() + Vec2::new(2.0, 0.0),
                                                    r.center() + Vec2::new(-2.0, 4.0),
                                                ],
                                                Stroke::new(1.2, colors.muted),
                                            ));
                                        }
                                    }
                                });
                            });
                        if scroll.state.offset.x > 0.5 {
                            let rect = scroll.inner_rect;
                            let width = rect.width().min(16.0);
                            let mut fade = egui::Mesh::default();
                            fade.colored_vertex(rect.left_top(), fill);
                            fade.colored_vertex(rect.left_bottom(), fill);
                            fade.colored_vertex(
                                rect.left_bottom() + Vec2::new(width, 0.0),
                                Color32::TRANSPARENT,
                            );
                            fade.colored_vertex(
                                rect.left_top() + Vec2::new(width, 0.0),
                                Color32::TRANSPARENT,
                            );
                            fade.add_triangle(0, 1, 2);
                            fade.add_triangle(0, 2, 3);
                            ui.painter().add(egui::Shape::mesh(fade));
                        }
                        if icons::compact_button(
                            ui,
                            Icon::ChevronDown,
                            crate::i18n::text("Ввести путь вручную · ⌘/Ctrl+L"),
                            false,
                        )
                        .clicked()
                        {
                            tab.begin_address();
                            actions.push(Action::Focus(index));
                        }
                    }
                });
            });
        },
    );
    if let Some(error) = &tab.address_error {
        ui.colored_label(colors.error, error);
    }
}

fn edit_address(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    index: usize,
    trailing: f32,
    actions: &mut Vec<Action>,
) {
    let field_id = ui.make_persistent_id(("path_input", tab.id));
    let popup_id = field_id.with("suggestions");
    let mut suggestions = tab
        .address_completion
        .suggestions(&tab.address, &tab.path, ui.ctx());
    let breadcrumbs = icons::compact_button(
        ui,
        Icon::Breadcrumbs,
        crate::i18n::text("Вернуться к хлебным крошкам · Esc"),
        false,
    );
    let cancel = breadcrumbs.clicked();

    let focused = ui.memory(|memory| memory.has_focus(field_id));
    let (enter, complete, step) = ui.input_mut(|input| {
        if !focused || input.modifiers != egui::Modifiers::NONE {
            return (false, false, None);
        }
        let enter = input.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
        if suggestions.is_empty() {
            return (enter, false, None);
        }
        let complete = input.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
        let step = if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
            Some(1)
        } else if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
            Some(-1)
        } else {
            None
        };
        (enter, complete, step)
    });
    let margin = egui::Margin::symmetric(2, 0);
    let width = (ui.available_width() - trailing - margin.sum().x).max(24.0);
    let mut output = egui::TextEdit::singleline(&mut tab.address)
        .id(field_id)
        .font(egui::TextStyle::Body)
        .hint_text(crate::i18n::text("Путь к папке или sftp://сервер/папка"))
        .vertical_align(egui::Align::Center)
        .frame(false)
        .margin(margin)
        .min_size(Vec2::new(0.0, icons::COMPACT_SIZE))
        .desired_width(width)
        .return_key(None)
        .lock_focus(!suggestions.is_empty())
        .show(ui);
    let response = &output.response;
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::TextEdit,
            true,
            crate::i18n::text("Путь к папке"),
        )
    });
    let mut cursor_range = None;
    if tab.focus_address {
        response.request_focus();
        cursor_range = Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(tab.address.chars().count()),
        ));
        tab.focus_address = false;
    }
    if response.has_focus() {
        actions.push(Action::Focus(index));
    }
    if response.changed() {
        tab.address_completion.edited = true;
        tab.address_completion.selected = None;
        tab.address_error = None;
        suggestions = tab
            .address_completion
            .suggestions(&tab.address, &tab.path, ui.ctx());
    }
    if let Some(step) = step.filter(|_| !suggestions.is_empty()) {
        let count = suggestions.len();
        tab.address_completion.selected = Some(match tab.address_completion.selected {
            Some(selected) => (selected as isize + step).rem_euclid(count as isize) as usize,
            None if step > 0 => 0,
            None => count - 1,
        });
    }
    if let Some(item) = complete
        .then(|| suggestions.get(tab.address_completion.selected.unwrap_or(0)))
        .flatten()
    {
        tab.address = item.path.display();
        let separator = if item.path.remote() {
            '/'
        } else {
            std::path::MAIN_SEPARATOR
        };
        if !tab.address.ends_with(separator) {
            tab.address.push(separator);
        }
        tab.address_completion.selected = None;
        tab.address_error = None;
        let end = egui::text::CCursor::new(tab.address.chars().count());
        cursor_range = Some(egui::text::CCursorRange::one(end));
        suggestions = tab
            .address_completion
            .suggestions(&tab.address, &tab.path, ui.ctx());
        ui.ctx().request_repaint();
    }
    if let Some(range) = cursor_range {
        output.state.cursor.set_char_range(Some(range));
        output.state.store(ui.ctx(), field_id);
    }
    let go_button = icons::compact_button(
        ui,
        Icon::Forward,
        crate::i18n::text("Перейти по введённому пути · Enter"),
        false,
    );
    let go = go_button.clicked();

    let escape =
        (focused || response.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Escape));
    let mut destination = None;
    if !cancel && !escape && !suggestions.is_empty() {
        if response.has_focus() {
            ui.memory_mut(|memory| memory.open_popup(popup_id));
        }
        let mut anchor = response.clone();
        anchor.rect = breadcrumbs
            .rect
            .union(go_button.rect)
            .expand2(Vec2::new(7.0, 4.0));
        destination = egui::popup::popup_below_widget(
            ui,
            popup_id,
            &anchor,
            egui::popup::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                ui.set_width(ui.available_width().max(148.0));
                ui.label(
                    egui::RichText::new(crate::i18n::text("Подсказки пути"))
                        .small()
                        .weak(),
                )
                .on_hover_text(crate::i18n::text(
                    "↑↓ выбрать · Tab дополнить · Enter открыть",
                ));
                for (i, item) in suggestions.iter().enumerate() {
                    let selected = tab.address_completion.selected == Some(i);
                    if ui
                        .add(egui::Button::new(&item.name).selected(selected).truncate())
                        .on_hover_text(item.path.display())
                        .clicked()
                    {
                        return Some(item.path.clone());
                    }
                }
                None
            },
        )
        .flatten();
    }
    if destination.is_none() && (enter || go) && !cancel && !escape {
        let selected = tab
            .address_completion
            .selected
            .and_then(|i| suggestions.get(i));
        if let (true, Some(item)) = (enter, selected) {
            destination = Some(item.path.clone());
        } else {
            match resolve_address(&tab.address, &tab.path) {
                Ok(path) => destination = Some(path),
                Err(error) => {
                    tab.address_error = Some(error);
                    response.request_focus();
                }
            }
        }
    }
    if cancel || escape || destination.is_some() {
        tab.address = tab.path.display();
        tab.edit_address = false;
        tab.address_error = None;
        tab.address_completion.reset();
        response.surrender_focus();
    }
    if !tab.edit_address || suggestions.is_empty() {
        ui.memory_mut(|memory| {
            if memory.is_popup_open(popup_id) {
                memory.close_popup();
            }
        });
    }
    if let Some(path) = destination {
        actions.push(Action::Navigate(index, path));
    }
}

fn breadcrumb(
    ui: &mut egui::Ui,
    label: &str,
    icon: Option<Icon>,
    current: bool,
    colors: Palette,
) -> egui::Response {
    let color = if current { colors.ink } else { colors.muted };
    let text = ui
        .painter()
        .layout_no_wrap(label.into(), FontId::proportional(13.0), color);
    let icon_width = if icon.is_some() { 20.0 } else { 0.0 };
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(text.size().x + icon_width + 12.0, 28.0),
        Sense::click(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let highlight = response.hovered() || response.has_focus();
    if highlight {
        ui.painter().rect_filled(rect, 5, colors.raised);
    }
    if let Some(icon) = icon {
        icons::paint(
            ui,
            egui::Rect::from_center_size(
                rect.left_center() + egui::vec2(14.0, 0.0),
                Vec2::splat(17.0),
            ),
            icon,
            color,
        );
    }
    let color = if highlight { colors.ink } else { color };
    ui.painter().galley_with_override_text_color(
        egui::pos2(
            rect.left() + 6.0 + icon_width,
            rect.center().y - text.size().y / 2.0,
        ),
        text,
        color,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn render_pane(
    ui: &mut egui::Ui,
    pane: &mut Pane,
    options: PaneOptions<'_>,
    actions: &mut Vec<Action>,
    drop_targets: &mut DropTargets,
    tab_targets: &mut TabDropTargets,
) {
    let PaneOptions {
        index: pane_index,
        active,
        settings,
        idle,
        zed_available,
    } = options;
    let colors = Palette::from_ctx(ui.ctx());
    ui.push_id(pane_index, |ui| {
        let border = workspace_border(colors);
        const PANE_PADDING: i8 = 10;
        let outline = if active {
            border.lerp_to_gamma(colors.mauve, 0.3)
        } else {
            border
        };
        let background = ui.painter().add(egui::Shape::Noop);
        let frame = egui::Frame::new().inner_margin(egui::Margin {
            left: 1,
            right: 1,
            top: 1,
            bottom: PANE_PADDING + 1,
        });
        let frame = frame.show(ui, |pane_ui| {
            pane_ui.set_min_height(pane_ui.available_height() - 1.0);
            let (strip, active_tab) = tab_strip(
                pane_ui,
                pane,
                pane_index,
                actions,
                drop_targets,
                tab_targets,
                idle,
            );
            let mut content = egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(PANE_PADDING, 0))
                .begin(pane_ui);
            let ui = &mut content.content_ui;
            let grid = settings.grid_for(pane_index);
            let tab = pane.tab_mut();
            address_bar(ui, tab, pane_index, actions, drop_targets);
            let response = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let controls = 2.0 * (icons::COMPACT_SIZE + ui.spacing().item_spacing.x);
                    let filter_width = (ui.available_width() - controls).max(24.0);
                    let response = ui.add_sized(
                        [filter_width, icons::COMPACT_SIZE],
                        egui::TextEdit::singleline(&mut tab.filter)
                            .vertical_align(egui::Align::Center)
                            .margin(egui::Margin::symmetric(8, 0))
                            .hint_text(crate::i18n::text("Фильтр по имени…")),
                    );
                    if icons::compact_button(ui, Icon::Grid, crate::i18n::text("Сетка"), grid)
                        .clicked()
                    {
                        actions.push(Action::SetGrid(pane_index, true));
                    }
                    if icons::compact_button(ui, Icon::List, crate::i18n::text("Список"), !grid)
                        .clicked()
                    {
                        actions.push(Action::SetGrid(pane_index, false));
                    }
                    response
                })
                .inner;
            if tab.focus_filter {
                response.request_focus();
                tab.focus_filter = false;
            }
            if response.has_focus() {
                actions.push(Action::Focus(pane_index));
            }
            if response.changed() {
                let visible_paths: BTreeSet<_> = tab
                    .visible(settings.hidden)
                    .iter()
                    .map(|i| tab.entries[*i].path.clone())
                    .collect();
                tab.selected.retain(|p| visible_paths.contains(p));
            }
            ui.add_space(4.0);
            let (header, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::hover());
            let band = header.expand2(Vec2::new(f32::from(PANE_PADDING), 0.0));
            ui.painter().rect_filled(band, 0, colors.raised);
            for y in [band.top(), band.bottom()] {
                ui.painter()
                    .hline(band.x_range(), y, Stroke::new(1.0, border));
            }
            let wide = header.width() > 380.0;
            let name_end = header.right() - if wide { 182.0 } else { 72.0 };
            for (label, sort, left, right) in [
                (
                    crate::i18n::text("Имя"),
                    Sort::Name,
                    header.left() + 8.0,
                    name_end,
                ),
                (
                    crate::i18n::text("Размер"),
                    Sort::Size,
                    name_end,
                    if wide {
                        header.right() - 113.0
                    } else {
                        header.right()
                    },
                ),
                (
                    crate::i18n::text("Изменён"),
                    Sort::Modified,
                    header.right() - 108.0,
                    header.right(),
                ),
            ] {
                if sort == Sort::Modified && !wide {
                    continue;
                }
                let r = egui::Rect::from_min_max(
                    egui::pos2(left, header.top()),
                    egui::pos2(right, header.bottom()),
                );
                let response = ui.interact(r, ui.id().with(label), Sense::click());
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        ui.is_enabled(),
                        crate::tr_format!("Сортировка: {label}", "Sort: {label}"),
                    )
                });
                let painter = ui.painter().with_clip_rect(r);
                let text = painter.text(
                    r.left_center(),
                    egui::Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(13.0),
                    if response.hovered() {
                        colors.ink
                    } else {
                        colors.muted
                    },
                );
                if tab.sort == sort {
                    let x = text.right() + 6.0;
                    let y = r.center().y;
                    let direction = if tab.descending { -1.0 } else { 1.0 };
                    painter.add(egui::Shape::line(
                        vec![
                            egui::pos2(x, y + 2.0 * direction),
                            egui::pos2(x + 4.0, y - 2.0 * direction),
                            egui::pos2(x + 8.0, y + 2.0 * direction),
                        ],
                        Stroke::new(1.2, colors.muted),
                    ));
                }
                if response.clicked() {
                    if tab.sort == sort {
                        tab.descending = !tab.descending;
                    } else {
                        tab.sort = sort;
                        tab.descending = false;
                    }
                    actions.push(Action::Focus(pane_index));
                }
            }
            drop_targets.add(
                ui,
                ui.available_rect_before_wrap(),
                pane_index,
                &tab.path,
                !tab.blocking_load() && tab.error.is_none(),
            );
            if let Some(error) = &tab.error {
                ui.colored_label(colors.error, error);
            }
            if tab.blocking_load() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(crate::i18n::text("Чтение папки…"));
                });
            }
            let visible = tab.visible(settings.hidden);
            if visible.is_empty() && !tab.blocking_load() && tab.error.is_none() {
                ui.add_space(50.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(if tab.filter.is_empty() {
                            crate::i18n::text("Папка пуста")
                        } else {
                            crate::i18n::text("Ничего не найдено")
                        })
                        .size(20.0)
                        .color(colors.muted),
                    );
                    ui.label(
                        RichText::new(if tab.filter.is_empty() {
                            crate::i18n::text("Создайте папку или вставьте файлы")
                        } else {
                            crate::i18n::text("Попробуйте другое имя")
                        })
                        .size(12.0)
                        .color(colors.muted),
                    );
                });
            }
            let row_height = if grid { 104.0 } else { 36.0 };
            let columns = if grid {
                (ui.available_width() / 108.0).floor().max(1.0) as usize
            } else {
                1
            };
            let rows = visible.len().div_ceil(columns);
            let mut scroll = egui::ScrollArea::vertical();
            if let Some(path) = tab.scroll_to.take() {
                if let Some(position) = visible.iter().position(|i| tab.entries[*i].path == path) {
                    scroll = scroll.vertical_scroll_offset(
                        (position / columns) as f32 * (row_height + ui.spacing().item_spacing.y),
                    );
                }
            }
            const STATUS_HEIGHT: f32 = 22.0;
            const STATUS_RULE_HEIGHT: f32 = 1.0;
            let status_height =
                STATUS_HEIGHT + STATUS_RULE_HEIGHT + 2.0 * ui.spacing().item_spacing.y;
            let files_height = (ui.available_height() - status_height).max(40.0);
            scroll
                .max_height(files_height)
                .min_scrolled_height(files_height)
                .id_salt((&tab.path, "files"))
                .auto_shrink([false, false])
                .show_rows(ui, row_height, rows, |ui, range| {
                    for row in range {
                        if grid {
                            ui.horizontal(|ui| {
                                for col in 0..columns {
                                    if let Some(index) = visible.get(row * columns + col) {
                                        entry_widget(
                                            ui,
                                            tab,
                                            *index,
                                            &visible,
                                            EntryOptions {
                                                pane: pane_index,
                                                split: settings.split,
                                                grid: true,
                                                idle,
                                                zed_available,
                                            },
                                            actions,
                                            drop_targets,
                                        );
                                    }
                                }
                            });
                        } else if let Some(index) = visible.get(row) {
                            entry_widget(
                                ui,
                                tab,
                                *index,
                                &visible,
                                EntryOptions {
                                    pane: pane_index,
                                    split: settings.split,
                                    grid: false,
                                    idle,
                                    zed_available,
                                },
                                actions,
                                drop_targets,
                            );
                        }
                    }
                });
            ui.add(egui::Separator::default().spacing(STATUS_RULE_HEIGHT));
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), STATUS_HEIGHT),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.label(
                        RichText::new(crate::tr_format!(
                            "Выбрано: {}",
                            "Selected: {}",
                            tab.selected.len()
                        ))
                        .size(11.0)
                        .color(colors.muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(crate::tr_format!(
                                "{} объектов",
                                "{} items",
                                visible.len()
                            ))
                            .size(11.0)
                            .color(colors.muted),
                        );
                    });
                },
            );
            content.end(pane_ui);
            (strip, active_tab)
        });
        let (strip, active_tab) = frame.inner;
        let mut body = frame.response.rect.shrink(0.5);
        body.min.y = strip.bottom() - 0.5;
        paint_pane_outline(
            ui.painter(),
            background,
            body,
            active_tab,
            colors.bg,
            Stroke::new(1.0, outline),
        );
        tab_targets.set_pane_rect(ui, pane_index, frame.response.rect);
    });
}

struct EntryOptions {
    pane: usize,
    split: bool,
    grid: bool,
    idle: bool,
    zed_available: bool,
}

fn entry_widget(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    index: usize,
    visible: &[usize],
    options: EntryOptions,
    actions: &mut Vec<Action>,
    drop_targets: &mut DropTargets,
) {
    let colors = Palette::from_ctx(ui.ctx());
    let EntryOptions {
        pane,
        split,
        grid,
        idle,
        zed_available,
    } = options;
    let entry = &tab.entries[index];
    let is_dir = entry.is_dir;
    let path = entry.path.clone();
    let selected = tab.selected.contains(&path);
    let size = if grid {
        Vec2::new(100.0, 98.0)
    } else {
        Vec2::new(ui.available_width(), 36.0)
    };
    let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    if entry.is_dir {
        drop_targets.add(
            ui,
            rect,
            pane,
            &path,
            !tab.blocking_load() && tab.error.is_none(),
        );
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            &entry.name,
        )
    });
    if !grid
        && visible
            .iter()
            .position(|i| *i == index)
            .is_some_and(|i| i % 2 == 0)
    {
        ui.painter()
            .rect_filled(rect, 6, colors.bg.lerp_to_gamma(colors.raised, 0.22));
    }
    if selected || response.hovered() {
        ui.painter().rect_filled(
            rect,
            5,
            if selected {
                colors.selection
            } else {
                colors.raised
            },
        );
    }
    if selected {
        ui.painter().rect_stroke(
            rect,
            6,
            Stroke::new(1.0, colors.mauve.gamma_multiply(0.55)),
            egui::StrokeKind::Inside,
        );
    }
    let check_rect =
        egui::Rect::from_center_size(rect.left_center() + Vec2::new(15.0, 0.0), Vec2::splat(14.0));
    if !grid {
        ui.painter().rect(
            check_rect,
            3,
            if selected {
                colors.mauve
            } else {
                Color32::TRANSPARENT
            },
            Stroke::new(1.0, if selected { colors.mauve } else { colors.muted }),
            egui::StrokeKind::Inside,
        );
        if selected {
            ui.painter().add(egui::Shape::line(
                vec![
                    check_rect.min + Vec2::new(3.0, 7.0),
                    check_rect.min + Vec2::new(6.0, 10.0),
                    check_rect.min + Vec2::new(11.0, 4.0),
                ],
                Stroke::new(1.6, colors.surface),
            ));
        }
    }
    let icon_rect = if grid {
        egui::Rect::from_center_size(rect.center_top() + Vec2::new(0.0, 30.0), Vec2::splat(37.0))
    } else {
        egui::Rect::from_min_size(rect.min + Vec2::new(32.0, 6.0), Vec2::splat(24.0))
    };
    let icon = icons::file_icon(&entry.name, entry.is_dir);
    icons::paint(ui, icon_rect, icon, icons::tint(icon, colors));
    if entry.is_symlink {
        ui.painter().text(
            icon_rect.right_bottom(),
            egui::Align2::RIGHT_BOTTOM,
            "↗",
            FontId::proportional(12.0),
            colors.ink,
        );
    }
    let name_color = colors.ink;
    if grid {
        let galley = ui.painter().layout(
            entry.name.clone(),
            FontId::proportional(12.0),
            name_color,
            93.0,
        );
        let clip =
            egui::Rect::from_min_size(rect.min + Vec2::new(3.0, 55.0), Vec2::new(94.0, 36.0));
        ui.painter().with_clip_rect(clip).galley(
            egui::pos2(rect.center().x - galley.size().x / 2.0, rect.top() + 55.0),
            galley,
            name_color,
        );
    } else {
        let right_width = if rect.width() > 380.0 { 182.0 } else { 72.0 };
        let name_rect = egui::Rect::from_min_max(
            rect.min + Vec2::new(64.0, 0.0),
            egui::pos2(rect.right() - right_width, rect.bottom()),
        );
        ui.painter().with_clip_rect(name_rect).text(
            name_rect.left_center(),
            egui::Align2::LEFT_CENTER,
            &entry.name,
            FontId::proportional(14.0),
            name_color,
        );
        let size = if entry.is_dir {
            "—".into()
        } else {
            human_size(entry.size)
        };
        if rect.width() > 380.0 {
            ui.painter().text(
                egui::pos2(rect.right() - 119.0, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                size,
                FontId::proportional(11.0),
                colors.muted,
            );
            let date = entry
                .modified
                .map(|t| {
                    chrono::DateTime::<chrono::Local>::from(t)
                        .format("%d.%m.%y %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            ui.painter().text(
                rect.right_center() - Vec2::new(5.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                date,
                FontId::proportional(10.0),
                colors.muted,
            );
        } else {
            ui.painter().text(
                rect.right_center() - Vec2::new(5.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                size,
                FontId::proportional(11.0),
                colors.muted,
            );
        }
    }
    if idle && response.drag_started_by(egui::PointerButton::Primary) {
        egui::DragAndDrop::set_payload(ui.ctx(), DragFiles(tab.drag_paths(index, visible)));
        actions.push(Action::Focus(pane));
    }
    let on_checkbox = !grid
        && response
            .interact_pointer_pos()
            .is_some_and(|p| check_rect.expand(4.0).contains(p));
    if response.clicked() {
        let mut modifiers = ui.input(|i| i.modifiers);
        if on_checkbox {
            modifiers.command = true;
        }
        tab.choose(index, visible, modifiers);
        actions.push(Action::Focus(pane));
    }
    if response.double_clicked() && !on_checkbox {
        actions.push(Action::Open(pane, path.clone()));
    }
    if response.secondary_clicked() {
        if !selected {
            tab.selected.clear();
            tab.selected.insert(path.clone());
            tab.anchor = Some(path.clone());
        }
        actions.push(Action::Focus(pane));
    }
    response
        .on_hover_text(path.display().to_string())
        .context_menu(|ui| {
            if ui.button(crate::i18n::text("Открыть")).clicked() {
                actions.push(Action::Open(pane, path.clone()));
                ui.close_menu();
            }
            if !is_dir && ui.add_enabled(zed_available, egui::Button::new(crate::i18n::text("Открыть в Zed")))
                .on_disabled_hover_text(crate::i18n::text("Zed не найден. Установите редактор и выберите Настройки → Инструменты → Проверить снова."))
                .clicked() {
                actions.push(Action::OpenInZed(pane, path.clone()));
                ui.close_menu();
            }
            ui.separator();
            if split {
            ui.menu_button(crate::i18n::text("В соседнюю панель"), |ui| {
                for (label, kind) in [(crate::i18n::text("Копировать"), TransferKind::Copy), (crate::i18n::text("Переместить"), TransferKind::Move)] {
                    if ui.add_enabled(idle, egui::Button::new(label)).clicked() {
                        actions.push(Action::Focus(pane));
                        actions.push(Action::Transfer(kind));
                        ui.close_menu();
                    }
                }
            });
            }
            for (label, action, enabled) in [
                (crate::i18n::text("Копировать"), Action::Copy(false), true),
                (crate::i18n::text("Вырезать"), Action::Copy(true), true),
                (crate::i18n::text("Вставить"), Action::Paste(false), idle),
                (
                    crate::i18n::text("Переименовать…"),
                    Action::Rename,
                    idle && tab.selected.len() == 1,
                ),
                (
                    if path.remote() {
                        crate::i18n::text("Удалить навсегда…")
                    } else {
                        crate::i18n::text("В корзину…")
                    },
                    Action::Trash,
                    idle,
                ),
                (crate::i18n::text("Свойства… · Alt+Enter"), Action::Properties,
                    !tab.selected.is_empty() && tab.selected.iter().all(|path| !path.remote())),
                (crate::i18n::text("Копировать путь"), Action::CopyPath, true),
                (crate::i18n::text("Показать в системе"), Action::Reveal, !path.remote()),
            ] {
                if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                    actions.push(Action::Focus(pane));
                    actions.push(action);
                    ui.close_menu();
                }
            }
        });
}

fn file_name(path: &Location) -> String {
    path.name()
}
fn operation_details(ui: &mut egui::Ui, progress: &crate::operations::Snapshot) {
    if let Some(item) = &progress.item {
        let name = item.file_name().map_or_else(
            || item.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let phase = if progress.waiting_for_conflict {
            crate::i18n::text("Ожидается решение")
        } else {
            crate::i18n::text("Текущий объект")
        };
        ui.add(egui::Label::new(format!("{phase}: {name}")).truncate())
            .on_hover_text(item.display().to_string());
    }
    if let Some(transfer) = &progress.transfer {
        let written = human_size(transfer.written);
        let now = Instant::now();
        ui.horizontal_wrapped(|ui| {
            if let Some(expected) = transfer.expected.filter(|size| transfer.written <= *size) {
                ui.label(format!("{written} / {}", human_size(expected)));
                let fraction = if expected == 0 {
                    if transfer.stream_finished {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    (transfer.written as f64 / expected as f64) as f32
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(100.0)
                        .show_percentage(),
                );
            } else {
                ui.label(crate::tr_format!(
                    "Записано: {written}",
                    "Written: {written}"
                ));
            }
            if let Some(speed) = transfer.bytes_per_second(now) {
                ui.label(crate::tr_format!(
                    "{}/с",
                    "{}/s",
                    human_size(speed.round() as u64)
                ));
            }
            if let Some(seconds) = transfer.remaining_seconds(now) {
                let time = if seconds < 60 {
                    crate::tr_format!("{seconds} с", "{seconds} s")
                } else {
                    crate::tr_format!(
                        "{} мин {:02} с",
                        "{} min {:02} s",
                        seconds / 60,
                        seconds % 60
                    )
                };
                ui.label(crate::tr_format!(
                    "До конца файла: ≈ {time}",
                    "File remaining: ≈ {time}"
                ));
            }
            if transfer.stream_finished {
                ui.label(crate::i18n::text("Завершение записи…"));
            }
        });
    }
    if progress.total_written > 0 {
        ui.label(crate::tr_format!(
            "Записано за операцию: {}",
            "Total written: {}",
            human_size(progress.total_written)
        ));
    }
}
fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        crate::tr_format!("{bytes} Б", "{bytes} B")
    } else if bytes < 1024 * 1024 {
        crate::tr_format!("{:.1} КБ", "{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        crate::tr_format!("{:.1} МБ", "{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        crate::tr_format!(
            "{:.1} ГБ",
            "{:.1} GB",
            bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        )
    }
}
fn resolve_address(address: &str, current: &Location) -> Result<Location, String> {
    fs_ops::resolve(address, current)
}
fn place_button(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let colors = Palette::from_ctx(ui.ctx());
    let icon = match label {
        "Домашняя" | "Home" => Icon::Home,
        "Рабочий стол" | "Desktop" => Icon::Desktop,
        "Документы" | "Documents" => Icon::Document,
        "Загрузки" | "Downloads" => Icon::Download,
        "Изображения" | "Pictures" => Icon::Image,
        "Музыка" | "Music" => Icon::Music,
        "Видео" | "Videos" => Icon::Video,
        "Файловая система"
        | "Тома и диски"
        | "Носители"
        | "Filesystem"
        | "Volumes and disks"
        | "Media" => Icon::Drive,
        x if x.starts_with("SFTP") => Icon::Server,
        _ => Icon::Folder,
    };
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 31.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            label,
        )
    });
    if selected || response.hovered() {
        ui.painter().rect(
            rect,
            8,
            if selected {
                colors.selection
            } else {
                colors.raised
            },
            Stroke::new(
                1.0,
                if selected {
                    colors.mauve.gamma_multiply(0.7)
                } else {
                    Color32::TRANSPARENT
                },
            ),
            egui::StrokeKind::Inside,
        );
    }
    icons::paint(
        ui,
        egui::Rect::from_center_size(rect.left_center() + Vec2::new(20.0, 0.0), Vec2::splat(21.0)),
        icon,
        icons::tint(icon, colors),
    );
    let text_rect = rect.shrink2(Vec2::new(8.0, 0.0));
    ui.painter().with_clip_rect(text_rect).text(
        rect.left_center() + Vec2::new(40.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        colors.ink,
    );
    response.on_hover_text(label).clicked()
}

#[cfg(test)]
mod tests {
    #[test]
    fn language_settings_preserve_legacy_preferences_and_round_trip() {
        let mut settings: super::Settings =
            serde_json::from_str(r#"{"hidden":true,"split":false}"#).unwrap();
        assert_eq!(settings.language, crate::i18n::Language::English);
        assert!(settings.hidden);
        assert!(!settings.split);
        settings.language = crate::i18n::Language::Russian;
        let restored: super::Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored.language, crate::i18n::Language::Russian);
        assert!(restored.hidden);
        assert!(!restored.split);
    }

    use super::*;

    fn wait_for_directory(tab: &mut Tab) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while tab.loading.is_some() && Instant::now() < deadline {
            tab.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(tab.loading.is_none(), "directory read did not finish");
    }

    #[test]
    fn automatic_refresh_reads_real_files_preserves_state_and_recovers_after_error() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("watched");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("keep.txt"), "old").unwrap();
        std::fs::write(folder.join("rename.txt"), "rename").unwrap();
        let ctx = egui::Context::default();
        let mut tab = Tab::new(folder.clone().into(), &ctx);
        wait_for_directory(&mut tab);
        let keep = tab.path.join("keep.txt");
        let renamed = tab.path.join("rename.txt");
        tab.selected = BTreeSet::from([keep.clone(), renamed.clone()]);
        tab.cursor = Some(keep.clone());
        tab.anchor = Some(renamed);
        tab.filter = ".txt".into();
        tab.sort = Sort::Size;
        tab.descending = true;
        // Changes are made independently of PawCommander's operation/refresh path.
        std::fs::write(folder.join("keep.txt"), "changed file contents").unwrap();
        std::fs::rename(folder.join("rename.txt"), folder.join("renamed.txt")).unwrap();
        std::fs::write(folder.join("new.txt"), "new").unwrap();
        tab.auto_refresh(&ctx, tab.next_auto_refresh - Duration::from_millis(1));
        assert!(tab.loading.is_none(), "poll must respect its interval");
        tab.auto_refresh(&ctx, tab.next_auto_refresh);
        assert!(tab.automatic_loading);
        assert!(
            !tab.blocking_load(),
            "automatic reads must not add a spinner"
        );
        wait_for_directory(&mut tab);
        assert_eq!(tab.selected, BTreeSet::from([keep.clone()]));
        assert_eq!(tab.cursor, Some(keep.clone()));
        assert!(tab.anchor.is_none());
        assert!(tab.scroll_to.is_none());
        assert_eq!(tab.filter, ".txt");
        assert!(tab.sort == Sort::Size && tab.descending);
        assert_eq!(tab.entries.len(), 3);
        assert_eq!(
            tab.entries.iter().find(|e| e.path == keep).unwrap().size,
            21
        );
        assert!(tab.entries.iter().any(|e| e.name == "renamed.txt"));

        // A missing/remounted volume keeps the last readable listing and retries.
        let moved = root.path().join("offline");
        std::fs::rename(&folder, &moved).unwrap();
        tab.auto_refresh(&ctx, tab.next_auto_refresh);
        wait_for_directory(&mut tab);
        assert!(tab
            .error
            .as_ref()
            .unwrap()
            .contains("last successful listing"));
        assert_eq!(tab.entries.len(), 3);
        assert!(tab.selected.contains(&keep));
        std::fs::rename(&moved, &folder).unwrap();
        std::fs::remove_file(folder.join("keep.txt")).unwrap();
        tab.auto_refresh(&ctx, tab.next_auto_refresh);
        wait_for_directory(&mut tab);
        assert!(tab.error.is_none());
        assert_eq!(tab.entries.len(), 2);
        assert!(tab.selected.is_empty());
        assert!(tab.cursor.is_none());
    }

    #[test]
    fn automatic_refresh_is_single_flight_ignores_stale_reads_and_skips_sftp() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("current"), "current").unwrap();
        let ctx = egui::Context::default();
        let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
        wait_for_directory(&mut tab);
        let (stale_tx, rx) = mpsc::channel();
        tab.loading = Some(rx);
        tab.automatic_loading = true;
        tab.auto_refresh(&ctx, tab.next_auto_refresh);
        assert!(tab.automatic_loading);
        // Sending to the original receiver proves no second scan replaced it.
        stale_tx.send(Ok(vec![])).unwrap();
        tab.navigate(other.path().to_path_buf().into(), &ctx);
        wait_for_directory(&mut tab);
        assert_eq!(tab.entries.len(), 1);
        assert_eq!(tab.entries[0].name, "current");
        tab.path = Location::Sftp {
            endpoint: Endpoint {
                host: "unused.invalid".into(),
                port: 22,
                user: "test".into(),
                key: None,
            },
            path: "/".into(),
        };
        tab.auto_refresh(&ctx, tab.next_auto_refresh);
        assert!(
            tab.loading.is_none(),
            "automatic refresh must never connect to SFTP"
        );
    }

    #[test]
    fn properties_shortcut_opens_selected_group_without_navigating() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("folder")).unwrap();
        std::fs::write(root.path().join("file"), b"data").unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        wait_for_directory(app.tab_mut());
        app.tab_mut().selected = BTreeSet::from([
            root.path().join("folder").into(),
            root.path().join("file").into(),
        ]);
        let before = app.tab().path.clone();
        let _ = ctx.run(
            egui::RawInput {
                modifiers: egui::Modifiers::ALT,
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::ALT,
                }],
                ..Default::default()
            },
            |ctx| app.keyboard(ctx),
        );
        assert!(app.properties.is_some());
        assert_eq!(app.tab().path, before);
        assert_eq!(app.selected().len(), 2);
    }

    #[test]
    fn automatic_refresh_schedules_only_displayed_tabs_and_waits_for_pointer_release() {
        let root = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.settings.split = false;
        let background = Tab::new(root.path().to_path_buf().into(), &ctx);
        app.panes[0].tabs.push(background);
        for pane in &mut app.panes {
            for tab in &mut pane.tabs {
                wait_for_directory(tab);
                tab.next_auto_refresh = Instant::now() - AUTO_REFRESH_INTERVAL;
            }
        }
        let pointer = egui::Event::PointerButton {
            pos: egui::pos2(100.0, 100.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        let _ = ctx.run(
            egui::RawInput {
                events: vec![pointer],
                ..Default::default()
            },
            |ctx| app.poll(ctx),
        );
        assert!(
            app.panes[0].tab().loading.is_none(),
            "held pointer postpones automatic read"
        );
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::PointerButton {
                    pos: egui::pos2(100.0, 100.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                }],
                ..Default::default()
            },
            |_| {},
        );
        // Start in a fresh input frame with no held/released pointer.
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.poll(ctx));
        assert!(app.panes[0].tabs[0].automatic_loading);
        assert!(
            app.panes[0].tabs[1].loading.is_none(),
            "inactive tab stays idle"
        );
        assert!(
            app.panes[1].tab().loading.is_none(),
            "hidden pane stays idle"
        );
        wait_for_directory(app.panes[0].tab_mut());
        app.settings.split = true;
        app.poll(&ctx);
        assert!(
            app.panes[1].tab().automatic_loading,
            "displayed pane resumes scanning"
        );
        wait_for_directory(app.panes[1].tab_mut());
    }

    #[test]
    fn automatic_refresh_preserves_egui_scroll_in_list_and_grid() {
        fn draw(
            ctx: &egui::Context,
            pane: &mut Pane,
            settings: &Settings,
        ) -> Vec<(String, egui::Pos2)> {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(640.0, 550.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        render_pane(
                            ui,
                            pane,
                            PaneOptions {
                                index: 0,
                                active: true,
                                settings,
                                idle: true,
                                zed_available: false,
                            },
                            &mut vec![],
                            &mut DropTargets::default(),
                            &mut TabDropTargets::default(),
                        );
                    });
                },
            );
            output
                .shapes
                .into_iter()
                .filter_map(|clipped| match clipped.shape {
                    egui::Shape::Text(text) if text.galley.text().starts_with("file") => {
                        Some((text.galley.text().to_owned(), text.pos))
                    }
                    _ => None,
                })
                .collect()
        }
        for grid in [false, true] {
            let root = tempfile::tempdir().unwrap();
            for i in 0..100 {
                std::fs::write(root.path().join(format!("file{i:03}")), "content").unwrap();
            }
            let ctx = egui::Context::default();
            let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
            wait_for_directory(&mut tab);
            let selected = tab.path.join("file035");
            tab.selected.insert(selected.clone());
            let mut pane = Pane {
                tabs: vec![tab],
                current: 0,
            };
            let settings = Settings {
                grid,
                ..Default::default()
            };
            draw(&ctx, &mut pane, &settings);
            pane.tab_mut().scroll_to = Some(selected.clone());
            draw(&ctx, &mut pane, &settings);
            let before = draw(&ctx, &mut pane, &settings);
            assert!(
                before.iter().any(|(name, _)| name == "file035"),
                "scrolled middle row is rendered"
            );
            assert!(
                !before.iter().any(|(name, _)| name == "file000"),
                "test must start below the top"
            );
            std::fs::write(root.path().join("file999"), "external").unwrap();
            pane.tab_mut()
                .auto_refresh(&ctx, Instant::now() + AUTO_REFRESH_INTERVAL);
            assert_eq!(
                draw(&ctx, &mut pane, &settings),
                before,
                "no layout shift while reading"
            );
            wait_for_directory(pane.tab_mut());
            assert_eq!(
                draw(&ctx, &mut pane, &settings),
                before,
                "refresh retains pixel offset and rows"
            );
            assert!(pane.tab().selected.contains(&selected));
            assert!(pane.tab().scroll_to.is_none());
        }
    }

    #[derive(Default)]
    struct MemoryStorage(std::collections::HashMap<String, String>);
    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.into(), value);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn operation_center_stays_live_during_a_stalled_copy_and_reports_the_error() {
        use std::io::{self, Read};

        struct SlowSource {
            first: bool,
            waiting: mpsc::Sender<()>,
            release: Receiver<()>,
        }

        impl Read for SlowSource {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    // Simulate enough elapsed I/O time to get a first rate estimate.
                    std::thread::sleep(Duration::from_millis(120));
                    buffer[..4].copy_from_slice(b"part");
                    return Ok(4);
                }
                self.waiting.send(()).unwrap();
                let _ = self.release.recv();
                Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "test disconnect",
                ))
            }
        }

        fn draw_footer(app: &mut PawCommander, ctx: &egui::Context) -> String {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    app.poll(ctx);
                    app.footer(ctx);
                },
            )
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
        }

        let root = tempfile::tempdir().unwrap();
        let other = root.path().join("another folder");
        std::fs::create_dir(&other).unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let (waiting, blocked) = mpsc::channel();
        let (resume, release) = mpsc::channel();
        app.start_progress_job(
            &ctx,
            "Копирование файлов",
            move |progress| {
                let mut input = SlowSource {
                    first: true,
                    waiting,
                    release,
                };
                progress
                    .copy(Path::new("file.txt"), Some(16), &mut input, &mut io::sink())
                    .map(|_| "Готово".into())
                    .map_err(|error| error.to_string())
            },
        );
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        // The bottom panel settles its measured height after its first frame.
        draw_footer(&mut app, &ctx);
        let active = draw_footer(&mut app, &ctx);
        assert!(active.contains("Current item: file.txt"), "{active}");
        assert!(active.contains("4 B / 16 B"), "{active}");
        assert!(active.contains("B/s"), "{active}");
        assert!(active.contains("File remaining: ≈"), "{active}");
        app.action(Action::Navigate(0, other.clone().into()), &ctx);
        assert_eq!(app.tab().path.path(), other);
        assert!(app.job.is_some());

        resume.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.job.is_some() && Instant::now() < deadline {
            app.poll(&ctx);
            std::thread::yield_now();
        }
        assert!(app.job.is_none());
        assert!(draw_footer(&mut app, &ctx).contains("test disconnect"));
    }

    #[test]
    fn pointer_drag_on_tabs_reorders_transfers_and_cancels() {
        fn frame(
            ctx: &egui::Context,
            panes: &mut [Pane; 2],
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(940.0, 200.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    let mut file_targets = DropTargets::default();
                    let mut tab_targets = TabDropTargets::default();
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.columns(2, |cols| {
                            for (i, ui) in cols.iter_mut().enumerate() {
                                tab_strip(
                                    ui,
                                    &mut panes[i],
                                    i,
                                    &mut vec![],
                                    &mut file_targets,
                                    &mut tab_targets,
                                    true,
                                );
                            }
                        });
                    });
                    if let Some(drop) = tab_targets.finish(ctx, true) {
                        move_tab(panes, drop, ctx);
                    }
                },
            )
        }

        fn label(output: &egui::FullOutput, name: &str) -> egui::Pos2 {
            output
                .shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) if text.galley.text() == name => {
                        Some(text.pos + text.galley.size() * 0.5)
                    }
                    _ => None,
                })
                .unwrap()
        }

        fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }
        }
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let make_tab = |name| Tab::new(dir.path().join(name).into(), &ctx);
        let mut panes = [
            Pane {
                tabs: vec![make_tab("alpha"), make_tab("beta")],
                current: 0,
            },
            Pane {
                tabs: vec![make_tab("gamma")],
                current: 0,
            },
        ];
        let alpha_id = panes[0].tab().id;
        let output = frame(&ctx, &mut panes, vec![]);
        let alpha = label(&output, "alpha");
        let after_beta = label(&output, "beta") + egui::vec2(43.0, 0.0);
        frame(
            &ctx,
            &mut panes,
            vec![egui::Event::PointerMoved(alpha), button(alpha, true)],
        );
        frame(
            &ctx,
            &mut panes,
            vec![egui::Event::PointerMoved(after_beta)],
        );
        let output = frame(&ctx, &mut panes, vec![button(after_beta, false)]);
        assert_eq!(panes[0].tabs[1].id, alpha_id);

        let alpha = label(&output, "alpha");
        let gamma = label(&output, "gamma");
        let before_gamma = gamma - egui::vec2(24.0, 0.0);
        frame(
            &ctx,
            &mut panes,
            vec![egui::Event::PointerMoved(alpha), button(alpha, true)],
        );
        frame(
            &ctx,
            &mut panes,
            vec![egui::Event::PointerMoved(before_gamma)],
        );
        let output = frame(&ctx, &mut panes, vec![button(before_gamma, false)]);
        assert_eq!(panes[0].tabs.len(), 1);
        assert_eq!(panes[1].tabs[0].id, alpha_id);

        let alpha = label(&output, "alpha");
        let beta = label(&output, "beta");
        frame(
            &ctx,
            &mut panes,
            vec![egui::Event::PointerMoved(alpha), button(alpha, true)],
        );
        frame(&ctx, &mut panes, vec![egui::Event::PointerMoved(beta)]);
        frame(
            &ctx,
            &mut panes,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        frame(&ctx, &mut panes, vec![button(beta, false)]);
        assert_eq!(panes[0].tabs.len(), 1);
        assert_eq!(panes[1].tabs[0].id, alpha_id);
    }

    #[test]
    fn tab_moves_keep_the_tab_state_and_leave_both_panes_usable() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let path: Location = root.path().to_path_buf().into();
        let mut moved = Tab::new(path.clone(), &ctx);
        let id = moved.id;
        moved.filter = "report".into();
        moved.selected.insert(path.join("report.txt"));
        moved.history.insert(0, path.join("previous"));
        moved.history_index = 1;
        let other = Tab::new(path.clone(), &ctx);
        let other_id = other.id;
        let mut panes = [
            Pane {
                tabs: vec![moved, other],
                current: 1,
            },
            Pane {
                tabs: vec![Tab::new(path.clone(), &ctx)],
                current: 0,
            },
        ];
        assert!(move_tab(
            &mut panes,
            TabDrop {
                source: 0,
                id,
                destination: 0,
                slot: 2
            },
            &ctx
        ));
        assert_eq!(panes[0].tabs[0].id, other_id);
        assert_eq!(panes[0].tab().id, id);
        assert!(!move_tab(
            &mut panes,
            TabDrop {
                source: 0,
                id,
                destination: 0,
                slot: 1
            },
            &ctx
        ));

        assert!(move_tab(
            &mut panes,
            TabDrop {
                source: 0,
                id,
                destination: 1,
                slot: 0
            },
            &ctx
        ));
        assert_eq!(panes[0].tab().id, other_id);
        assert_eq!(panes[1].tab().id, id);
        assert_eq!(panes[1].tab().filter, "report");
        assert!(panes[1].tab().selected.contains(&path.join("report.txt")));
        assert_eq!(panes[1].tab().history[0], path.join("previous"));
        assert_eq!(panes[1].tab().history_index, 1);
        assert!(move_tab(
            &mut panes,
            TabDrop {
                source: 0,
                id: other_id,
                destination: 1,
                slot: 2
            },
            &ctx
        ));
        assert_eq!(panes[0].tabs.len(), 1);
        assert_ne!(panes[0].tab().id, other_id);
        assert_eq!(panes[1].tab().id, other_id);
        assert_eq!(panes[1].tabs.len(), 3);
    }

    #[test]
    fn pane_views_migrate_and_persist_independently() {
        let mut settings = Settings {
            grid: true,
            ..Default::default()
        };
        assert!(settings.grid_for(0) && settings.grid_for(1));
        settings.set_grid(0, false);
        let mut storage = MemoryStorage::default();
        eframe::set_value(&mut storage, "views", &settings);
        let restored: Settings = eframe::get_value(&storage, "views").unwrap();
        assert!(!restored.grid_for(0));
        assert!(restored.grid_for(1));
    }

    #[test]
    fn address_editor_submits_typed_paths_completes_folders_and_cancels() {
        let ctx = egui::Context::default();
        Theme::Frappe.apply(&ctx);
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("Two Paths")).unwrap();
        std::fs::create_dir_all(root.path().join("Two Words/Nested folder")).unwrap();
        std::fs::write(root.path().join("Two Before Folders.txt"), "").unwrap();
        let original: Location = root.path().to_path_buf().into();
        let mut tab = Tab::new(original.clone(), &ctx);
        tab.begin_address();
        let mut time = 0.0;
        let mut render = |tab: &mut Tab, events| {
            time += 0.1;
            let mut actions = vec![];
            let _ = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(440.0, 200.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        address_bar(ui, tab, 1, &mut actions, &mut DropTargets::default());
                    });
                },
            );
            actions
        };
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        render(&mut tab, vec![]);
        render(&mut tab, vec![egui::Event::Paste("two words".into())]);
        assert_eq!(
            tab.address, "two words",
            "editing starts with the full path selected"
        );
        let actions = render(&mut tab, vec![key(egui::Key::Enter)]);
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::Navigate(1, p) if *p == original.join("two words"))));
        assert!(!tab.edit_address);

        let wait_for_suggestions =
            |tab: &mut Tab, render: &mut dyn FnMut(&mut Tab, Vec<_>) -> _| {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !ctx.memory(|memory| memory.any_popup_open()) {
                    assert!(Instant::now() < deadline, "expected folder suggestions");
                    std::thread::sleep(Duration::from_millis(5));
                    render(tab, vec![]);
                }
            };
        tab.begin_address();
        render(&mut tab, vec![]);
        render(&mut tab, vec![egui::Event::Paste("two".into())]);
        wait_for_suggestions(&mut tab, &mut render);
        render(&mut tab, vec![key(egui::Key::ArrowDown)]);
        render(&mut tab, vec![key(egui::Key::ArrowDown)]);
        let actions = render(&mut tab, vec![key(egui::Key::Tab)]);
        assert!(tab.edit_address);
        assert_eq!(
            tab.address,
            format!(
                "{}{}",
                original.join("Two Words").display(),
                std::path::MAIN_SEPARATOR
            )
        );
        assert!(!actions.iter().any(|a| matches!(a, Action::Navigate(_, _))));
        wait_for_suggestions(&mut tab, &mut render);
        render(&mut tab, vec![key(egui::Key::ArrowDown)]);
        let actions = render(&mut tab, vec![key(egui::Key::Enter)]);
        assert!(actions.iter().any(|a| matches!(
            a, Action::Navigate(1, p) if *p == original.join("Two Words").join("Nested folder")
        )));
        assert!(!tab.edit_address);

        tab.begin_address();
        render(&mut tab, vec![]);
        render(
            &mut tab,
            vec![egui::Event::Paste("discard this edit".into())],
        );
        let actions = render(&mut tab, vec![key(egui::Key::Escape)]);
        assert!(!tab.edit_address);
        assert_eq!(tab.address, original.display());
        assert!(!actions.iter().any(|a| matches!(a, Action::Navigate(_, _))));
    }

    #[test]
    fn settings_preserve_remote_tabs_and_read_legacy_local_paths() {
        let mut storage = MemoryStorage::default();
        #[derive(Serialize)]
        struct Legacy {
            locations: Vec<Vec<PathBuf>>,
            bookmarks: Vec<PathBuf>,
        }
        eframe::set_value(
            &mut storage,
            "legacy",
            &Legacy {
                locations: vec![vec![PathBuf::from("/tmp/two words")]],
                bookmarks: vec![PathBuf::from("/tmp")],
            },
        );
        let legacy: Settings = eframe::get_value(&storage, "legacy").expect("legacy settings");
        assert_eq!(
            legacy.locations[0][0],
            Location::Local(PathBuf::from("/tmp/two words"))
        );
        assert_eq!(legacy.theme, Theme::Frappe);
        let remote =
            fs_ops::resolve("sftp://alice@example.com:2222/папка", &legacy.bookmarks[0]).unwrap();
        let settings = Settings {
            locations: vec![vec![remote.clone()]],
            bookmarks: vec![remote.clone()],
            theme: Theme::Dark,
            sftp_profiles: vec![remote.clone()],
            ..Default::default()
        };
        eframe::set_value(&mut storage, "new", &settings);
        let restored: Settings = eframe::get_value(&storage, "new").expect("SFTP settings");
        assert_eq!(restored.locations[0][0], remote);
        assert_eq!(restored.bookmarks[0], remote);
        assert_eq!(restored.theme, Theme::Dark);
        assert_eq!(restored.sftp_profiles, vec![remote]);
    }
    #[test]
    fn remote_tab_history_and_new_tab_keep_backend() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let local = Location::Local(root.path().into());
        let remote = Location::Sftp {
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 0,
                user: "test".into(),
                key: None,
            },
            path: "/".into(),
        };
        let mut tab = Tab::new(local.clone(), &ctx);
        tab.navigate(remote.clone(), &ctx);
        tab.navigate(remote.join("sub"), &ctx);
        tab.history_go(-1, &ctx);
        assert_eq!(tab.path, remote);
        tab.history_go(-1, &ctx);
        assert_eq!(tab.path, local);
        tab.history_go(1, &ctx);
        let duplicate = Tab::new(tab.path.clone(), &ctx);
        assert_eq!(duplicate.path, remote);
        assert_eq!(duplicate.address, remote.display());
    }
    #[test]
    fn locate_result_is_selected_after_loading_and_stale_result_is_reported() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".hidden файл");
        std::fs::write(&path, "test").unwrap();
        let location: Location = path.clone().into();
        let ctx = egui::Context::default();
        let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
        let (tx, rx) = mpsc::channel();
        tab.loading = Some(rx);
        tab.reveal_after_load = Some(location.clone());
        tx.send(fs_ops::read_directory(&tab.path)).unwrap();
        tab.poll();
        assert_eq!(tab.selected, BTreeSet::from([location.clone()]));
        assert_eq!(tab.cursor, Some(location.clone()));
        assert_eq!(tab.scroll_to, Some(location.clone()));
        assert!(tab.reveal_after_load.is_none());
        assert!(tab
            .visible(true)
            .iter()
            .any(|i| tab.entries[*i].path == location));

        std::fs::remove_file(path).unwrap();
        let (tx, rx) = mpsc::channel();
        tab.loading = Some(rx);
        tab.reveal_after_load = Some(location);
        tx.send(fs_ops::read_directory(&tab.path)).unwrap();
        tab.poll();
        assert!(tab.selected.is_empty());
        assert!(tab.error.unwrap().contains("index may be outdated"));
    }

    #[test]
    fn relative_address_preserves_spaces() {
        assert_eq!(
            resolve_address("two words", &PathBuf::from("/tmp").into()).unwrap(),
            Location::Local(PathBuf::from("/tmp/two words"))
        );
    }
    #[test]
    fn sftp_file_double_click_and_zed_context_menu_keep_the_clicked_path() {
        fn text_rect(shape: &egui::epaint::Shape) -> Option<egui::Rect> {
            match shape {
                egui::epaint::Shape::Text(text) if text.galley.job.text == "Open in Zed" => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()))
                }
                egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(text_rect),
                _ => None,
            }
        }
        for (context_menu, zed_available, checkbox) in [
            (false, true, false),
            (true, true, false),
            (true, false, false),
            (false, true, true),
        ] {
            let ctx = egui::Context::default();
            let root = tempfile::tempdir().unwrap();
            let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
            tab.loading = None;
            let path = Location::Sftp {
                endpoint: Endpoint {
                    host: "example.invalid".into(),
                    port: 2222,
                    user: "test".into(),
                    key: None,
                },
                path: "/project/.env".into(),
            };
            tab.entries = vec![FileEntry {
                path: path.clone(),
                name: ".env".into(),
                is_dir: false,
                is_symlink: false,
                size: 12,
                modified: None,
            }];
            let mut time = 0.0;
            let mut render = |events| {
                let mut actions = vec![];
                time += 0.02;
                let output = ctx.run(
                    egui::RawInput {
                        time: Some(time),
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(600.0, 400.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            entry_widget(
                                ui,
                                &mut tab,
                                0,
                                &[0],
                                EntryOptions {
                                    pane: 1,
                                    split: true,
                                    grid: false,
                                    idle: true,
                                    zed_available,
                                },
                                &mut actions,
                                &mut DropTargets::default(),
                            );
                        });
                    },
                );
                (actions, output)
            };
            // The leading checkbox toggles selection; double-click the filename to open.
            let pos = egui::pos2(if checkbox { 24.0 } else { 90.0 }, 24.0);
            let button = |button, pressed, pos| egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            render(vec![egui::Event::PointerMoved(pos)]);
            if context_menu {
                render(vec![button(egui::PointerButton::Secondary, true, pos)]);
                render(vec![button(egui::PointerButton::Secondary, false, pos)]);
                let (_, output) = render(vec![]);
                let rect = output
                    .shapes
                    .iter()
                    .find_map(|s| text_rect(&s.shape))
                    .expect("file menu exposes Open in Zed");
                let menu_pos = rect.center();
                render(vec![
                    egui::Event::PointerMoved(menu_pos),
                    button(egui::PointerButton::Primary, true, menu_pos),
                ]);
                let (actions, _) =
                    render(vec![button(egui::PointerButton::Primary, false, menu_pos)]);
                assert!(
                    actions
                        .iter()
                        .any(|a| matches!(a, Action::OpenInZed(1, p) if p == &path))
                        == zed_available
                );
            } else {
                render(vec![button(egui::PointerButton::Primary, true, pos)]);
                render(vec![button(egui::PointerButton::Primary, false, pos)]);
                render(vec![button(egui::PointerButton::Primary, true, pos)]);
                let (actions, _) = render(vec![button(egui::PointerButton::Primary, false, pos)]);
                assert_eq!(
                    actions
                        .iter()
                        .any(|a| matches!(a, Action::Open(1, p) if p == &path)),
                    !checkbox
                );
                if checkbox {
                    assert!(
                        tab.selected.is_empty(),
                        "two checkbox clicks toggle selection on and off"
                    );
                }
            }
        }
    }

    #[test]
    fn dragging_rows_and_tiles_preserves_visible_group_and_transfers_files() {
        for grid in [false, true] {
            let ctx = egui::Context::default();
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source");
            let destination = root.path().join("destination");
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&destination).unwrap();
            for name in ["a.txt", "b.txt", ".hidden"] {
                std::fs::write(source.join(name), name).unwrap();
            }
            let mut tab = Tab::new(source.into(), &ctx);
            tab.loading = None;
            tab.entries = fs_ops::read_directory(&tab.path).unwrap();
            tab.selected = tab.entries.iter().map(|e| e.path.clone()).collect();
            let mut source_pos = egui::Pos2::ZERO;
            let mut target_pos = egui::Pos2::ZERO;
            let mut run = |events| {
                let mut drop = None;
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(800.0, 500.0),
                    )),
                    events,
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    let mut targets = DropTargets::default();
                    let mut actions = vec![];
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.columns(2, |cols| {
                            let target_rect = cols[0].available_rect_before_wrap();
                            target_pos = target_rect.center();
                            targets.add(
                                &cols[0],
                                target_rect,
                                0,
                                &destination.clone().into(),
                                true,
                            );
                            source_pos =
                                cols[1].available_rect_before_wrap().min + Vec2::splat(10.0);
                            let visible = tab.visible(false);
                            entry_widget(
                                &mut cols[1],
                                &mut tab,
                                visible[0],
                                &visible,
                                EntryOptions {
                                    pane: 1,
                                    split: true,
                                    grid,
                                    idle: true,
                                    zed_available: true,
                                },
                                &mut actions,
                                &mut targets,
                            );
                        });
                    });
                    drop = targets.finish(ctx, true);
                });
                (source_pos, target_pos, drop)
            };
            let (start, end, _) = run(vec![]);
            run(vec![egui::Event::PointerMoved(start)]);
            run(vec![egui::Event::PointerButton {
                pos: start,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }]);
            run(vec![egui::Event::PointerMoved(end)]);
            let (_, _, result) = run(vec![egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            let request = result.expect("actual entry widget starts a drag").unwrap();
            assert_eq!(
                request.sources.len(),
                2,
                "hidden selection must not be dragged"
            );
            assert_eq!(request.destination, Location::Local(destination.clone()));
            assert_eq!(request.kind, TransferKind::Copy);
            fs_ops::transfer(&request.sources, &request.destination, request.kind).unwrap();
            for name in ["a.txt", "b.txt"] {
                assert_eq!(
                    std::fs::read_to_string(destination.join(name)).unwrap(),
                    name
                );
                assert!(tab.path.join(name).path().exists());
            }
            assert!(!destination.join(".hidden").exists());
            // Dragging an unselected item replaces the group without interpreting
            // Shift (which controls the drop operation) as range selection.
            tab.selected.clear();
            let visible = tab.visible(false);
            let paths = tab.drag_paths(visible[0], &visible);
            assert_eq!(paths.len(), 1);
            assert_eq!(tab.selected.len(), 1);
        }
    }

    #[test]
    fn filters_sort_and_range_selection() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
        tab.entries = [
            ("z.txt", false),
            ("B", true),
            ("a.txt", false),
            (".hidden", false),
        ]
        .into_iter()
        .map(|(name, is_dir)| FileEntry {
            path: root.path().join(name).into(),
            name: name.into(),
            is_dir,
            is_symlink: false,
            size: 1,
            modified: None,
        })
        .collect();
        let visible = tab.visible(false);
        assert_eq!(visible, vec![1, 2, 0]);
        tab.choose(1, &visible, egui::Modifiers::NONE);
        tab.choose(0, &visible, egui::Modifiers::SHIFT);
        assert_eq!(tab.selected.len(), 3);
        tab.filter = "TXT".into();
        assert_eq!(tab.visible(false), vec![2, 0]);
        tab.descending = true;
        tab.filter.clear();
        assert_eq!(tab.visible(false), vec![1, 0, 2]);
    }
}
