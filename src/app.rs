mod column_view;
mod quick_look;
mod selection;

use crate::command_palette::{CommandPalette, PaletteOutcome};
use crate::conflict::{Answer, ConflictDialog, Decision};
use crate::directory_tree::DirectoryTree;
use crate::drag_drop::{DragFiles, DropRequest, DropTargets};
use crate::file_clipboard::{FileList, SystemClipboard};
use crate::folder_jump::{FolderHistory, FolderJump, JumpOutcome};
use crate::fs_ops::TransferKind;
use crate::icons::{self, Icon};
use crate::list_columns::{Column, ColumnLayout, ListColumns};
use crate::operations::{Cancellation, Progress, ProgressView};
use crate::preview::Preview;
use crate::result_history::History;
use crate::search::panel::{ReadyAction, TabTarget};
use crate::shortcuts::{self, Command, ShortcutEditor, Shortcuts};
use crate::tab_drag::{clamp_tab_slot, DragTab, TabDrop, TabDropTargets};
use crate::theme::{appearance_menu, paint_focus, Palette, Theme};
use crate::thumbnails::Thumbnails;
use crate::vfs::{self as fs_ops, Endpoint, FileEntry, Location};
use eframe::egui::{self, Color32, FontId, RichText, Sense, Stroke, Vec2};
use quick_look::QuickLook;
use selection::{EntryIdentity, Rule, RuleKind};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static NEXT_BOOKMARK_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    locations: Vec<Vec<Location>>,
    pinned_tab_counts: [usize; 2],
    bookmarks: Vec<Location>,
    folder_history: FolderHistory,
    split: bool,
    pane_layout: PaneLayout,
    preview: bool,
    verify_copies: bool,
    hidden: bool,
    grid: bool,
    pane_grid: Option<[bool; 2]>,
    list_columns: ListColumns,
    theme: Theme,
    ui_scale: u16,
    high_contrast: bool,
    language: crate::i18n::Language,
    sftp_profiles: Vec<Location>,
    shortcuts: Shortcuts,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum PaneOrientation {
    #[default]
    SideBySide,
    Stacked,
}

impl PaneOrientation {
    fn axis(self) -> usize {
        match self {
            Self::SideBySide => 0,
            Self::Stacked => 1,
        }
    }

    fn label(self) -> &'static str {
        crate::i18n::text(match self {
            Self::SideBySide => "Рядом",
            Self::Stacked => "Одна над другой",
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct PaneLayout {
    orientation: PaneOrientation,
    fractions: [f32; 2],
    sidebar_visible: bool,
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self {
            orientation: PaneOrientation::default(),
            fractions: [0.5; 2],
            sidebar_visible: true,
        }
    }
}

impl PaneLayout {
    fn rects(&mut self, ui: &mut egui::Ui, workspace: egui::Rect) -> [egui::Rect; 2] {
        let axis = self.orientation.axis();
        let extent = workspace.size()[axis];
        let gap = extent.min(8.0);
        let usable = extent - gap;
        // Keep both panes usable when space allows. On smaller workspaces,
        // reserve most of the available space for the panes equally.
        let minimum = [180.0_f32, 220.0][axis].min(usable * 0.45);
        let fraction = &mut self.fractions[axis];
        if !fraction.is_finite() {
            *fraction = 0.5;
        }
        let mut first = (usable * *fraction).clamp(minimum, usable - minimum);
        let mut divider = workspace;
        divider.min[axis] += first;
        divider.max[axis] = divider.min[axis] + gap;
        let response = ui.interact(divider, ui.id().with(("pane_divider", axis)), Sense::DRAG);
        let primary = egui::PointerButton::Primary;
        let grab_id = response.id.with("grab_offset");
        if response.drag_started_by(primary) {
            if let Some(origin) = ui.input(|input| input.pointer.press_origin()) {
                ui.data_mut(|data| {
                    data.insert_temp(grab_id, origin[axis] - divider.center()[axis]);
                });
            }
        }
        if usable > 0.0 && (response.dragged_by(primary) || response.drag_stopped_by(primary)) {
            let grab_offset = ui.data(|data| data.get_temp::<f32>(grab_id));
            if let (Some(pointer), Some(grab_offset)) =
                (response.interact_pointer_pos(), grab_offset)
            {
                let proposed = pointer[axis] - grab_offset - workspace.min[axis] - gap * 0.5;
                let resized = proposed.clamp(minimum, usable - minimum);
                if (resized - first).abs() >= 0.5 {
                    first = resized;
                    *fraction = first / usable;
                    divider.min[axis] = workspace.min[axis] + first;
                    divider.max[axis] = divider.min[axis] + gap;
                }
            }
        }
        if response.drag_stopped() {
            ui.data_mut(|data| data.remove::<f32>(grab_id));
        }
        let cursor = if axis == 0 {
            egui::CursorIcon::ResizeHorizontal
        } else {
            egui::CursorIcon::ResizeVertical
        };
        let response = response.on_hover_and_drag_cursor(cursor);
        let colors = Palette::from_ctx(ui.ctx());
        let stroke = Stroke::new(
            if response.hovered() || response.dragged() {
                2.0
            } else {
                1.0
            },
            if response.hovered() || response.dragged() {
                colors.blue
            } else {
                workspace_border(colors)
            },
        );
        if axis == 0 {
            ui.painter()
                .vline(divider.center().x, divider.y_range(), stroke);
        } else {
            ui.painter()
                .hline(divider.x_range(), divider.center().y, stroke);
        }
        let mut panes = [workspace; 2];
        panes[0].max[axis] = divider.min[axis];
        panes[1].min[axis] = divider.max[axis];
        panes
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Bookmark {
    id: u64,
    location: Location,
    name: String,
    group: String,
    shortcut: Option<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedBookmark {
    #[serde(default)]
    id: u64,
    location: SavedLocation,
    name: String,
    group: String,
    shortcut: Option<u8>,
}

impl From<&Bookmark> for SavedBookmark {
    fn from(bookmark: &Bookmark) -> Self {
        Self {
            id: bookmark.id,
            location: SavedLocation::from(&bookmark.location),
            name: bookmark.name.clone(),
            group: bookmark.group.clone(),
            shortcut: bookmark.shortcut,
        }
    }
}

impl SavedBookmark {
    fn into_bookmark(self) -> Option<Bookmark> {
        let location = self.location.into_location()?;
        Some(Bookmark {
            id: if self.id == 0 {
                next_bookmark_id()
            } else {
                self.id
            },
            name: if self.name.trim().is_empty() {
                file_name(&location)
            } else {
                self.name
            },
            location,
            group: self.group,
            shortcut: self.shortcut.filter(|slot| (1..=9).contains(slot)),
        })
    }
}

impl Bookmark {
    fn new(location: Location) -> Self {
        Self {
            id: next_bookmark_id(),
            name: file_name(&location),
            location,
            group: String::new(),
            shortcut: None,
        }
    }
}

fn next_bookmark_id() -> u64 {
    NEXT_BOOKMARK_ID.fetch_add(1, Ordering::Relaxed)
}

fn reserve_bookmark_ids(bookmarks: &[Bookmark]) {
    let next = bookmarks
        .iter()
        .map(|bookmark| bookmark.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    NEXT_BOOKMARK_ID.fetch_max(next, Ordering::Relaxed);
}

struct BookmarkEditor {
    id: u64,
    name: String,
    group: String,
    focus_name: bool,
}

fn apply_bookmark_edit(bookmarks: &mut [Bookmark], id: u64, name: &str, group: &str) -> bool {
    let Some(bookmark) = bookmarks.iter_mut().find(|bookmark| bookmark.id == id) else {
        return false;
    };
    bookmark.name = if name.trim().is_empty() {
        file_name(&bookmark.location)
    } else {
        name.trim().to_owned()
    };
    bookmark.group = group.trim().to_owned();
    true
}

impl BookmarkEditor {
    fn new(bookmark: &Bookmark) -> Self {
        Self {
            id: bookmark.id,
            name: bookmark.name.clone(),
            group: bookmark.group.clone(),
            focus_name: true,
        }
    }

    fn show(&mut self, ctx: &egui::Context, bookmarks: &mut [Bookmark]) -> bool {
        let response = egui::Modal::new(egui::Id::new("bookmark_editor")).show(ctx, |ui| {
            ui.heading(crate::i18n::text("Изменить избранное"));
            ui.label(crate::i18n::text("Название"));
            let name = ui.text_edit_singleline(&mut self.name);
            if self.focus_name && !ui.is_sizing_pass() {
                name.request_focus();
                self.focus_name = false;
            }
            ui.label(crate::i18n::text("Группа"));
            ui.text_edit_singleline(&mut self.group);
            let mut done = false;
            ui.horizontal(|ui| {
                if ui.button(crate::i18n::text("Сохранить")).clicked() {
                    apply_bookmark_edit(bookmarks, self.id, &self.name, &self.group);
                    done = true;
                }
                if ui.button(crate::i18n::text("Отмена")).clicked() {
                    done = true;
                }
            });
            done
        });
        response.inner || response.should_close()
    }
}

fn bookmark_slot_key(slot: u8) -> Option<egui::Key> {
    match slot {
        1 => Some(egui::Key::Num1),
        2 => Some(egui::Key::Num2),
        3 => Some(egui::Key::Num3),
        4 => Some(egui::Key::Num4),
        5 => Some(egui::Key::Num5),
        6 => Some(egui::Key::Num6),
        7 => Some(egui::Key::Num7),
        8 => Some(egui::Key::Num8),
        9 => Some(egui::Key::Num9),
        _ => None,
    }
}

fn assign_bookmark_shortcut(
    bookmarks: &mut [Bookmark],
    index: usize,
    slot: Option<u8>,
    shortcuts: &Shortcuts,
) -> bool {
    if index >= bookmarks.len() {
        return false;
    }
    if let Some(slot) = slot {
        let Some(key) = bookmark_slot_key(slot) else {
            return false;
        };
        let chord = shortcuts::chord(key, egui::Modifiers::ALT);
        if shortcuts.command(chord).is_some()
            || bookmarks
                .iter()
                .enumerate()
                .any(|(other, item)| other != index && item.shortcut == Some(slot))
        {
            return false;
        }
    }
    bookmarks[index].shortcut = slot;
    true
}

fn move_bookmark(bookmarks: &mut [Bookmark], from: usize, to: usize) -> bool {
    if from >= bookmarks.len() || to >= bookmarks.len() || from == to {
        return false;
    }
    bookmarks.swap(from, to);
    true
}

fn bookmark_peer(bookmarks: &[Bookmark], index: usize, direction: isize) -> Option<usize> {
    let bookmark = bookmarks.get(index)?;
    let mut next = index as isize + direction;
    while (0..bookmarks.len() as isize).contains(&next) {
        let candidate = &bookmarks[next as usize];
        if candidate.group.trim() == bookmark.group.trim() {
            return Some(next as usize);
        }
        next += direction;
    }
    None
}

fn validate_bookmark_shortcuts(bookmarks: &mut [Bookmark], shortcuts: &Shortcuts) -> bool {
    let mut changed = false;
    let mut used = HashSet::new();
    for bookmark in bookmarks {
        let Some(slot) = bookmark.shortcut else {
            continue;
        };
        let chord = bookmark_slot_key(slot).map(|key| shortcuts::chord(key, egui::Modifiers::ALT));
        if chord.is_none_or(|chord| shortcuts.command(chord).is_some()) || !used.insert(slot) {
            bookmark.shortcut = None;
            changed = true;
        }
    }
    changed
}
impl Settings {
    const UI_SCALES: [u16; 5] = [90, 100, 110, 125, 150];

    fn set_ui_scale(&mut self, ctx: &egui::Context, scale: u16) {
        self.ui_scale = if Self::UI_SCALES.contains(&scale) {
            scale
        } else {
            100
        };
        ctx.set_zoom_factor(f32::from(self.ui_scale) / 100.0);
    }

    fn grid_for(&self, index: usize) -> bool {
        self.pane_grid.unwrap_or([self.grid; 2])[index]
    }
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            locations: vec![],
            pinned_tab_counts: [0; 2],
            bookmarks: vec![],
            folder_history: FolderHistory::default(),
            split: true,
            pane_layout: PaneLayout::default(),
            preview: true,
            verify_copies: false,
            hidden: false,
            grid: false,
            pane_grid: None,
            list_columns: ListColumns::default(),
            theme: Theme::Frappe,
            ui_scale: 100,
            high_contrast: false,
            language: crate::i18n::Language::default(),
            sftp_profiles: vec![],
            shortcuts: Shortcuts::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum Sort {
    #[default]
    Name,
    Size,
    Modified,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FileOrder {
    key: Sort,
    descending: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum Density {
    #[default]
    Standard,
    Compact,
}

impl Density {
    fn label(self) -> &'static str {
        crate::i18n::text(match self {
            Self::Standard => "Обычная",
            Self::Compact => "Компактная",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FolderView {
    mode: FolderMode,
    hidden: bool,
    density: Density,
    order: FileOrder,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum FolderMode {
    #[default]
    List,
    Grid,
    Columns,
}

impl FolderMode {
    fn from_grid(grid: bool) -> Self {
        if grid {
            Self::Grid
        } else {
            Self::List
        }
    }

    fn label(self) -> &'static str {
        crate::i18n::text(match self {
            Self::List => "Список",
            Self::Grid => "Сетка",
            Self::Columns => "Колоночный режим",
        })
    }

    fn icon(self) -> Icon {
        match self {
            Self::List => Icon::List,
            Self::Grid => Icon::Grid,
            Self::Columns => Icon::FolderColumns,
        }
    }
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
struct FolderViewOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<FolderMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grid: Option<bool>,
    hidden: Option<bool>,
    density: Option<Density>,
    order: Option<FileOrder>,
}

impl FolderViewOverrides {
    fn resolve(&self, defaults: FolderView) -> FolderView {
        FolderView {
            mode: self
                .mode
                .or_else(|| self.grid.map(FolderMode::from_grid))
                .unwrap_or(defaults.mode),
            hidden: self.hidden.unwrap_or(defaults.hidden),
            density: self.density.unwrap_or(defaults.density),
            order: self.order.unwrap_or(defaults.order),
        }
    }
}

type FolderViews = BTreeMap<Location, FolderViewOverrides>;

fn is_dot_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

#[derive(Serialize, Deserialize)]
struct SavedFolderView {
    location: SavedLocation,
    view: FolderViewOverrides,
}

#[derive(Clone, Copy)]
enum FolderViewChange {
    Mode(FolderMode),
    Hidden(bool),
    Density(Density),
    Sort(Sort),
}

#[derive(Serialize, Deserialize)]
struct WorkspaceSession {
    panes: [PaneSession; 2],
    active: usize,
}

#[derive(Serialize, Deserialize)]
struct NamedWorkspace {
    name: String,
    folders: [SavedLocation; 2],
}

#[derive(Serialize, Deserialize)]
struct PaneSession {
    tabs: Vec<TabSession>,
    current: usize,
}

#[derive(Serialize, Deserialize)]
struct TabSession {
    path: SavedLocation,
    pinned: bool,
    history: Vec<SavedLocation>,
    history_index: usize,
    sort: Sort,
    descending: bool,
    scroll_offset: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    browse_columns: bool,
}

// Local paths or SFTP connection coordinates and a key-file reference. Secrets
// belong to SFTP sessions and the credential store, never to saved locations.
#[derive(Clone, Serialize, Deserialize)]
enum SavedLocation {
    Local(SavedPath),
    Sftp {
        host: String,
        port: u16,
        user: String,
        key: Option<SavedPath>,
        path: SavedPath,
    },
    Archive {
        archive: SavedPath,
        path: SavedPath,
    },
}

// Preserve native names that PathBuf's serializer cannot handle.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum SavedPath {
    Utf8(String),
    Unix(Vec<u8>),
    Windows(Vec<u16>),
}

impl From<&Location> for SavedLocation {
    fn from(location: &Location) -> Self {
        match location {
            Location::Local(path) => Self::Local(path.as_path().into()),
            Location::Archive { archive, path } => Self::Archive {
                archive: archive.as_path().into(),
                path: path.as_path().into(),
            },
            Location::Sftp { endpoint, path } => Self::Sftp {
                host: endpoint.host.clone(),
                port: endpoint.port,
                user: endpoint.user.clone(),
                key: endpoint.key.as_deref().map(SavedPath::from),
                path: path.as_path().into(),
            },
        }
    }
}

impl SavedLocation {
    fn into_location(self) -> Option<Location> {
        Some(match self {
            Self::Local(path) => Location::Local(path.into_path()?),
            Self::Archive { archive, path } => Location::Archive {
                archive: Arc::new(archive.into_path()?),
                path: path.into_path()?,
            },
            Self::Sftp {
                host,
                port,
                user,
                key,
                path,
            } => Location::Sftp {
                endpoint: Endpoint {
                    host,
                    port,
                    user,
                    key: match key {
                        Some(key) => Some(key.into_path()?),
                        None => None,
                    },
                },
                path: path.into_path()?,
            },
        })
    }
}

impl From<&Path> for SavedPath {
    fn from(path: &Path) -> Self {
        if let Some(path) = path.to_str() {
            return Self::Utf8(path.to_owned());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::Unix(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::Windows(path.as_os_str().encode_wide().collect())
        }
    }
}

impl SavedPath {
    pub(crate) fn into_path(self) -> Option<PathBuf> {
        match self {
            Self::Utf8(path) => Some(path.into()),
            #[cfg(unix)]
            Self::Unix(bytes) => {
                use std::os::unix::ffi::OsStringExt;
                Some(std::ffi::OsString::from_vec(bytes).into())
            }
            #[cfg(windows)]
            Self::Windows(wide) => {
                use std::os::windows::ffi::OsStringExt;
                Some(std::ffi::OsString::from_wide(&wide).into())
            }
            _ => None,
        }
    }
}
// Poll only displayed directories. Wait from completion to bound work on slow
// filesystems, and poll SFTP less frequently to limit network traffic.
const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SFTP_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirectoryLoadMode {
    Progressive,
    Reload,
    Automatic,
}

#[derive(Clone, Copy)]
enum FolderVisit {
    Unseen,
    CountOnOpen,
    Recorded,
}

enum DirectoryRead {
    Batch(Vec<FileEntry>),
    Finished(Result<Option<Vec<FileEntry>>, String>),
    Archive(Result<(Arc<crate::archive::Index>, Vec<FileEntry>), String>),
}

struct DirectoryLoad {
    receiver: Receiver<DirectoryRead>,
    cancelled: Arc<AtomicBool>,
    mode: DirectoryLoadMode,
}

impl DirectoryLoad {
    fn channel(mode: DirectoryLoadMode) -> (SyncSender<DirectoryRead>, Self) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let load = Self {
            receiver,
            cancelled: Arc::new(AtomicBool::new(false)),
            mode,
        };
        (sender, load)
    }
}

impl Drop for DirectoryLoad {
    fn drop(&mut self) {
        // The receiver also closes, unblocking a producer waiting to send.
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn send_progressive_listing(
    read: impl FnOnce(&mut dyn FnMut(FileEntry)) -> Result<(), String>,
    send: impl Fn(DirectoryRead) -> bool,
) {
    const BATCH_SIZE: usize = 1024;
    const BATCH_INTERVAL: Duration = Duration::from_millis(40);
    let mut batch = Vec::new();
    let mut last_batch = None;
    let result = read(&mut |entry| {
        batch.push(entry);
        // Publish the first entry immediately even if the next read stalls.
        if batch.len() == BATCH_SIZE
            || last_batch.is_none_or(|at: Instant| at.elapsed() >= BATCH_INTERVAL)
        {
            send(DirectoryRead::Batch(std::mem::take(&mut batch)));
            last_batch = Some(Instant::now());
        }
    });
    if batch.is_empty() || send(DirectoryRead::Batch(batch)) {
        send(DirectoryRead::Finished(result.map(|()| None)));
    }
}

#[derive(Default)]
struct DirectoryEntries {
    files: Vec<FileEntry>,
    view: RefCell<DirectoryView>,
}

impl Deref for DirectoryEntries {
    type Target = [FileEntry];

    fn deref(&self) -> &Self::Target {
        &self.files
    }
}

#[derive(Default)]
struct DirectoryView {
    folded_names: Vec<Option<Box<str>>>,
    query: Option<ViewQuery>,
    scanned: usize,
    rows: Rc<Vec<usize>>,
}

struct ViewQuery {
    hidden: bool,
    filter: String,
    sort: Sort,
    descending: bool,
}

impl DirectoryEntries {
    fn replace(&mut self, files: Vec<FileEntry>) {
        self.files = files;
        *self.view.get_mut() = DirectoryView::default();
    }

    fn extend(&mut self, files: Vec<FileEntry>) {
        self.files.extend(files);
    }

    fn visible(&self, hidden: bool, filter: &str, sort: Sort, descending: bool) -> Rc<Vec<usize>> {
        let mut view = self.view.borrow_mut();
        let DirectoryView {
            folded_names,
            query,
            scanned,
            rows,
        } = &mut *view;
        if query.as_ref().is_none_or(|previous| {
            previous.hidden != hidden
                || previous.filter != filter
                || previous.sort != sort
                || previous.descending != descending
        }) {
            *query = Some(ViewQuery {
                hidden,
                filter: filter.into(),
                sort,
                descending,
            });
            *scanned = 0;
            if let Some(rows) = Rc::get_mut(rows) {
                rows.clear();
            } else {
                *rows = Rc::default();
            }
        }
        if *scanned == self.len() {
            return rows.clone();
        }

        let filter = filter.to_lowercase();
        if sort == Sort::Name || !filter.is_empty() {
            folded_names.extend(self[folded_names.len()..].iter().map(|entry| {
                let name = &entry.name;
                if name.is_ascii() && !name.bytes().any(|byte| byte.is_ascii_uppercase()) {
                    return None;
                }
                let folded = name.to_lowercase();
                (folded != *name).then(|| folded.into_boxed_str())
            }));
        }
        let folded_name =
            |index: usize| folded_names[index].as_deref().unwrap_or(&self[index].name);
        let indices = Rc::make_mut(rows);
        let previous_len = indices.len();
        indices.extend((*scanned..self.len()).filter(|&index| {
            (hidden || !self[index].name.starts_with('.'))
                && (filter.is_empty() || folded_name(index).contains(&filter))
        }));
        if indices.len() != previous_len {
            // Prior rows are already sorted after an append. Stable sorting also
            // keeps enumeration order for entries with identical sort keys.
            indices.sort_by(|&a, &b| {
                self[b].is_dir.cmp(&self[a].is_dir).then_with(|| {
                    let order = match sort {
                        Sort::Name => folded_name(a).cmp(folded_name(b)),
                        Sort::Size => self[a].size.cmp(&self[b].size),
                        Sort::Modified => self[a].modified.cmp(&self[b].modified),
                    }
                    .then_with(|| self[a].name.cmp(&self[b].name));
                    if descending {
                        order.reverse()
                    } else {
                        order
                    }
                })
            });
        }
        *scanned = self.len();
        rows.clone()
    }
}

struct Tab {
    id: u64,
    path: Location,
    pinned: bool,
    address: String,
    edit_address: bool,
    focus_address: bool,
    address_error: Option<String>,
    address_completion: crate::address::Completion,
    focus_filter: bool,
    filter: String,
    entries: DirectoryEntries,
    selected: BTreeSet<Location>,
    previous_selection: Option<BTreeMap<Location, EntryIdentity>>,
    anchor: Option<Location>,
    cursor: Option<Location>,
    rendered_rows: HashMap<egui::Id, Location>,
    scroll_to: Option<Location>,
    scroll_offset: f32,
    scroll_restore: Option<f32>,
    file_scroll_scope: Option<egui::Id>,
    file_scroll_geometry: Option<(usize, f32)>,
    reveal_after_load: Option<Location>,
    column_reveal: Option<Location>,
    browse_columns: bool,
    history: Vec<Location>,
    history_index: usize,
    loading: Option<DirectoryLoad>,
    has_complete_listing: bool,
    folder_visit: FolderVisit,
    archive_index: Option<Arc<crate::archive::Index>>,
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

    fn from_session(saved: TabSession, home: &Path, ctx: &egui::Context) -> Self {
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

    fn session(&self) -> TabSession {
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
    fn begin_address(&mut self) {
        self.address = self.path.display();
        self.edit_address = true;
        self.focus_address = true;
        self.address_error = None;
        self.address_completion.reset();
    }
    fn reload(&mut self, ctx: &egui::Context) {
        self.archive_index = None;
        self.start_load(ctx, false);
    }
    fn start_load(&mut self, ctx: &egui::Context, automatic: bool) {
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
    fn auto_refresh(&mut self, ctx: &egui::Context, now: Instant) {
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
    fn blocking_load(&self) -> bool {
        self.loading.is_some() && !self.automatic_loading()
    }
    fn automatic_loading(&self) -> bool {
        self.loading
            .as_ref()
            .is_some_and(|load| load.mode == DirectoryLoadMode::Automatic)
    }
    fn finish_load(&mut self, result: Result<Option<Vec<FileEntry>>, String>) {
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
                if let Some(path) = self.column_reveal.take() {
                    if self.entries.iter().any(|entry| entry.path == path) {
                        self.select_revealed(path);
                    }
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
    fn select_revealed(&mut self, path: Location) {
        self.selected = BTreeSet::from([path.clone()]);
        self.anchor = Some(path.clone());
        self.cursor = Some(path.clone());
        self.scroll_to = Some(path);
    }

    fn navigate_column(&mut self, path: Location, reveal: Option<Location>, ctx: &egui::Context) {
        self.navigate(path, ctx);
        self.browse_columns = true;
        self.column_reveal = reveal;
    }

    fn navigate(&mut self, path: Location, ctx: &egui::Context) {
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
    fn reset_and_load(&mut self, ctx: &egui::Context) {
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
    fn history_go(&mut self, delta: isize, ctx: &egui::Context) {
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
    fn poll(&mut self) -> bool {
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
    fn folder_view(&self, settings: &Settings, pane: usize, views: &FolderViews) -> FolderView {
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

    fn folder_view_action(&self, pane: usize, change: FolderViewChange) -> Action {
        Action::FolderView {
            pane,
            tab: self.id,
            location: self.path.clone(),
            change,
        }
    }

    fn column_action(&self, pane: usize, navigation: ColumnNavigation) -> Action {
        Action::ColumnNavigation {
            pane,
            tab: self.id,
            location: self.path.clone(),
            navigation,
        }
    }

    fn legacy_order(&self) -> FileOrder {
        FileOrder {
            key: self.sort,
            descending: self.descending,
        }
    }

    fn visible(&self, view: FolderView) -> Rc<Vec<usize>> {
        self.entries.visible(
            view.hidden,
            &self.filter,
            view.order.key,
            view.order.descending,
        )
    }

    fn hide_dot_file_selection(&mut self) {
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
    fn choose(&mut self, index: usize, visible: &[usize], modifiers: egui::Modifiers) {
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

    fn entry_id(&self, path: &Location) -> egui::Id {
        egui::Id::new(("file_entry", self.id, path))
    }

    fn file_list_id(&self) -> egui::Id {
        // Stable row IDs need the same accessibility parent in every view.
        egui::Id::new(("file_list", self.id))
    }

    fn focus_cursor(&self, ctx: &egui::Context) {
        if let Some(path) = &self.cursor {
            ctx.memory_mut(|memory| memory.request_focus(self.entry_id(path)));
        }
    }

    fn focused_cursor(&self, ctx: &egui::Context) -> Option<egui::Id> {
        let id = self.entry_id(self.cursor.as_ref()?);
        ctx.memory(|memory| memory.has_focus(id)).then_some(id)
    }

    fn selected_visible(&self, view: FolderView) -> Vec<Location> {
        self.visible(view)
            .iter()
            .map(|&i| &self.entries[i].path)
            .filter(|path| self.selected.contains(*path))
            .cloned()
            .collect()
    }

    fn remember_selection(&mut self, visible: &[usize]) {
        self.previous_selection = Some(
            visible
                .iter()
                .map(|&index| &self.entries[index])
                .filter(|entry| self.selected.contains(&entry.path))
                .map(|entry| (entry.path.clone(), EntryIdentity::from(entry)))
                .collect(),
        );
    }

    fn reconcile_previous_selection(&mut self) {
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

    fn change_selection(&mut self, view: FolderView, change: SelectionChange) {
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

    fn move_selection(&mut self, delta: isize, view: FolderView, shift: bool) {
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
struct Pane {
    tabs: Vec<Tab>,
    current: usize,
    closed_tabs: VecDeque<(usize, Location)>,
    tab_context_menu: Option<egui::Id>,
    columns: column_view::Columns,
}
impl Pane {
    const MAX_CLOSED_TABS: usize = 16;

    fn new(tabs: Vec<Tab>) -> Self {
        Self {
            tabs,
            current: 0,
            closed_tabs: VecDeque::new(),
            tab_context_menu: None,
            columns: column_view::Columns::default(),
        }
    }

    fn from_session(saved: PaneSession, home: &Path, ctx: &egui::Context) -> Self {
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

    fn session(&self) -> PaneSession {
        PaneSession {
            tabs: self.tabs.iter().map(Tab::session).collect(),
            current: self.current,
        }
    }

    fn pinned_count(&self) -> usize {
        self.tabs.iter().take_while(|tab| tab.pinned).count()
    }

    fn toggle_pin(&mut self, index: usize) {
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

    fn can_close_tab(&self, index: usize) -> bool {
        self.tabs.len() > 1 && !self.tabs[index].pinned
    }

    fn close_tab(&mut self, index: usize) {
        if self.can_close_tab(index) {
            let tab = self.remove_tab(index);
            if self.closed_tabs.len() == Self::MAX_CLOSED_TABS {
                self.closed_tabs.pop_front();
            }
            self.closed_tabs.push_back((index, tab.path));
        }
    }

    fn insert_selected_tab(&mut self, index: usize, tab: Tab) {
        self.tab_mut().address_completion.suspend();
        self.tabs.insert(index, tab);
        self.current = index;
    }

    fn reopen_tab(&mut self, ctx: &egui::Context) -> bool {
        let Some((index, path)) = self.closed_tabs.pop_back() else {
            return false;
        };
        let slot = index.clamp(self.pinned_count(), self.tabs.len());
        self.insert_selected_tab(slot, Tab::new(path, ctx));
        true
    }

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
    fn select_tab(&mut self, index: usize) {
        if index != self.current {
            self.tab_mut().address_completion.suspend();
            self.current = index;
        }
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

#[derive(Clone)]
enum ColumnNavigation {
    Left,
    Right,
    Open(Location),
    Browse(Location),
    Reveal(Location),
}

#[derive(Clone)]
enum Action {
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
enum SelectionChange {
    All,
    Invert,
    Restore,
    Matches(BTreeSet<Location>),
}

struct SelectionScan {
    rule: Rule,
    rows: Rc<Vec<usize>>,
    next: usize,
    matches: BTreeSet<Location>,
}

impl SelectionScan {
    fn new(rule: Rule, rows: Rc<Vec<usize>>) -> Self {
        Self {
            rule,
            rows,
            next: 0,
            matches: BTreeSet::new(),
        }
    }

    fn step(&mut self, entries: &[FileEntry], deadline: Instant) -> bool {
        while let Some(&index) = self.rows.get(self.next) {
            if self.next.is_multiple_of(32) && Instant::now() >= deadline {
                return false;
            }
            let entry = &entries[index];
            if self.rule.matches(entry) {
                self.matches.insert(entry.path.clone());
            }
            self.next += 1;
        }
        true
    }
}

enum Dialog {
    SelectionRule {
        command: Command,
        kind: RuleKind,
        tab: u64,
        location: Location,
        input: String,
        scan: Option<SelectionScan>,
    },
    NewFolder {
        parent: Location,
        name: String,
    },
    Rename {
        source: Location,
        name: String,
    },
    Trash {
        paths: Vec<Location>,
    },
    WorkspaceName {
        editing: Option<usize>,
        name: String,
    },
}
enum TransferEvent {
    Conflict(Box<ConflictDialog>),
    Moved(Location, Vec<PathBuf>),
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

type JobWork = Box<dyn FnOnce(&mut Progress) -> Result<String, String> + Send>;
type CutRequest = (u64, Vec<Location>);

// One global worker serializes all mutations, including aliased and nested paths.
// Pending closures own their inputs; no filesystem work is done until dispatch.
struct PendingJob {
    label: String,
    description: String,
    work: JobWork,
    transfer: Option<TransferInteraction>,
    cut: Option<CutRequest>,
}

const MAX_PENDING_JOBS: usize = 100;

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
    folder_views: FolderViews,
    directory_tree: DirectoryTree,
    bookmarks: Vec<Bookmark>,
    bookmark_menu_open: Option<u64>,
    bookmark_editor: Option<BookmarkEditor>,
    workspaces: Vec<NamedWorkspace>,
    preview: Preview,
    thumbnails: Thumbnails,
    quick_look: Option<QuickLook>,
    quick_look_preview: Option<Preview>,
    properties: Option<crate::properties::Properties>,
    clipboard: Option<Clipboard>,
    system_clipboard: SystemClipboard,
    dialog: Option<Dialog>,
    dialog_focus: bool,
    dialog_file_focus: Option<(u64, Location)>,
    job: Option<Job>,
    pending_jobs: VecDeque<PendingJob>,
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
    editor_job: Option<Receiver<Result<String, String>>>,
    tools: crate::tools::Tools,
    search: crate::search::Search,
}
impl PawCommander {
    const SETTINGS_KEY: &str = "pawcommander.settings";
    const SESSION_KEY: &str = "pawcommander.session";
    const WORKSPACES_KEY: &str = "pawcommander.workspaces";
    const BOOKMARKS_KEY: &str = "pawcommander.bookmarks";
    const FOLDER_VIEWS_KEY: &str = "pawcommander.folder_views";

    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<PathBuf>) -> Self {
        let mut settings: Settings = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, Self::SETTINGS_KEY))
            .unwrap_or_default();
        let workspaces = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, Self::WORKSPACES_KEY))
            .unwrap_or_default();
        let folder_views = cc
            .storage
            .and_then(|storage| {
                eframe::get_value::<Vec<SavedFolderView>>(storage, Self::FOLDER_VIEWS_KEY)
            })
            .unwrap_or_default()
            .into_iter()
            .filter_map(|saved| Some((saved.location.into_location()?, saved.view)))
            .collect();
        let saved_bookmarks = cc.storage.and_then(|storage| {
            eframe::get_value::<Vec<SavedBookmark>>(storage, Self::BOOKMARKS_KEY)
        });
        let mut bookmarks: Vec<Bookmark> = saved_bookmarks.map_or_else(
            || {
                settings
                    .bookmarks
                    .iter()
                    .cloned()
                    .map(Bookmark::new)
                    .collect()
            },
            |saved| {
                saved
                    .into_iter()
                    .filter_map(SavedBookmark::into_bookmark)
                    .collect()
            },
        );
        reserve_bookmark_ids(&bookmarks);
        settings.shortcuts.validate();
        settings.list_columns.validate();
        let bookmark_shortcut_conflict =
            validate_bookmark_shortcuts(&mut bookmarks, &settings.shortcuts);
        crate::i18n::set_language(settings.language);
        settings.theme.apply(&cc.egui_ctx, settings.high_contrast);
        cc.egui_ctx
            .options_mut(|options| options.zoom_with_keyboard = false);
        settings.set_ui_scale(&cc.egui_ctx, settings.ui_scale);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let (panes, active) = if let Some(start) = start {
            let path = if start.is_absolute() {
                start
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| home.clone())
                    .join(start)
            };
            let mut left = Tab::new(path.clone().into(), &cc.egui_ctx);
            left.folder_visit = FolderVisit::CountOnOpen;
            // The launch argument puts the same location in both panes.
            let mut right = Tab::new(path.into(), &cc.egui_ctx);
            right.folder_visit = FolderVisit::Recorded;
            (vec![Pane::new(vec![left]), Pane::new(vec![right])], 0)
        } else if let Some(session) = cc
            .storage
            .and_then(|storage| eframe::get_value::<WorkspaceSession>(storage, Self::SESSION_KEY))
        {
            let panes = session
                .panes
                .into_iter()
                .map(|pane| Pane::from_session(pane, &home, &cc.egui_ctx))
                .collect();
            let active = if settings.split {
                session.active.min(1)
            } else {
                0
            };
            (panes, active)
        } else {
            let panes = (0..2)
                .map(|i| {
                    let Some(locations) = settings.locations.get(i).filter(|v| !v.is_empty())
                    else {
                        return Pane::new(vec![Tab::new(home.clone().into(), &cc.egui_ctx)]);
                    };
                    Pane::new(
                        locations
                            .iter()
                            .enumerate()
                            .map(|(index, path)| {
                                let mut tab = Tab::new(path.clone(), &cc.egui_ctx);
                                tab.pinned = index < settings.pinned_tab_counts[i];
                                tab
                            })
                            .collect(),
                    )
                })
                .collect();
            (panes, 0)
        };
        Self {
            panes,
            active,
            settings,
            folder_views,
            directory_tree: DirectoryTree::default(),
            bookmarks,
            bookmark_menu_open: None,
            bookmark_editor: None,
            workspaces,
            preview: Preview::new(),
            thumbnails: Thumbnails::new(&cc.egui_ctx),
            quick_look: None,
            quick_look_preview: None,
            properties: None,
            clipboard: None,
            system_clipboard: SystemClipboard::new(cc),
            dialog: None,
            dialog_focus: false,
            dialog_file_focus: None,
            job: None,
            pending_jobs: VecDeque::new(),
            queue_start_failed: false,
            #[cfg(test)]
            fail_job_spawn: false,
            active_cut: None,
            transfer: None,
            notice: bookmark_shortcut_conflict.then(|| {
                (
                    crate::i18n::text(
                        "Сочетание избранного очищено: клавиша занята другой командой",
                    )
                    .into(),
                    false,
                )
            }),
            show_help: false,
            history: History::new(&cc.egui_ctx),
            recovery: crate::recovery::Recovery::new(&cc.egui_ctx),
            show_history: false,
            shortcut_editor: None,
            command_palette: None,
            folder_jump: None,
            palette_release_key: None,
            palette_focus_to_restore: None,
            keyboard_had_focus: false,
            tab_held: false,
            pane_key_presses: Vec::new(),
            focus_toolbar: false,
            show_accessibility: false,
            accessibility_focus: false,
            show_list_columns: false,
            list_columns_focus: false,
            sftp_form: None,
            connection: None,
            native_drag: Default::default(),
            editor_job: None,
            tools: crate::tools::Tools::detect(),
            search: crate::search::Search::load(cc.storage),
        }
    }
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

    fn change_folder_view(&mut self, index: usize, change: FolderViewChange) {
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

    fn selected(&self) -> Vec<Location> {
        self.tab().selected_visible(self.folder_view(self.active))
    }
    fn refresh(&mut self, ctx: &egui::Context) {
        self.preview.refresh();
        self.directory_tree.refresh();
        for pane in &mut self.panes {
            pane.columns.refresh();
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
        self.start_described_job(ctx, label, label.into(), work);
    }

    fn start_described_job(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        description: String,
        work: impl FnOnce(&mut Progress) -> Result<String, String> + Send + 'static,
    ) {
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(work),
                transfer: None,
                cut: None,
            },
        );
    }

    fn submit_job(&mut self, ctx: &egui::Context, job: PendingJob) {
        if self.pending_jobs.len() >= MAX_PENDING_JOBS {
            self.notice = Some((crate::i18n::text("Очередь заполнена (100 заданий). Дождитесь завершения или удалите ожидающее задание.").into(), true));
            return;
        }
        if self.job.is_none() && self.pending_jobs.is_empty() {
            self.notice = None;
        }
        self.pending_jobs.push_back(job);
        self.start_next_job(ctx);
        ctx.request_repaint();
    }

    fn start_next_job(&mut self, ctx: &egui::Context) {
        if self.queue_start_failed {
            return;
        }
        while self.job.is_none() {
            let Some(PendingJob {
                label,
                description,
                work,
                transfer,
                cut,
                ..
            }) = self.pending_jobs.pop_front()
            else {
                break;
            };
            debug_assert!(self.transfer.is_none());
            let (tx, rx) = mpsc::channel();
            let worker_ctx = ctx.clone();
            let (mut reporter, progress) = Progress::tracked();
            let started = Instant::now();
            let history = self.history.sender();
            let history_label = label.clone();
            let recovery_directory = self.recovery.directory();
            let recovery_description = description.clone();
            // A failed OS spawn consumes its closure. Retain the work separately
            // so failure cannot discard this job or the jobs behind it.
            let work = Arc::new(Mutex::new(Some(work)));
            let worker_work = work.clone();
            let worker = move || {
                let work = worker_work.lock().unwrap().take().unwrap();
                let history_started = crate::result_history::now();
                if recovery_directory.is_none() && !cfg!(test) {
                    let _ = tx.send(Err(crate::tr_format!(
                        "Операция не начата: папка данных приложения для восстановления недоступна.",
                        "Operation not started: application data directory for recovery is unavailable."
                    )));
                    worker_ctx.request_repaint();
                    return;
                }
                let pending = match recovery_directory
                    .as_deref()
                    .map(|directory| {
                        crate::recovery::begin(
                            directory,
                            &history_label,
                            &recovery_description,
                            history_started,
                        )
                    })
                    .transpose()
                {
                    Ok(pending) => pending,
                    Err(error) => {
                        let _ = tx.send(Err(crate::tr_format!(
                            "Операция не начата: не удалось сохранить запись восстановления: {error}",
                            "Operation not started: could not save recovery record: {error}"
                        )));
                        worker_ctx.request_repaint();
                        return;
                    }
                };
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&mut reporter)));
                let panicked = outcome.is_err();
                let mut result = outcome.unwrap_or_else(|_| {
                    Err(crate::i18n::text(
                        "Фоновая операция неожиданно завершилась. Проверьте файлы в обеих папках.",
                    )
                    .into())
                });
                let persisted = if let Some(record) = std::mem::take(&mut reporter.results).finish(
                    history_label,
                    history_started,
                    &result,
                ) {
                    history.append(record)
                } else {
                    true
                };
                if !panicked && persisted {
                    if let Some(pending) = pending {
                        if let Err(error) = pending.complete() {
                            result = Err(crate::tr_format!(
                                "Операция завершилась, но очистка записи восстановления не подтверждена: {error}. Проверьте журнал результатов.",
                                "Operation finished, but recovery record cleanup could not be confirmed: {error}. Check result history."
                            ));
                        }
                    }
                } else if !persisted && recovery_directory.is_some() {
                    result = Err(crate::tr_format!(
                        "Операция завершилась, но журнал результатов не сохранён. Проверьте файлы в обеих папках; запись восстановления сохранена, если была создана.",
                        "Operation finished, but result history was not saved. Check files in both folders; the recovery record was retained if created."
                    ));
                }
                let _ = tx.send(result);
                worker_ctx.request_repaint();
            };
            let builder = std::thread::Builder::new().name("pawcommander-file-operation".into());
            #[cfg(test)]
            let spawn_result = if self.fail_job_spawn {
                Err(std::io::Error::other("injected worker spawn failure"))
            } else {
                builder.spawn(worker)
            };
            #[cfg(not(test))]
            let spawn_result = builder.spawn(worker);
            if let Err(error) = spawn_result {
                self.pending_jobs.push_front(PendingJob {
                    label,
                    description,
                    work: work.lock().unwrap().take().unwrap(),
                    transfer,
                    cut,
                });
                self.queue_start_failed = true;
                self.notice = Some((
                    crate::tr_format!(
                        "Не удалось запустить операцию: {error}",
                        "Could not start operation: {error}"
                    ),
                    true,
                ));
                break;
            }
            self.job = Some(Job {
                label,
                started,
                result: rx,
                progress,
            });
            self.transfer = transfer;
            self.active_cut = cut;
            ctx.request_repaint();
        }
    }
    fn drop_enabled(&self) -> bool {
        !self.conflict_open()
            && self.quick_look.is_none()
            && self.dialog.is_none()
            && self.properties.is_none()
            && self.sftp_form.is_none()
            && !self.show_help
            && !self.show_history
            && !self.recovery.is_open()
            && self.shortcut_editor.is_none()
            && self.command_palette.is_none()
            && self.folder_jump.is_none()
            && !self.show_list_columns
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
        if sources.is_empty() {
            return;
        }
        if destination.is_archive() || sources.iter().any(Location::is_archive) {
            self.notice = Some((crate::archive::readonly().into(), true));
            return;
        }
        let cut_generation = if from_cut_clipboard {
            self.clipboard
                .as_ref()
                .map(|clipboard| clipboard.generation)
        } else {
            None
        };
        let cut = cut_generation.map(|generation| (generation, sources.clone()));
        if let Some((generation, paths)) = &cut {
            let overlaps = |(other_generation, other_paths): &CutRequest| {
                generation == other_generation
                    && paths.iter().any(|path| {
                        other_paths.iter().any(|other| {
                            path.same_backend(other)
                                && (path.path().starts_with(other.path())
                                    || other.path().starts_with(path.path()))
                        })
                    })
            };
            if self.active_cut.as_ref().is_some_and(overlaps)
                || self
                    .pending_jobs
                    .iter()
                    .filter_map(|job| job.cut.as_ref())
                    .any(overlaps)
            {
                self.notice = Some((
                    crate::i18n::text("Эти вырезанные объекты уже ожидают перемещения.").into(),
                    true,
                ));
                return;
            }
        }
        let verify_copies = kind == TransferKind::Copy && self.settings.verify_copies;
        let mut description = format!(
            "{} ({}): {} → {}",
            label,
            sources.len(),
            sources.first().unwrap().display(),
            destination.display()
        );
        if verify_copies {
            description.push_str(" · SHA-256");
        }
        let (events, rx) = mpsc::channel();
        let worker_ctx = ctx.clone();
        let work = move |progress: &mut Progress| {
            progress.set_copy_verification(verify_copies);
            let cancellation = progress.cancellation();
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
                    Ok(wait_for_conflict_answer(&answer, &cancellation))
                },
                |location, covered| {
                    if cut_generation.is_some() {
                        let _ =
                            events.send(TransferEvent::Moved(location.clone(), covered.to_vec()));
                        worker_ctx.request_repaint();
                    }
                },
            )?;
            Ok(crate::i18n::transfer_finished(
                summary.completed,
                summary.skipped,
                &summary
                    .remaining
                    .iter()
                    .map(Location::display)
                    .collect::<Vec<_>>(),
                summary.preparing,
                summary.cancelled,
                &destination.display(),
            ))
        };
        self.submit_job(
            ctx,
            PendingJob {
                label: label.into(),
                description,
                work: Box::new(work),
                transfer: Some(TransferInteraction {
                    events: rx,
                    cut_generation,
                    conflict: None,
                }),
                cut,
            },
        );
    }

    fn poll_transfer(&mut self) {
        while let Some(transfer) = &mut self.transfer {
            match transfer.events.try_recv() {
                Ok(TransferEvent::Conflict(dialog)) => {
                    if !self
                        .job
                        .as_ref()
                        .is_some_and(|job| job.progress.cancel.is_requested())
                    {
                        transfer.conflict = Some(dialog);
                    }
                }
                Ok(TransferEvent::Moved(location, covered)) => {
                    if let Some(generation) = transfer.cut_generation {
                        self.consume_cut_source(&location, &covered, generation);
                    }
                }
                // The worker drops its event sender before publishing its result. Keep
                // ownership (and Stop state) until that terminal result is consumed.
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
    }

    fn request_transfer_stop(&mut self, ctx: &egui::Context) {
        if self.transfer.is_none() {
            return;
        }
        if let Some(job) = &self.job {
            if job.progress.cancel.is_requested() {
                return;
            }
            job.progress.cancel.request();
            if let Some(transfer) = &mut self.transfer {
                // Dropping a dialog releases its answer sender too. The worker also
                // observes the token if a conflict event is still in flight.
                transfer.conflict = None;
            }
            ctx.request_repaint();
        }
    }

    fn transfer_stop_shortcut(&mut self, ctx: &egui::Context) {
        if self.transfer.is_some()
            && self.job.is_some()
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::Escape))
        {
            self.request_transfer_stop(ctx);
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

    fn poll(&mut self, ctx: &egui::Context) {
        self.poll_transfer();
        if self.conflict_open() && self.dialog_file_focus.is_none() {
            self.remember_dialog_file_focus(ctx);
        }
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
                        self.directory_tree.refresh();
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
                        for pane in &mut self.panes {
                            pane.tab_mut().address_completion.suspend();
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
        // Avoid reordering rows under a held pointer. Defer automatic updates
        // during operations as well.
        let can_reorder = !self.native_drag.active()
            && !ctx.input(|input| input.pointer.any_down() || input.pointer.any_released())
            && !egui::DragAndDrop::has_any_payload(ctx);
        let automatic_allowed = self.job.is_none() && self.transfer.is_none() && can_reorder;
        // Hold automatic refresh results while a rule examines this tab's visible rows.
        let scanning_tab = match &self.dialog {
            Some(Dialog::SelectionRule {
                tab, scan: Some(_), ..
            }) => Some(*tab),
            _ => None,
        };
        let now = Instant::now();
        for (pane_index, pane) in self.panes.iter_mut().enumerate() {
            let displayed = self.settings.split || pane_index == 0;
            for (tab_index, tab) in pane.tabs.iter_mut().enumerate() {
                let current = displayed && tab_index == pane.current;
                let automatic_allowed = automatic_allowed && scanning_tab != Some(tab.id);
                if can_reorder && (!tab.automatic_loading() || automatic_allowed) && tab.poll() {
                    ctx.request_repaint();
                }
                if tab.has_complete_listing {
                    match tab.folder_visit {
                        FolderVisit::CountOnOpen => {
                            self.settings.folder_history.record(&tab.path);
                            tab.folder_visit = FolderVisit::Recorded;
                        }
                        FolderVisit::Unseen if current => {
                            self.settings.folder_history.record_if_new(&tab.path);
                            tab.folder_visit = FolderVisit::Recorded;
                        }
                        FolderVisit::Unseen | FolderVisit::Recorded => {}
                    }
                }
                if current {
                    if automatic_allowed {
                        tab.auto_refresh(ctx, now);
                    } else {
                        ctx.request_repaint_after(AUTO_REFRESH_INTERVAL);
                    }
                }
            }
            let tab = &pane.tabs[pane.current];
            let columns = displayed
                && tab
                    .folder_view(&self.settings, pane_index, &self.folder_views)
                    .mode
                    == FolderMode::Columns;
            let can_read = can_reorder
                && (!tab.path.is_archive() || tab.has_complete_listing)
                && (!tab.path.remote() || (tab.has_complete_listing && self.connection.is_none()));
            pane.columns
                .update(ctx, columns.then_some(tab), can_read, now);
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
                // Moved events can arrive between the first event poll and the
                // result. Consume them before releasing transfer ownership.
                self.poll_transfer();
                self.transfer = None;
                self.active_cut = None;
                self.job = None;
                self.refresh(ctx);
            }
        }
        self.start_next_job(ctx);
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

    fn consume_cut_source(&mut self, source: &Location, covered: &[PathBuf], generation: u64) {
        if let Some(clipboard) = self
            .clipboard
            .as_mut()
            .filter(|clipboard| clipboard.generation == generation)
        {
            if let Some(source) = source.local() {
                let moved: HashSet<_> = std::iter::once(source)
                    .chain(covered.iter().map(PathBuf::as_path))
                    .collect();
                #[cfg(target_os = "macos")]
                if let ClipboardOrigin::System(files) = &clipboard.origin {
                    self.system_clipboard.consume_moved(files, &moved);
                }
                clipboard
                    .paths
                    .retain(|path| path.local().is_none_or(|path| !moved.contains(path)));
            } else {
                clipboard.paths.retain(|path| {
                    !path.same_backend(source) || !path.path().starts_with(source.path())
                });
            }
        }
    }

    fn action(&mut self, action: Action, ctx: &egui::Context) {
        if self.tab().path.is_archive()
            && matches!(
                action,
                Action::Copy(_)
                    | Action::Paste(_)
                    | Action::NewFolder
                    | Action::Rename
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
                } else if let Some(path) = p.local() {
                    if let Err(e) = open::that_detached(path) {
                        self.notice = Some((
                            crate::tr_format!(
                                "Не удалось открыть файл: {e}",
                                "Could not open file: {e}"
                            ),
                            true,
                        ));
                    }
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
                if selected.len() == 1 {
                    self.remember_dialog_file_focus(ctx);
                    let source = selected[0].clone();
                    let name = file_name(&source);
                    self.dialog = Some(Dialog::Rename { source, name });
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
    fn navigate_columns(&mut self, pane: usize, navigation: ColumnNavigation, ctx: &egui::Context) {
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

    fn navigate_history(&mut self, pane: usize, delta: isize, ctx: &egui::Context) {
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

    fn open_workspace(&mut self, index: usize, ctx: &egui::Context) {
        let Some(workspace) = self.workspaces.get(index) else {
            return;
        };
        let [Some(left), Some(right)] = workspace.folders.clone().map(SavedLocation::into_location)
        else {
            self.notice = Some((
                crate::i18n::text("Пути этого рабочего пространства несовместимы с системой.")
                    .into(),
                true,
            ));
            return;
        };
        self.settings.split = true;
        for (pane, path) in self.panes.iter_mut().zip([left, right]) {
            pane.tab_mut().navigate(path, ctx);
        }
    }

    fn save_workspace_name(&mut self, editing: Option<usize>, name: String) {
        let name = name.trim().to_owned();
        if let Some(index) = editing {
            if let Some(workspace) = self.workspaces.get_mut(index) {
                workspace.name = name;
            }
        } else {
            self.workspaces.push(NamedWorkspace {
                name,
                folders: std::array::from_fn(|index| (&self.panes[index].tab().path).into()),
            });
        }
    }

    fn workspace_name_taken(&self, name: &str, editing: Option<usize>) -> bool {
        let name = name.trim();
        self.workspaces
            .iter()
            .enumerate()
            .any(|(index, workspace)| Some(index) != editing && workspace.name == name)
    }

    fn open_search(&mut self) {
        self.search.open(self.panes[self.active].tab().path.local());
    }

    fn reveal_search_result(&mut self, path: PathBuf, ctx: &egui::Context) {
        let Some(parent) = path.parent() else {
            return;
        };
        self.tab_mut().navigate(parent.to_path_buf().into(), ctx);
        // A hidden result needs to be visible in its destination folder.
        if is_dot_file(&path) && !self.folder_view(self.active).hidden {
            self.change_folder_view(self.active, FolderViewChange::Hidden(true));
        }
        let tab = self.tab_mut();
        tab.filter.clear();
        tab.selected.clear();
        tab.reveal_after_load = Some(path.into());
    }

    fn show_search_results(&mut self, ctx: &egui::Context) {
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
            .show(ctx, target, destination, enabled, can_copy);
    }

    fn search_target_is_current(&mut self, target: &TabTarget, ctx: &egui::Context) -> bool {
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

    fn dispatch_search_result(&mut self, action: ReadyAction, ctx: &egui::Context) {
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
                if crate::archive::Format::from_path(&path).is_some() {
                    if self.search_target_is_current(&target, ctx) {
                        Self::restore_focus(ctx, None);
                        self.action(Action::Open(target.pane, Location::Local(path)), ctx);
                    }
                } else {
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
                    let location = path.into();
                    if self.folder_view(target.pane).mode == FolderMode::Columns {
                        self.navigate_columns(target.pane, ColumnNavigation::Browse(location), ctx);
                    } else {
                        self.tab_mut().navigate(location, ctx);
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
                    paths.into_iter().map(Location::from).collect(),
                    destination,
                    TransferKind::Copy,
                    false,
                );
            }
        }
    }

    fn toggle_split(&mut self) {
        self.settings.split = !self.settings.split;
        if !self.settings.split {
            self.panes[1].tab_mut().address_completion.suspend();
            self.active = 0;
        }
    }

    fn run_shortcut(&mut self, command: Command, ctx: &egui::Context) {
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
            Command::Rename => Action::Rename,
            Command::NewFolder => Action::NewFolder,
            Command::Trash => Action::Trash,
            Command::CopyToPane => Action::Transfer(TransferKind::Copy),
            Command::MoveToPane => Action::Transfer(TransferKind::Move),
        };
        self.action(action, ctx);
    }

    fn apply_selection(&mut self, change: SelectionChange, ctx: &egui::Context) {
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

    fn keyboard_blocked(&self, ctx: &egui::Context) -> bool {
        let visible_panes = 1 + usize::from(self.settings.split);
        egui::DragAndDrop::has_payload_of_type::<DragTab>(ctx)
            || egui::DragAndDrop::has_payload_of_type::<DragFiles>(ctx)
            || ctx.memory(|memory| memory.any_popup_open())
            // Legacy egui context menus are tracked separately from other popups.
            || self.panes[..visible_panes].iter().any(|pane| {
                pane.tab_context_menu
                    .and_then(|id| ctx.read_response(id))
                    .is_some_and(|response| response.context_menu_opened())
            })
            || self.dialog.is_some()
            || self.properties.is_some()
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

    fn bound_pane_command(&self, chord: egui::KeyboardShortcut) -> Option<Command> {
        self.settings.shortcuts.command(chord).filter(|&command| {
            chord.logical_key != egui::Key::Tab
                || command != Command::OtherPane
                || self.settings.split
        })
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
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
            && (ctx.wants_keyboard_input()
                || (self.keyboard_had_focus
                    && ctx.input(|input| input.key_pressed(egui::Key::Escape))));
        let mut actions = Vec::new();
        let mut pane = self.active;
        ctx.input_mut(|input| {
            if !focused {
                for (index, event) in pane_key_presses {
                    input.events.insert(index.min(input.events.len()), event);
                }
            }
            input.events.retain(|event| {
                let action = if let egui::Event::Key {
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
                } else if !focused {
                    file_clipboard_action(event)
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

    fn run_pane_action(&mut self, action: Action, ctx: &egui::Context) {
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

    fn open_command_palette(&mut self, ctx: &egui::Context, previous_focus: Option<egui::Id>) {
        let focused_file = self
            .file_row_focus(ctx)
            .filter(|(pane, file)| Some(self.panes[*pane].tab().entry_id(file)) == previous_focus)
            .map(|(_, file)| file.clone());
        self.command_palette = Some(CommandPalette::new(previous_focus, focused_file));
        Self::restore_focus(ctx, None);
    }

    fn restore_focus(ctx: &egui::Context, previous_focus: Option<egui::Id>) {
        ctx.memory_mut(|memory| {
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
            if let Some(id) = previous_focus {
                memory.request_focus(id);
            }
        });
    }

    fn guard_picker_close_key(&mut self, ctx: &egui::Context, keys: &[egui::Key], key: egui::Key) {
        if keys.contains(&key) && ctx.input(|input| input.key_down(key)) {
            self.palette_release_key = Some(key);
        }
    }

    fn show_command_palette(&mut self, ctx: &egui::Context, keys: &[egui::Key]) {
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
                Command::Rename => idle && writable && has_selection && selected.len() == 1,
                Command::Trash => idle && writable && has_selection,
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

    fn show_folder_jump(&mut self, ctx: &egui::Context, keys: &[egui::Key]) {
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

    fn file_row_focus(&self, ctx: &egui::Context) -> Option<(usize, &Location)> {
        let id = ctx.memory(|memory| memory.focused())?;
        self.panes
            .iter()
            .enumerate()
            .take(if self.settings.split { 2 } else { 1 })
            .find_map(|(pane, state)| state.tab().rendered_rows.get(&id).map(|path| (pane, path)))
    }

    fn remember_dialog_file_focus(&mut self, ctx: &egui::Context) {
        self.dialog_file_focus = self
            .file_row_focus(ctx)
            .map(|(pane, path)| (self.panes[pane].tab().id, path.clone()))
            .or_else(|| {
                let tab = self.tab();
                tab.focused_cursor(ctx)?;
                Some((tab.id, tab.cursor.clone()?))
            });
    }

    fn restore_dialog_file_focus(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() {
            return;
        }
        let Some((tab_id, path)) = self.dialog_file_focus.take() else {
            return;
        };
        if self.keyboard_blocked(ctx) || self.tab().id != tab_id {
            return;
        }
        let tab = self.tab();
        let id = tab.entry_id(&path);
        let target = tab
            .rendered_rows
            .contains_key(&id)
            .then_some(id)
            .or_else(|| {
                tab.visible(self.folder_view(self.active))
                    .iter()
                    .map(|&index| tab.entry_id(&tab.entries[index].path))
                    .find(|id| tab.rendered_rows.contains_key(id))
            });
        if let Some(id) = target {
            // Render has run, so the just-closed modal cannot clear this request.
            ctx.memory_mut(|memory| memory.request_focus(id));
            ctx.request_repaint();
        }
    }

    fn scale_shortcuts(&mut self, ctx: &egui::Context) {
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

    fn accessibility_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_accessibility {
            return;
        }
        let response = egui::Modal::new(egui::Id::new("accessibility")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.heading(crate::i18n::text("Доступность"));
            let contrast = ui.checkbox(
                &mut self.settings.high_contrast,
                crate::i18n::text("Высокая контрастность"),
            );
            if self.accessibility_focus && !ui.is_sizing_pass() {
                contrast.request_focus();
                self.accessibility_focus = false;
            }
            if contrast.changed() {
                self.settings.theme.apply(ctx, self.settings.high_contrast);
            }
            ui.add_space(6.0);
            ui.label(crate::i18n::text("Масштаб интерфейса"));
            for scale in Settings::UI_SCALES {
                let response = ui.radio(self.settings.ui_scale == scale, format!("{scale}%"));
                if response.clicked() {
                    self.settings.set_ui_scale(ctx, scale);
                }
            }
            ui.add_space(6.0);
            ui.button(crate::i18n::text("Закрыть")).clicked()
        });
        if response.inner || response.should_close() {
            self.show_accessibility = false;
            self.focus_toolbar = true;
            ctx.request_repaint();
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
                    let writable = !tab.path.is_archive();
                    for (icon, tip, enabled, action) in [
                        (Icon::Back, Command::Back, tab.history_index > 0, Action::Back),
                        (Icon::Forward, Command::Forward, tab.history_index + 1 < tab.history.len(), Action::Forward),
                        (Icon::Up, Command::Parent, tab.path.parent().is_some(), Action::Up),
                        (Icon::Refresh, Command::Refresh, true, Action::Refresh),
                    ] {
                        let tip = self.settings.shortcuts.hint(ctx, tip, tip.label());
                        if ui.add_enabled_ui(enabled, |ui| icons::button(ui, icon, "", false)).inner.on_hover_text(tip).clicked() {
                            actions.push(action);
                        }
                    }
                    ui.separator();
                    let search_width = (ui.available_width() - 432.0).clamp(icons::TOOLBAR_BUTTON_SIZE, 420.0);
                    let search_label = if search_width >= 150.0 {
                        crate::i18n::text("Поиск файлов…")
                    } else if search_width >= 95.0 {
                        crate::i18n::text("Поиск…")
                    } else {
                        ""
                    };
                    let search_enabled = !self.keyboard_blocked(ctx);
                    if ui.add_enabled_ui(search_enabled, |ui| icons::button_width(ui, Icon::Search, search_label, false, search_width)).inner
                        .on_hover_text(self.settings.shortcuts.hint(ctx, Command::Search, Command::Search.label())).clicked() {
                        self.open_search();
                    }
                    if ui.add_enabled_ui(self.job.is_none(), |ui| icons::button(ui, Icon::Server, "SFTP", self.tab().path.remote())).inner
                        .on_hover_text(crate::i18n::text("Подключиться по SFTP")).clicked() {
                        actions.push(Action::ConnectSftp);
                    }
                    for (icon, tip, enabled, action) in [
                        (Icon::NewFolder, self.settings.shortcuts.hint(ctx, Command::NewFolder, Command::NewFolder.label()), writable && !self.conflict_open(), Action::NewFolder),
                        (Icon::Copy, crate::i18n::text("Копировать · ⌘/Ctrl+C").into(), writable && !self.tab().selected.is_empty(), Action::Copy(false)),
                        (Icon::Paste, crate::i18n::text("Вставить · ⌘/Ctrl+V").into(), writable && !self.conflict_open(), Action::Paste(false)),
                    ] {
                        if ui.add_enabled_ui(enabled, |ui| icons::button(ui, icon, "", false)).inner.on_hover_text(tip).clicked() {
                            actions.push(action);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let menu_id = ui.id().with("settings_menu");
                        let settings_label = if ctx.screen_rect().width() >= 760.0 {
                            crate::i18n::text("Настройки")
                        } else {
                            ""
                        };
                        let button = icons::button(ui, Icon::Settings, settings_label, false);
                        if self.focus_toolbar && !self.show_accessibility {
                            button.request_focus();
                            paint_focus(ui, &button, 8);
                            self.focus_toolbar = false;
                        }
                        let mut menu = egui::menu::BarState::load(ctx, menu_id);
                        let opened = button.clicked() && menu.is_none();
                        let focus_entry = ui.data_mut(|data| {
                            if opened {
                                data.insert_temp(menu_id.with("focus_entry"), true);
                            }
                            data.get_temp::<bool>(menu_id.with("focus_entry")).unwrap_or(false)
                        });
                        let shown = menu.bar_menu(&button, |ui| {
                            let response = ui.button(crate::i18n::text("Доступность"));
                            if focus_entry && !ui.is_sizing_pass() {
                                response.request_focus();
                                ui.data_mut(|data| data.remove::<bool>(menu_id.with("focus_entry")));
                            }
                            paint_focus(ui, &response, 5);
                            if response.clicked() {
                                self.show_accessibility = true;
                                self.accessibility_focus = true;
                                ui.close_menu();
                            }
                            let shortcuts = ui.button(crate::i18n::text("Горячие клавиши"));
                            paint_focus(ui, &shortcuts, 5);
                            if shortcuts.clicked() {
                                self.shortcut_editor = Some(ShortcutEditor::new(&self.settings.shortcuts));
                                ctx.memory_mut(|memory| {
                                    if let Some(id) = memory.focused() {
                                        memory.surrender_focus(id);
                                    }
                                });
                                ui.close_menu();
                            }
                            let palette_label = Command::Palette.label();
                            let palette = ui.button(self.settings.shortcuts.hint(ctx, Command::Palette, palette_label));
                            paint_focus(ui, &palette, 5);
                            if palette.clicked() {
                                self.open_command_palette(ctx, Some(button.id));
                                ui.close_menu();
                            }
                            let about = ui.button(crate::i18n::text("О PawCommander"));
                            paint_focus(ui, &about, 5);
                            if about.clicked() { self.show_help = true; ui.close_menu(); }
                            let language_menu = ui.menu_button("Язык / Language", |ui| {
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
                            paint_focus(ui, &language_menu.response, 5);
                            ui.separator();
                            let verification = ui.checkbox(
                                &mut self.settings.verify_copies,
                                crate::i18n::text("Проверять копии (SHA-256)"),
                            ).on_hover_text(crate::i18n::text("Повторно читает временные файлы перед публикацией. Применяется к новым заданиям копирования; перемещение использует переименование."));
                            paint_focus(ui, &verification, 5);
                            ui.separator();
                            appearance_menu(ui, &mut self.settings.theme, self.settings.high_contrast);
                            ui.separator();
                            ui.label(crate::i18n::text("Расположение панелей"));
                            ui.horizontal(|ui| {
                                for orientation in [PaneOrientation::SideBySide, PaneOrientation::Stacked] {
                                    let response = ui.radio_value(
                                        &mut self.settings.pane_layout.orientation,
                                        orientation,
                                        orientation.label(),
                                    );
                                    paint_focus(ui, &response, 5);
                                }
                            });
                            ui.separator();
                            if ui.button(crate::tr_format!("Восстановление после сбоя", "Crash recovery")).clicked() {
                                self.recovery.reopen();
                                ui.close_menu();
                            }
                            ui.separator();
                            ui.label(crate::i18n::text("Вид текущей папки"));
                            let view = self.folder_view(self.active);
                            let mut hidden = view.hidden;
                            let response = ui.checkbox(&mut hidden, crate::i18n::text("Скрытые файлы"));
                            paint_focus(ui, &response, 5);
                            if response.changed() {
                                actions.push(self.folder_view_action(self.active, FolderViewChange::Hidden(hidden)));
                            }
                            ui.horizontal(|ui| {
                                ui.label(crate::i18n::text("Плотность"));
                                for density in [Density::Standard, Density::Compact] {
                                    let response = ui.radio(view.density == density, density.label());
                                    paint_focus(ui, &response, 5);
                                    if response.clicked() {
                                        actions.push(self.folder_view_action(self.active, FolderViewChange::Density(density)));
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label(crate::i18n::text("Сортировка"));
                                sort_controls(ui, view.order, |sort| {
                                    actions.push(self.folder_view_action(self.active, FolderViewChange::Sort(sort)));
                                });
                            });
                            let columns = ui.button(crate::i18n::text("Столбцы списка"));
                            paint_focus(ui, &columns, 5);
                            if columns.clicked() {
                                self.show_list_columns = true;
                                self.list_columns_focus = true;
                                ui.close_menu();
                            }
                            ui.separator();
                            let tools_menu = ui.menu_button(crate::i18n::text("Инструменты"), |ui| {
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
                            paint_focus(ui, &tools_menu.response, 5);
                        });
                        if shown.is_some() && menu.is_none() && ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
                            button.request_focus();
                        }
                        ctx.accesskit_node_builder(button.id, |node| node.set_expanded(menu.is_some()));
                        menu.store(ctx, menu_id);
                        if icons::button(ui, Icon::Info, "", self.settings.preview).on_hover_text(self.settings.shortcuts.hint(ctx, Command::Preview, Command::Preview.label())).clicked() {
                            self.settings.preview = !self.settings.preview;
                        }
                        let split_icon = match self.settings.pane_layout.orientation {
                            PaneOrientation::SideBySide => Icon::Split,
                            PaneOrientation::Stacked => Icon::SplitStacked,
                        };
                        if icons::button(ui, split_icon, "", self.settings.split).on_hover_text(self.settings.shortcuts.hint(ctx, Command::Split, Command::Split.label())).clicked() {
                            self.toggle_split();
                        }
                        let sidebar = self.settings.pane_layout.sidebar_visible;
                        let hint = crate::i18n::text(if sidebar {
                            "Скрыть боковую панель"
                        } else {
                            "Показать боковую панель"
                        });
                        if icons::button(ui, Icon::Sidebar, "", sidebar).on_hover_text(hint).clicked() {
                            self.settings.pane_layout.sidebar_visible = !sidebar;
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
        // Rebuild this from visible menu responses every frame. The keyboard
        // guard checks the row ID too, so a removed bookmark cannot leave it set.
        self.bookmark_menu_open = None;
        if !self.settings.pane_layout.sidebar_visible {
            self.directory_tree.inactive();
            return;
        }
        let colors = Palette::from_ctx(ctx);
        egui::SidePanel::left("places")
            .exact_width(if ctx.screen_rect().width() < 900.0 {
                148.0
            } else {
                204.0
            })
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
                        let jump = ui.add_enabled(
                            !self.keyboard_blocked(ctx),
                            egui::Button::new(crate::i18n::text("К папке…")),
                        );
                        paint_focus(ui, &jump, 5);
                        if jump
                            .on_hover_text(self.settings.shortcuts.hint(
                                ctx,
                                Command::FolderJump,
                                Command::FolderJump.label(),
                            ))
                            .clicked()
                        {
                            actions.push(Action::Shortcut(Command::FolderJump));
                        }
                        let tab = self.tab();
                        let current = tab.path.clone();
                        let tab_id = tab.id;
                        let waiting_for_sftp = current.remote() && tab.blocking_load();
                        let tree_location = current
                            .archive_source()
                            .and_then(Path::parent)
                            .map(|parent| Location::Local(parent.to_owned()));
                        let show_hidden = self.folder_view(self.active).hidden;
                        let tree = ui.add_enabled_ui(!self.keyboard_blocked(ctx), |ui| {
                            self.directory_tree.show(
                                ui,
                                tab_id,
                                tree_location.as_ref().unwrap_or(&current),
                                show_hidden,
                                waiting_for_sftp,
                            )
                        });
                        if let Some(location) = tree.inner {
                            actions.push(Action::Navigate(self.active, location));
                        }
                        self.workspace_places(ui, actions);
                        ui.add_space(6.0);
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
                                let groups: Vec<(String, Vec<usize>)> = {
                                    let mut groups = Vec::<(String, Vec<usize>)>::new();
                                    for (index, bookmark) in self.bookmarks.iter().enumerate() {
                                        let label = bookmark.group.trim().to_owned();
                                        if let Some((_, indices)) =
                                            groups.iter_mut().find(|(name, _)| *name == label)
                                        {
                                            indices.push(index);
                                        } else {
                                            groups.push((label, vec![index]));
                                        }
                                    }
                                    groups
                                };
                                let mut remove = None;
                                let mut pending_move = None;
                                for (group, indices) in groups {
                                    if !group.is_empty() {
                                        ui.small(RichText::new(&group).color(colors.muted));
                                    }
                                    for i in indices {
                                        let (id, name, location, shortcut) = {
                                            let bookmark = &self.bookmarks[i];
                                            (
                                                bookmark.id,
                                                bookmark.name.clone(),
                                                bookmark.location.clone(),
                                                bookmark.shortcut,
                                            )
                                        };
                                        let label = shortcut.map_or_else(
                                            || name.clone(),
                                            |slot| format!("{}  ·  Alt+{slot}", name),
                                        );
                                        ui.horizontal(|ui| {
                                            let response = ui
                                                .add_sized(
                                                    [ui.available_width() - 32.0, 29.0],
                                                    egui::Button::new(format!("      {label}"))
                                                        .selected(current == location),
                                                )
                                                .on_hover_text(location.display().to_string());
                                            icons::paint(
                                                ui,
                                                egui::Rect::from_center_size(
                                                    response.rect.left_center()
                                                        + Vec2::new(16.0, 0.0),
                                                    Vec2::splat(18.0),
                                                ),
                                                Icon::Star,
                                                colors.yellow,
                                            );
                                            if response.clicked() {
                                                actions.push(Action::Navigate(
                                                    self.active,
                                                    location.clone(),
                                                ));
                                            }
                                            ui.menu_button("⋯", |ui| {
                                                self.bookmark_menu_open = Some(id);
                                                if ui
                                                    .button(crate::i18n::text("Изменить избранное"))
                                                    .clicked()
                                                {
                                                    self.bookmark_editor = Some(
                                                        BookmarkEditor::new(&self.bookmarks[i]),
                                                    );
                                                    ui.close_menu();
                                                }
                                                ui.separator();
                                                ui.menu_button(
                                                    crate::i18n::text("Сочетание"),
                                                    |ui| {
                                                        if ui
                                                            .selectable_label(
                                                                self.bookmarks[i]
                                                                    .shortcut
                                                                    .is_none(),
                                                                crate::i18n::text("Не назначено"),
                                                            )
                                                            .clicked()
                                                        {
                                                            assign_bookmark_shortcut(
                                                                &mut self.bookmarks,
                                                                i,
                                                                None,
                                                                &self.settings.shortcuts,
                                                            );
                                                            ui.close_menu();
                                                        }
                                                        for slot in 1..=9 {
                                                            let occupied_by_command = self
                                                                .settings
                                                                .shortcuts
                                                                .command(shortcuts::chord(
                                                                    bookmark_slot_key(slot)
                                                                        .unwrap(),
                                                                    egui::Modifiers::ALT,
                                                                ))
                                                                .is_some();
                                                            let occupied_by_bookmark = self
                                                                .bookmarks
                                                                .iter()
                                                                .enumerate()
                                                                .any(|(other, item)| {
                                                                    other != i
                                                                        && item.shortcut
                                                                            == Some(slot)
                                                                });
                                                            let response = ui.add_enabled(
                                                                !occupied_by_command
                                                                    && !occupied_by_bookmark,
                                                                egui::SelectableLabel::new(
                                                                    self.bookmarks[i].shortcut
                                                                        == Some(slot),
                                                                    format!("Alt+{slot}"),
                                                                ),
                                                            );
                                                            if response.clicked() {
                                                                assign_bookmark_shortcut(
                                                                    &mut self.bookmarks,
                                                                    i,
                                                                    Some(slot),
                                                                    &self.settings.shortcuts,
                                                                );
                                                                ui.close_menu();
                                                            }
                                                        }
                                                    },
                                                );
                                                ui.separator();
                                                ui.horizontal(|ui| {
                                                    let previous =
                                                        bookmark_peer(&self.bookmarks, i, -1);
                                                    if ui
                                                        .add_enabled(
                                                            previous.is_some(),
                                                            egui::Button::new(crate::i18n::text(
                                                                "Вверх",
                                                            )),
                                                        )
                                                        .clicked()
                                                    {
                                                        pending_move =
                                                            previous.map(|previous| (i, previous));
                                                        ui.close_menu();
                                                    }
                                                    let next = bookmark_peer(&self.bookmarks, i, 1);
                                                    if ui
                                                        .add_enabled(
                                                            next.is_some(),
                                                            egui::Button::new(crate::i18n::text(
                                                                "Вниз",
                                                            )),
                                                        )
                                                        .clicked()
                                                    {
                                                        pending_move = next.map(|next| (i, next));
                                                        ui.close_menu();
                                                    }
                                                });
                                                if ui
                                                    .button(crate::i18n::text(
                                                        "Убрать из избранного",
                                                    ))
                                                    .clicked()
                                                {
                                                    remove = Some(i);
                                                    ui.close_menu();
                                                }
                                            });
                                        });
                                    }
                                }
                                if let Some((from, to)) = pending_move {
                                    move_bookmark(&mut self.bookmarks, from, to);
                                }
                                if let Some(i) = remove {
                                    self.bookmarks.remove(i);
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
                    let mut hidden = self.folder_view(self.active).hidden;
                    let response = ui
                        .checkbox(&mut hidden, crate::i18n::text("Скрытые файлы"))
                        .on_hover_text(self.settings.shortcuts.hint(
                            ctx,
                            Command::Hidden,
                            Command::Hidden.label(),
                        ));
                    paint_focus(ui, &response, 5);
                    if response.changed() {
                        actions.push(
                            self.folder_view_action(self.active, FolderViewChange::Hidden(hidden)),
                        );
                    }
                });
                card.end(panel_ui);
            });
    }
    fn footer(&mut self, ctx: &egui::Context) {
        let colors = Palette::from_ctx(ctx);
        let mut stop_requested = false;
        let history_error = self.history.error.is_some();
        let history_button = |ui: &mut egui::Ui| {
            let response = ui.button(crate::i18n::text("Журнал результатов"));
            paint_focus(ui, &response, 5);
            if history_error {
                ui.colored_label(colors.error, crate::i18n::text("История не сохранена"));
            }
            response.clicked()
        };
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(colors.bg)
                    .inner_margin(egui::Margin::symmetric(18, 10)),
            )
            .show(ctx, |ui| {
                if let Some(job) = &mut self.job {
                    ui.horizontal(|ui| {
                        self.show_history |= history_button(ui);
                        ui.spinner();
                        ui.label(crate::tr_format!(
                            "Файловые операции: {} · {} с",
                            "File operations: {} · {} s",
                            job.label,
                            job.started.elapsed().as_secs()
                        ));
                        if self.transfer.is_some() {
                            let stopping = job.progress.cancel.is_requested();
                            let response = ui
                                .add_enabled(
                                    !stopping,
                                    egui::Button::new(if stopping {
                                        crate::i18n::text("Остановка…")
                                    } else {
                                        crate::i18n::text("Остановить")
                                    }),
                                )
                                .on_hover_text(crate::i18n::text("Остановить перенос · Shift+Esc"));
                            paint_focus(ui, &response, 5);
                            stop_requested = response.clicked();
                        }
                    });
                    operation_details(ui, job.progress.snapshot());
                    if let Some((message, _)) = &self.notice {
                        egui::ScrollArea::vertical()
                            .id_salt("active_operation_notice")
                            .max_height(80.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(message).color(colors.muted))
                                        .wrap()
                                        .selectable(true),
                                );
                            });
                    }
                } else if let Some((message, error)) = self.notice.clone() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        self.show_history |= history_button(ui);
                        if ui.small_button("×").clicked() {
                            self.notice = None;
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("operation_notice")
                            .max_height(160.0)
                            .show(ui, |ui| {
                                // Scroll content must use ordinary vertical layout. A
                                // wrapped horizontal layout gives the first text row
                                // the viewport's height and clips subsequent paths.
                                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                                    ui.add(
                                        egui::Label::new(RichText::new(message).color(if error {
                                            colors.error
                                        } else {
                                            colors.success
                                        }))
                                        .wrap()
                                        .selectable(true),
                                    );
                                });
                            });
                    });
                } else {
                    ui.horizontal(|ui| {
                        self.show_history |= history_button(ui);
                        let visible = self.tab().visible(self.folder_view(self.active)).len();
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
                                RichText::new(if self.tab().path.is_archive() {
                                    crate::archive::readonly()
                                } else {
                                    crate::i18n::text("Двойной щелчок — открыть")
                                })
                                .size(11.0)
                                .color(colors.muted),
                            );
                        });
                    });
                }
                self.pending_queue_ui(ui);
            });
        if stop_requested {
            self.request_transfer_stop(ctx);
        }
    }

    fn move_pending_job(&mut self, index: usize, direction: isize) {
        if let Some(other) = index.checked_add_signed(direction) {
            if index < self.pending_jobs.len() && other < self.pending_jobs.len() {
                self.pending_jobs.swap(index, other);
            }
        }
    }

    fn pending_queue_ui(&mut self, ui: &mut egui::Ui) {
        if self.pending_jobs.is_empty() {
            self.queue_start_failed = false;
            return;
        }
        if self.queue_start_failed {
            let retry = ui.button(crate::i18n::text("Повторить запуск очереди"));
            paint_focus(ui, &retry, 5);
            if retry.clicked() {
                self.queue_start_failed = false;
                self.start_next_job(ui.ctx());
            }
        }
        let mut remove = None;
        let mut reorder = None;
        egui::CollapsingHeader::new(crate::tr_format!(
            "Очередь: {} · одновременно: 1",
            "Queued: {} · concurrency: 1",
            self.pending_jobs.len()
        ))
        .id_salt("pending_file_jobs")
        .show(ui, |ui| {
            ui.label(crate::i18n::text("Остановка действует только на текущее задание. Ожидающие задания хранятся до закрытия приложения."));
            egui::ScrollArea::vertical().id_salt("pending_jobs_scroll").max_height(120.0).show(ui, |ui| {
                for (index, job) in self.pending_jobs.iter().enumerate() {
                    ui.push_id(index, |ui| {
                        ui.horizontal(|ui| {
                            let up = ui.add_enabled(index > 0, egui::Button::new("↑")).on_hover_text(crate::i18n::text("Раньше"));
                            paint_focus(ui, &up, 5);
                            if up.clicked() { reorder = Some((index, -1)); }
                            let down = ui.add_enabled(index + 1 < self.pending_jobs.len(), egui::Button::new("↓")).on_hover_text(crate::i18n::text("Позже"));
                            paint_focus(ui, &down, 5);
                            if down.clicked() { reorder = Some((index, 1)); }
                            let cancel = ui.button(crate::i18n::text("Удалить из очереди"));
                            paint_focus(ui, &cancel, 5);
                            if cancel.clicked() { remove = Some(index); }
                            ui.add(egui::Label::new(format!("{}. {}", index + 1, job.description)).truncate()).on_hover_text(&job.description);
                        });
                    });
                }
            });
        });
        if let Some(index) = remove {
            self.pending_jobs.remove(index);
            if self.pending_jobs.is_empty() {
                self.queue_start_failed = false;
            }
        } else if let Some((index, direction)) = reorder {
            self.move_pending_job(index, direction);
        }
    }

    fn workspace_places(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        egui::CollapsingHeader::new(crate::i18n::text("Пространства"))
            .id_salt("workspaces_group")
            .default_open(true)
            .show_unindented(ui, |ui| {
                ui.add_enabled_ui(self.drop_enabled(), |ui| {
                    let save = ui.button(crate::i18n::text("Сохранить пару папок…"));
                    paint_focus(ui, &save, 5);
                    if save.clicked() {
                        actions.push(Action::SaveWorkspace);
                    }
                    for (index, workspace) in self.workspaces.iter().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                let open = ui
                                    .add_sized(
                                        [(ui.available_width() - 34.0).max(0.0), 29.0],
                                        egui::Button::new(&workspace.name).truncate(),
                                    )
                                    .on_hover_text(&workspace.name);
                                paint_focus(ui, &open, 5);
                                if open.gained_focus() {
                                    open.scroll_to_me(Some(egui::Align::Center));
                                }
                                if open.clicked() {
                                    actions.push(Action::OpenWorkspace(index));
                                }
                                let menu = ui.menu_button("…", |ui| {
                                    if ui.button(crate::i18n::text("Переименовать…")).clicked()
                                    {
                                        actions.push(Action::RenameWorkspace(index));
                                        ui.close_menu();
                                    }
                                    if ui
                                        .button(crate::i18n::text("Удалить пространство"))
                                        .clicked()
                                    {
                                        actions.push(Action::RemoveWorkspace(index));
                                        ui.close_menu();
                                    }
                                });
                                let label = crate::tr_format!(
                                    "Действия для «{}»",
                                    "Actions for {}",
                                    workspace.name
                                );
                                ui.ctx().accesskit_node_builder(menu.response.id, |node| {
                                    node.set_label(label.clone());
                                });
                                paint_focus(ui, &menu.response.on_hover_text(label), 5);
                            });
                        });
                    }
                });
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
                    let label = ui.label(crate::i18n::text("Сервер"));
                    ui.text_edit_singleline(&mut form.host).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Порт"));
                    ui.add(egui::DragValue::new(&mut form.port).range(1..=65535)).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Пользователь"));
                    ui.text_edit_singleline(&mut form.user).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Начальная папка"));
                    ui.text_edit_singleline(&mut form.path).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Пароль"));
                    ui.add(egui::TextEdit::singleline(&mut form.password).password(true)).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Приватный ключ"));
                    ui.add(egui::TextEdit::singleline(&mut form.key).hint_text(crate::i18n::text("Путь к ключу (необязательно)"))).labelled_by(label.id);
                    ui.end_row();
                    let label = ui.label(crate::i18n::text("Пароль ключа"));
                    ui.add(egui::TextEdit::singleline(&mut form.passphrase).password(true)).labelled_by(label.id);
                    ui.end_row();
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
                ui.label(crate::i18n::text("Действующие сочетания показаны в разделе Настройки → Горячие клавиши."));
                ui.add_space(14.0); ui.label(crate::i18n::text("Для локальных файлов используется системный буфер, если он доступен. Для SFTP используется буфер PawCommander."));
                ui.label(crate::i18n::text("При совпадении имён выберите действие в диалоге. Удаление SFTP — навсегда с подтверждением; локальные файлы отправляются в корзину."));
            });
        }
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let mut cancel = false;
        let mut confirm = false;
        let title = match &dialog {
            Dialog::SelectionRule { command, .. } => command.label(),
            Dialog::NewFolder { .. } => crate::i18n::text("Новая папка"),
            Dialog::Rename { .. } => crate::i18n::text("Переименовать"),
            Dialog::WorkspaceName { editing, .. } => {
                if editing.is_some() {
                    crate::i18n::text("Переименовать рабочее пространство")
                } else {
                    crate::i18n::text("Сохранить пару папок")
                }
            }
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
                Dialog::SelectionRule { kind, input, scan, .. } => {
                    let (field, hint) = match kind {
                        RuleKind::Mask => (crate::i18n::text("Маска имени файла"), "photo-?.jpg"),
                        RuleKind::Extension => (crate::i18n::text("Расширение"), "txt"),
                    };
                    let label = ui.label(field);
                    let response = ui
                        .add_enabled(
                            scan.is_none(),
                            egui::TextEdit::singleline(input)
                                .hint_text(hint)
                                .desired_width(f32::INFINITY)
                                .return_key(None),
                        )
                        .labelled_by(label.id);
                    if self.dialog_focus {
                        response.request_focus();
                    }
                    if matches!(kind, RuleKind::Mask) {
                        ui.weak(crate::i18n::text("Звёздочка * заменяет любое число символов, знак ? — один символ."));
                    }
                    if let Some(scan) = scan {
                        let progress = scan.next as f32 / scan.rows.len().max(1) as f32;
                        ui.add(egui::ProgressBar::new(progress)
                            .text(crate::i18n::text("Поиск подходящих объектов…")));
                    } else if !input.is_empty() && kind.pattern(input).is_none() {
                        let error = match kind {
                            RuleKind::Mask => "Длина маски не должна превышать 1024 байта.",
                            RuleKind::Extension => "Введите одно расширение, например txt или .txt.",
                        };
                        ui.colored_label(colors.error, crate::i18n::text(error));
                    }
                }
                Dialog::NewFolder { parent, name } => {
                    ui.label(RichText::new(parent.display().to_string()).color(colors.muted));
                    let response = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text(crate::i18n::text("Имя папки"))
                            .desired_width(f32::INFINITY),
                    );
                    ctx.accesskit_node_builder(response.id, |node| node.set_label(crate::i18n::text("Имя папки")));
                    if self.dialog_focus {
                        response.request_focus();
                    }
                }
                Dialog::Rename { source, name } => {
                    ui.label(RichText::new(source.display().to_string()).color(colors.muted));
                    let response =
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    ctx.accesskit_node_builder(response.id, |node| node.set_label(crate::i18n::text("Имя файла")));
                    if self.dialog_focus {
                        response.request_focus();
                    }
                }
                Dialog::WorkspaceName { editing, name } => {
                    if editing.is_none() {
                        for (label, pane) in [
                            (crate::i18n::text("Левая папка"), &self.panes[0]),
                            (crate::i18n::text("Правая папка"), &self.panes[1]),
                        ] {
                            ui.label(crate::tr_format!(
                                "{label}: {}",
                                "{label}: {}",
                                pane.tab().path.display()
                            ));
                        }
                    }
                    let label = ui.label(crate::i18n::text("Имя рабочего пространства"));
                    let response = ui
                        .add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY))
                        .labelled_by(label.id);
                    if self.dialog_focus {
                        response.request_focus();
                    }
                    if self.workspace_name_taken(name, *editing) {
                        ui.colored_label(
                            colors.error,
                            crate::i18n::text("Это имя уже используется."),
                        );
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
            ui.add_space(16.0);
            let valid = match &dialog {
                Dialog::SelectionRule { kind, input, scan, .. } => {
                    scan.is_none() && kind.pattern(input).is_some()
                }
                Dialog::NewFolder { name, .. } | Dialog::Rename { name, .. } => {
                    !name.trim().is_empty()
                }
                Dialog::WorkspaceName { editing, name } => {
                    !name.trim().is_empty() && !self.workspace_name_taken(name, *editing)
                }
                Dialog::Trash { .. } => true,
            };
            ui.horizontal(|ui| {
                let cancel_button = ui.button(crate::i18n::text("Отмена"));
                if self.dialog_focus && matches!(dialog, Dialog::Trash { .. }) {
                    cancel_button.request_focus();
                }
                cancel = cancel_button.clicked();
                let label = if matches!(dialog, Dialog::Trash { .. }) {
                    if matches!(&dialog, Dialog::Trash { paths } if paths.iter().any(Location::remote)) { crate::i18n::text("Удалить навсегда") } else { crate::i18n::text("В корзину") }
                } else if matches!(dialog, Dialog::SelectionRule { .. }) {
                    crate::i18n::text("Выделить")
                } else {
                    crate::i18n::text("Сохранить")
                };
                let button = if self.settings.high_contrast {
                    egui::Button::new(label)
                } else {
                    egui::Button::new(RichText::new(label).color(Color32::WHITE))
                        .fill(Color32::from_rgb(45, 105, 220))
                };
                confirm = ui.add_enabled(valid, button).clicked();
            });
            let submit_on_enter = match dialog {
                Dialog::Trash { .. } => false,
                // The Enter that opens the dialog belongs to its sidebar control.
                Dialog::WorkspaceName { .. } | Dialog::SelectionRule { .. } => !self.dialog_focus,
                _ => true,
            };
            if submit_on_enter && valid && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                confirm = true;
            }
            self.dialog_focus = false;
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
        });
        if matches!(dialog, Dialog::SelectionRule { .. }) && (confirm || cancel) {
            let key = if cancel {
                egui::Key::Escape
            } else {
                egui::Key::Enter
            };
            if ctx.input(|input| input.key_pressed(key)) {
                self.guard_picker_close_key(ctx, &[key], key);
            }
        }
        if let Dialog::SelectionRule {
            kind,
            tab,
            location,
            input,
            scan,
            ..
        } = &mut dialog
        {
            if cancel {
                ctx.request_repaint();
                return;
            }
            if scan.is_none() && !confirm {
                self.dialog = Some(dialog);
                return;
            }
            let current = self.tab();
            let rows = current.visible(self.folder_view(self.active));
            if current.id != *tab
                || current.path != *location
                || scan
                    .as_ref()
                    .is_some_and(|scan| !Rc::ptr_eq(&scan.rows, &rows))
            {
                self.notice = Some((
                    crate::i18n::text("Папка или список изменились. Повторите выделение.").into(),
                    false,
                ));
                ctx.request_repaint();
                return;
            }
            if confirm {
                *scan = kind
                    .compile(input)
                    .map(|rule| SelectionScan::new(rule, rows));
            }
            if let Some(scan) = scan {
                if scan.step(&current.entries, Instant::now() + Duration::from_millis(4)) {
                    self.apply_selection(
                        SelectionChange::Matches(std::mem::take(&mut scan.matches)),
                        ctx,
                    );
                    return;
                }
            }
            self.dialog = Some(dialog);
            ctx.request_repaint();
            return;
        }
        if confirm && !cancel {
            match dialog {
                Dialog::SelectionRule { .. } => unreachable!("rule handled above"),
                Dialog::NewFolder { parent, name } => self.start_described_job(
                    ctx,
                    crate::i18n::text("Создание папки"),
                    format!(
                        "{}: {}",
                        crate::i18n::text("Создание папки"),
                        parent.join(&name)
                    ),
                    move |progress| {
                        let target = parent.join(&name);
                        progress
                            .results
                            .plan(vec![(parent.clone(), Some(target.display()))]);
                        progress.results.begin(&parent);
                        progress.item(&parent.path().join(&name));
                        fs_ops::create_directory(&parent, &name).map(|p| {
                            progress.results.target(&parent, p.display());
                            crate::tr_format!(
                                "Папка создана: {}",
                                "Folder created: {}",
                                p.display()
                            )
                        })
                    },
                ),
                Dialog::Rename { source, name } => self.start_described_job(
                    ctx,
                    crate::i18n::text("Переименование"),
                    format!(
                        "{}: {} → {}",
                        crate::i18n::text("Переименование"),
                        source,
                        name
                    ),
                    move |progress| {
                        let target = source.parent().map(|parent| parent.join(&name).display());
                        progress.results.plan(vec![(source.clone(), target)]);
                        progress.results.begin(&source);
                        progress.item(source.path());
                        fs_ops::rename_entry(&source, &name).map(|p| {
                            progress.results.target(&source, p.display());
                            crate::tr_format!("Переименовано: {}", "Renamed: {}", p.display())
                        })
                    },
                ),
                Dialog::WorkspaceName { editing, name } => {
                    self.save_workspace_name(editing, name);
                }
                Dialog::Trash { paths } => self.start_described_job(
                    ctx,
                    crate::i18n::text("Удаление файлов"),
                    format!(
                        "{} ({}): {}",
                        crate::i18n::text("Удаление файлов"),
                        paths.len(),
                        paths.first().map(Location::display).unwrap_or_default()
                    ),
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
        if self.dialog.is_none() && self.dialog_file_focus.is_some() {
            ctx.request_repaint();
        }
    }
}
impl eframe::App for PawCommander {
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        if let Some(accepted) = self.native_drag.finish_input(input) {
            if accepted {
                self.notice = Some((
                    crate::i18n::text("Файлы переданы принимающему приложению").into(),
                    false,
                ));
            }
        }

        self.pane_key_presses.clear();
        if !input.focused {
            self.tab_held = false;
            self.palette_release_key = None;
            return;
        }
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
        let pane_keys = (!ctx.wants_keyboard_input() || self.file_row_focus(ctx).is_some())
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

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.history.poll();
        self.recovery.poll(ctx);
        let colors = Palette::from_ctx(ctx);
        self.poll(ctx);
        self.transfer_stop_shortcut(ctx);
        if (self.job.is_some() || !self.pending_jobs.is_empty())
            && ctx.input(|i| i.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.notice = Some((
                crate::i18n::text("Дождитесь завершения файловой операции, затем закройте окно.")
                    .into(),
                false,
            ));
        }
        // Startup findings may arrive while another dialog or picker is open.
        // Suspend those handlers so Enter cannot submit an unrelated operation.
        if self.recovery.is_open() && !self.conflict_open() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label(crate::tr_format!(
                    "Закройте отчёт восстановления, чтобы продолжить работу.",
                    "Close the recovery report to continue."
                ));
            });
            self.recovery.show(ctx);
            return;
        }
        if !self.conflict_open() {
            if let Some(editor) = &mut self.shortcut_editor {
                editor.capture(ctx);
            }
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
        self.keyboard(ctx);
        let quick_look_focus = self.show_quick_look(ctx);
        let mut actions = vec![];
        let mut drop_targets = DropTargets::default();
        let mut tab_targets = TabDropTargets::default();
        self.toolbar(ctx, &mut actions);
        self.footer(ctx);
        self.history.show(ctx, &mut self.show_history);
        self.show_search_results(ctx);
        self.sidebar(ctx, &mut actions);
        let drop_enabled = self.drop_enabled() && !self.native_drag.active();
        let zed_available = self.tools.zed.is_some();
        if self.settings.preview {
            let selected = self.selected();
            let fingerprint = selected
                .first()
                .and_then(|path| self.tab().entries.iter().find(|entry| &entry.path == path))
                .map(|entry| (entry.size, entry.modified));
            let narrow = ctx.screen_rect().width() < 900.0;
            egui::SidePanel::right("preview")
                .default_width(if narrow { 155.0 } else { 260.0 })
                .width_range(if narrow { 145.0..=165.0 } else { 200.0..=380.0 })
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
        self.thumbnails.begin_frame();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(colors.surface).inner_margin(8))
            .show(ctx, |ui| {
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
        if self.dialog.is_none() {
            if let Some(transfer) = &mut self.transfer {
                if let Some(conflict) = &mut transfer.conflict {
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
            }
        }
        self.restore_dialog_file_focus(ctx);
        if let Some(id) = quick_look_focus {
            if !self.keyboard_blocked(ctx) && self.tab().rendered_rows.contains_key(&id) {
                ctx.memory_mut(|memory| memory.request_focus(id));
                ctx.request_repaint();
            }
        }
        if let Some(id) = self.palette_focus_to_restore.take() {
            if !self.keyboard_blocked(ctx) && !ctx.wants_keyboard_input() {
                ctx.memory_mut(|memory| memory.request_focus(id));
                ctx.request_repaint();
            }
        }
        if self.command_palette.is_some() {
            self.show_command_palette(ctx, &picker_keys);
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
                crate::i18n::text("Сочетание избранного очищено: клавиша занята другой командой")
                    .into(),
                false,
            ));
        }
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
        self.keyboard_had_focus = ctx.wants_keyboard_input() && self.file_row_focus(ctx).is_none();
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
        eframe::set_value(storage, Self::FOLDER_VIEWS_KEY, &saved_views);
        self.search.save(storage);
    }
}

#[derive(Clone, Copy)]
struct PaneOptions<'a> {
    index: usize,
    active: bool,
    settings: &'a Settings,
    folder_views: &'a FolderViews,
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
    let mut clicked_tab = None;
    let mut tab_context_menu = None;
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
                            let can_close = pane.can_close_tab(i);
                            let body = if can_close {
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
                                let label = if tab.pinned {
                                    crate::tr_format!("{name} (закреплена)", "{name} (pinned)")
                                } else {
                                    name.clone()
                                };
                                egui::WidgetInfo::selected(
                                    egui::WidgetType::SelectableLabel,
                                    true,
                                    selected,
                                    label,
                                )
                            });
                            ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_role(egui::accesskit::Role::Tab);
                                node.set_selected(selected);
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
                            let text_right = if tab.pinned {
                                close_rect.left()
                            } else {
                                body.right()
                            };
                            let text_rect = egui::Rect::from_min_max(
                                rect.min + Vec2::new(32.0, 0.0),
                                egui::pos2(text_right - 3.0, rect.bottom()),
                            );
                            let text = if text.size().x > text_rect.width() {
                                let mut job = egui::text::LayoutJob::simple(
                                    name.clone(),
                                    FontId::proportional(13.0),
                                    colors.ink,
                                    text_rect.width(),
                                );
                                job.wrap.max_rows = 1;
                                job.wrap.break_anywhere = true;
                                ui.painter().layout_job(job)
                            } else {
                                text
                            };
                            ui.painter()
                                .with_clip_rect(text_rect.intersect(ui.clip_rect()))
                                .galley(
                                    egui::pos2(
                                        rect.left() + 32.0,
                                        rect.center().y - text.size().y / 2.0,
                                    ),
                                    text,
                                    colors.ink,
                                );
                            targets.add(ui, rect, index, path, true);
                            if (changed && selected) || response.gained_focus() {
                                response.scroll_to_me(Some(egui::Align::Center));
                            }
                            paint_focus(ui, &response, 5);
                            if idle && response.drag_started_by(egui::PointerButton::Primary) {
                                egui::DragAndDrop::set_payload(
                                    ui.ctx(),
                                    DragTab {
                                        id: tab.id,
                                        pane: index,
                                        label: name.clone(),
                                        pinned: tab.pinned,
                                    },
                                );
                            } else if response.clicked() {
                                clicked_tab = Some(i);
                                actions.push(Action::Focus(index));
                            }
                            if !dragging {
                                response.context_menu(|ui| {
                                    tab_context_menu = Some(response.id);
                                    let pin = if tab.pinned {
                                        crate::i18n::text("Открепить вкладку")
                                    } else {
                                        crate::i18n::text("Закрепить вкладку")
                                    };
                                    let reopen = Command::ReopenTab.label();
                                    for (label, action, enabled) in [
                                        (pin, Action::TogglePinTab(index, i), true),
                                        (
                                            Command::DuplicateTab.label(),
                                            Action::DuplicateTab(index, i),
                                            true,
                                        ),
                                        (
                                            Command::CloseTab.label(),
                                            Action::CloseTab(index, i),
                                            can_close,
                                        ),
                                        (
                                            reopen,
                                            Action::ReopenTab(index),
                                            !pane.closed_tabs.is_empty(),
                                        ),
                                    ] {
                                        if ui
                                            .add_enabled(enabled, egui::Button::new(label))
                                            .clicked()
                                        {
                                            actions.push(action);
                                            ui.close_menu();
                                        }
                                    }
                                });
                                response
                                    .on_hover_text(path.display())
                                    .on_hover_cursor(egui::CursorIcon::Grab);
                            }
                            if tab.pinned {
                                icons::paint(ui, close_rect.shrink(5.0), Icon::Pin, colors.mauve);
                            } else if can_close {
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
                                if close.gained_focus() {
                                    close.scroll_to_me(Some(egui::Align::Center));
                                }
                                paint_focus(ui, &close, 5);
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
            paint_focus(ui, &response, 5);
            if response
                .on_hover_text(crate::i18n::text("Новая вкладка"))
                .clicked()
            {
                actions.push(Action::NewTab(index));
            }
        })
        .response
        .rect;
    if let Some(index) = clicked_tab {
        pane.select_tab(index);
    }
    pane.tab_context_menu = tab_context_menu;
    tab_targets.add_strip(
        ui,
        index,
        strip,
        pane.tabs.len(),
        pane.pinned_count(),
        visible_tabs,
    );
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
                            crate::i18n::text("Ввести путь вручную"),
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
    ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_label(crate::i18n::text("Путь к папке"));
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
    if response.changed() {
        tab.address_completion
            .address_changed(&tab.address, &tab.path);
        tab.address_error = None;
        suggestions = tab
            .address_completion
            .suggestions(&tab.address, &tab.path, ui.ctx());
    }
    if let Some(step) = step.filter(|_| !response.changed() && !suggestions.is_empty()) {
        let count = suggestions.len();
        tab.address_completion.selected = Some(match tab.address_completion.selected {
            Some(selected) => (selected as isize + step).rem_euclid(count as isize) as usize,
            None if step > 0 => 0,
            None => count - 1,
        });
    }
    let completed = if let Some(item) = (complete && !response.changed())
        .then(|| suggestions.get(tab.address_completion.selected.unwrap_or(0)))
        .flatten()
    {
        tab.address_completion.remember_completion(&item.path);
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
        true
    } else {
        false
    };
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
            match tab.address_completion.resolve(&tab.address, &tab.path) {
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
    if tab.edit_address {
        if completed {
            // egui may already be advancing focus to the next widget on Tab.
            // Restore focus after the buttons and popup have registered.
            response.request_focus();
        }
        if response.has_focus() {
            actions.push(Action::Focus(index));
        }
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
    if response.gained_focus() {
        response.scroll_to_me(Some(egui::Align::Center));
    }
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
    paint_focus(ui, &response, 5);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn sort_controls(ui: &mut egui::Ui, order: FileOrder, mut choose: impl FnMut(Sort)) {
    for (sort, column) in [
        (Sort::Name, Column::Name),
        (Sort::Size, Column::Size),
        (Sort::Modified, Column::Modified),
    ] {
        let label = column.label();
        let selected = order.key == sort;
        let title = if selected {
            format!("{label} {}", if order.descending { "↓" } else { "↑" })
        } else {
            label.to_owned()
        };
        let response = ui.selectable_label(selected, title);
        paint_focus(ui, &response, 4);
        if response.clicked() {
            choose(sort);
            ui.close_menu();
        }
    }
}

fn list_header(
    ui: &mut egui::Ui,
    layout: &ColumnLayout,
    order: FileOrder,
    padding: f32,
    compact: bool,
    mut choose_sort: impl FnMut(Sort),
) {
    if !compact {
        ui.add_space(4.0);
    }
    let height = if compact { 22.0 } else { 36.0 };
    let (header, _) = ui.allocate_exact_size(Vec2::new(layout.width, height), Sense::hover());
    let band = header.expand2(Vec2::new(padding, 0.0));
    let colors = Palette::from_ctx(ui.ctx());
    ui.painter().rect_filled(band, 0, colors.raised);
    for y in [band.top(), band.bottom()] {
        ui.painter().hline(
            band.x_range(),
            y,
            Stroke::new(1.0, workspace_border(colors)),
        );
    }
    for (column, rect) in layout.cells(header) {
        let label = column.label();
        let sort = match column {
            Column::Name => Some(Sort::Name),
            Column::Size => Some(Sort::Size),
            Column::Modified => Some(Sort::Modified),
            _ => None,
        };
        let response = sort.map(|sort| {
            let response = ui.interact(rect, ui.id().with(column as u8), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    ui.is_enabled(),
                    crate::tr_format!("Сортировка: {label}", "Sort: {label}"),
                )
            });
            paint_focus(ui, &response, 4);
            if response.clicked() {
                choose_sort(sort);
            }
            response
        });
        let painter = ui.painter().with_clip_rect(rect);
        let text = painter.text(
            rect.left_center() + Vec2::new(8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            if response.is_some_and(|response| response.hovered()) {
                colors.ink
            } else {
                colors.muted
            },
        );
        if sort == Some(order.key) {
            let x = text.right() + 6.0;
            let y = rect.center().y;
            let direction = if order.descending { -1.0 } else { 1.0 };
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(x, y + 2.0 * direction),
                    egui::pos2(x + 4.0, y - 2.0 * direction),
                    egui::pos2(x + 8.0, y + 2.0 * direction),
                ],
                Stroke::new(1.2, colors.muted),
            ));
        }
    }
}

fn list_notice(ui: &mut egui::Ui, tab: &Tab, empty: bool, compact: bool) {
    let colors = Palette::from_ctx(ui.ctx());
    if let Some(error) = &tab.error {
        ui.colored_label(colors.error, error);
    }
    if tab.blocking_load() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(if tab.path.is_archive() {
                crate::i18n::text("Чтение архива…")
            } else {
                crate::i18n::text("Чтение папки…")
            });
        });
    }
    if empty && !tab.blocking_load() && tab.error.is_none() {
        if !compact {
            ui.add_space(50.0);
        }
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(if tab.filter.is_empty() {
                    crate::i18n::text("Папка пуста")
                } else {
                    crate::i18n::text("Ничего не найдено")
                })
                .size(if compact { 13.0 } else { 20.0 })
                .color(colors.muted),
            );
            if !compact && (!tab.path.is_archive() || !tab.filter.is_empty()) {
                ui.label(
                    RichText::new(if tab.filter.is_empty() {
                        crate::i18n::text("Создайте папку или вставьте файлы")
                    } else {
                        crate::i18n::text("Попробуйте другое имя")
                    })
                    .size(12.0)
                    .color(colors.muted),
                );
            }
        });
    }
}

fn render_pane(
    ui: &mut egui::Ui,
    pane: &mut Pane,
    options: PaneOptions<'_>,
    actions: &mut Vec<Action>,
    drop_targets: &mut DropTargets,
    tab_targets: &mut TabDropTargets,
    mut thumbnails: Option<&mut Thumbnails>,
) {
    let PaneOptions {
        index: pane_index,
        active,
        settings,
        folder_views,
        idle,
        zed_available,
    } = options;
    let colors = Palette::from_ctx(ui.ctx());
    // Below this height the regular pane chrome can obscure a grid tile's name.
    let compact = ui.available_height() < 280.0;
    let sort_label = crate::i18n::text("Сортировка");
    ui.push_id(pane_index, |ui| {
        if compact {
            ui.spacing_mut().item_spacing.y = 1.0;
        }
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
            bottom: if compact { 1 } else { PANE_PADDING + 1 },
        });
        let frame = frame.show(ui, |pane_ui| {
            pane_ui.set_min_height((pane_ui.available_height() - 1.0).max(0.0));
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
            let tab = &mut pane.tabs[pane.current];
            let view = tab.folder_view(settings, pane_index, folder_views);
            let FolderView {
                mode,
                order,
                density,
                ..
            } = view;
            let grid = mode == FolderMode::Grid;
            let columns_mode = mode == FolderMode::Columns;
            let compact_rows = compact || density == Density::Compact;
            tab.rendered_rows.clear();
            address_bar(ui, tab, pane_index, actions, drop_targets);
            let response = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let show_sort = compact || columns_mode;
                    let controls = (3.0 + if show_sort { 1.0 } else { 0.0 })
                        * (icons::COMPACT_SIZE + ui.spacing().item_spacing.x);
                    let filter_width = (ui.available_width() - controls).max(12.0);
                    let response = ui.add_sized(
                        [filter_width, icons::COMPACT_SIZE],
                        egui::TextEdit::singleline(&mut tab.filter)
                            .vertical_align(egui::Align::Center)
                            .margin(egui::Margin::symmetric(5, 0))
                            .hint_text(crate::i18n::text("Фильтр по имени…")),
                    );
                    for choice in [FolderMode::Grid, FolderMode::List, FolderMode::Columns] {
                        if icons::compact_button(ui, choice.icon(), choice.label(), mode == choice)
                            .clicked()
                        {
                            actions.push(
                                tab.folder_view_action(pane_index, FolderViewChange::Mode(choice)),
                            );
                        }
                    }
                    if show_sort {
                        let menu =
                            ui.menu_button("↕", |ui| {
                                sort_controls(ui, order, |sort| {
                                    actions.push(tab.folder_view_action(
                                        pane_index,
                                        FolderViewChange::Sort(sort),
                                    ));
                                });
                            });
                        ui.ctx().accesskit_node_builder(menu.response.id, |node| {
                            node.set_label(sort_label);
                        });
                        paint_focus(ui, &menu.response, 4);
                        menu.response.on_hover_text(sort_label);
                    }
                    response
                })
                .inner;
            ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_label(crate::i18n::text("Фильтр по имени"));
            });
            if tab.focus_filter {
                response.request_focus();
                tab.focus_filter = false;
            }
            if response.has_focus() {
                actions.push(Action::Focus(pane_index));
            }
            if response.changed() {
                tab.column_reveal = None;
            }
            if response.changed() && !tab.selected.is_empty() {
                let visible_paths: BTreeSet<_> = tab
                    .visible(view)
                    .iter()
                    .map(|i| &tab.entries[*i].path)
                    .filter(|path| tab.selected.contains(*path))
                    .collect();
                tab.selected.retain(|p| visible_paths.contains(p));
            }
            let viewport_x = ui.available_rect_before_wrap().x_range();
            let available_width = viewport_x.span();
            let layout = if grid {
                ColumnLayout::responsive(available_width)
            } else {
                settings.list_columns.layout(available_width)
            };
            const STATUS_HEIGHT: f32 = 22.0;
            const STATUS_RULE_HEIGHT: f32 = 1.0;
            let spacing = ui.spacing().item_spacing.y;
            let status_height = if !compact {
                STATUS_HEIGHT + STATUS_RULE_HEIGHT + 2.0 * spacing
            } else {
                0.0
            };
            let scrolling_columns = layout.width > available_width;
            let tab_id = tab.id;
            let mut render_files = |ui: &mut egui::Ui| {
                if !compact || (!grid && settings.list_columns.is_custom()) {
                    list_header(
                        ui,
                        &layout,
                        order,
                        f32::from(PANE_PADDING),
                        compact,
                        |sort| {
                            actions.push(
                                tab.folder_view_action(pane_index, FolderViewChange::Sort(sort)),
                            );
                        },
                    );
                }
                drop_targets.add(
                    ui,
                    ui.available_rect_before_wrap(),
                    pane_index,
                    &tab.path,
                    !tab.blocking_load() && tab.error.is_none(),
                );
                let visible = tab.visible(view);
                if tab.error.is_some() || tab.blocking_load() || visible.is_empty() {
                    let notice_rect = egui::Rect::from_x_y_ranges(
                        viewport_x,
                        ui.available_rect_before_wrap().y_range(),
                    );
                    ui.scope_builder(egui::UiBuilder::new().max_rect(notice_rect), |ui| {
                        list_notice(ui, tab, visible.is_empty(), compact);
                    });
                }
                let row_height = if compact_rows {
                    COMPACT_FILE_ROW_HEIGHT
                } else if grid {
                    104.0
                } else {
                    36.0
                };
                let columns = if grid {
                    (ui.available_width() / 108.0).floor().max(1.0) as usize
                } else {
                    1
                };
                let rows = visible.len().div_ceil(columns);
                let reserved_status_height = if scrolling_columns {
                    0.0
                } else {
                    status_height
                };
                let files_height = (ui.available_height() - reserved_status_height).max(0.0);
                let mut scroll = egui::ScrollArea::vertical();
                let scope = ui.id();
                let previous_scope = tab.file_scroll_scope;
                let row_pitch = row_height + spacing;
                if !ui.is_sizing_pass() {
                    let previous_geometry = tab.file_scroll_geometry.replace((columns, row_pitch));
                    if previous_scope.is_some_and(|previous| previous != scope) {
                        tab.scroll_restore.get_or_insert(tab.scroll_offset);
                    }
                    tab.file_scroll_scope = Some(scope);
                    if let Some(position) = tab
                        .scroll_to
                        .take()
                        .and_then(|path| visible.iter().position(|i| tab.entries[*i].path == path))
                    {
                        tab.scroll_restore = None;
                        scroll =
                            scroll.vertical_scroll_offset((position / columns) as f32 * row_pitch);
                    } else if tab.has_complete_listing {
                        // Progressive batches can initially be too short to hold
                        // the offset or may still change which rows are at that offset.
                        let offset = tab.scroll_restore.take().or_else(|| {
                            let (old_columns, old_pitch) = previous_geometry?;
                            if (old_columns, old_pitch) == (columns, row_pitch) {
                                return None;
                            }
                            // Keep the first visible item in view when the grid reflows.
                            let old_row = tab.scroll_offset / old_pitch;
                            let item = old_row as usize * old_columns;
                            let row = item / columns;
                            Some((row as f32 + old_row.fract()) * row_pitch)
                        });
                        if let Some(offset) = offset {
                            let max_offset =
                                (rows as f32 * row_pitch - spacing - files_height).max(0.0);
                            scroll = scroll.vertical_scroll_offset(offset.min(max_offset));
                        }
                    }
                }
                let files_id = tab.file_list_id();
                let ctx = ui.ctx().clone();
                ctx.accesskit_node_builder(files_id, |node| {
                    node.set_role(egui::accesskit::Role::List);
                    let pane = crate::i18n::text(
                        match (
                            settings.split.then_some(settings.pane_layout.orientation),
                            pane_index,
                        ) {
                            (Some(PaneOrientation::Stacked), 0) => "Верхняя панель",
                            (Some(PaneOrientation::Stacked), _) => "Нижняя панель",
                            (_, 0) => "Левая панель",
                            _ => "Правая панель",
                        },
                    );
                    let view = if grid {
                        crate::i18n::text("Сетка")
                    } else {
                        crate::i18n::text("Список")
                    };
                    node.set_label(format!("{pane}, {view}, {}", tab.path.display()));
                });
                let file_scroll = ctx.with_accessibility_parent(files_id, || {
                    scroll
                        .max_height(files_height)
                        .min_scrolled_height(files_height)
                        .id_salt((tab.id, &tab.path, "files"))
                        .auto_shrink([false, false])
                        .show_rows(ui, row_height, rows, |ui, range| {
                            if grid && compact_rows {
                                ui.spacing_mut().interact_size.y = row_height;
                            }
                            for row in range {
                                if grid {
                                    ui.horizontal(|ui| {
                                        for col in 0..columns {
                                            let position = row * columns + col;
                                            if position < visible.len() {
                                                entry_widget(
                                                    ui,
                                                    tab,
                                                    position,
                                                    &visible,
                                                    EntryOptions {
                                                        columns: None,
                                                        thumbnails: thumbnails.as_deref_mut(),
                                                        pane: pane_index,
                                                        split: settings.split,
                                                        grid: true,
                                                        folder_columns: false,
                                                        compact: compact_rows,
                                                        idle,
                                                        zed_available,
                                                    },
                                                    actions,
                                                    drop_targets,
                                                );
                                            }
                                        }
                                    });
                                } else if row < visible.len() {
                                    entry_widget(
                                        ui,
                                        tab,
                                        row,
                                        &visible,
                                        EntryOptions {
                                            columns: Some(&layout),
                                            thumbnails: None,
                                            pane: pane_index,
                                            split: settings.split,
                                            grid: false,
                                            folder_columns: false,
                                            compact: compact_rows,
                                            idle,
                                            zed_available,
                                        },
                                        actions,
                                        drop_targets,
                                    );
                                }
                            }
                        })
                });
                if !ui.is_sizing_pass() {
                    let offset = file_scroll.state.offset.y;
                    if previous_scope == Some(scope) && offset > 0.0 && offset != tab.scroll_offset
                    {
                        // User scrolling in a partial listing takes precedence.
                        tab.scroll_restore = None;
                    }
                    tab.scroll_offset = offset;
                }
                visible.len()
            };
            let visible_count = if columns_mode {
                column_view::show(
                    ui,
                    tab,
                    &mut pane.columns,
                    options,
                    (ui.available_height() - status_height).max(0.0),
                    actions,
                    drop_targets,
                )
            } else if scrolling_columns {
                let height = (ui.available_height() - status_height).max(0.0);
                egui::ScrollArea::horizontal()
                    .id_salt(("file_columns", tab_id))
                    .min_scrolled_width(0.0)
                    .max_height(height)
                    .auto_shrink([false, false])
                    .drag_to_scroll(false)
                    .show(ui, |ui| {
                        ui.set_width(layout.width);
                        ui.style_mut().always_scroll_the_only_direction = false;
                        render_files(ui)
                    })
                    .inner
            } else {
                render_files(ui)
            };
            if !compact {
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
                                    visible_count
                                ))
                                .size(11.0)
                                .color(colors.muted),
                            );
                        });
                    },
                );
            }
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

const COMPACT_FILE_ROW_HEIGHT: f32 = 24.0;

struct EntryOptions<'a> {
    columns: Option<&'a ColumnLayout>,
    thumbnails: Option<&'a mut Thumbnails>,
    pane: usize,
    split: bool,
    grid: bool,
    folder_columns: bool,
    compact: bool,
    idle: bool,
    zed_available: bool,
}

fn entry_widget(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    position: usize,
    visible: &[usize],
    options: EntryOptions<'_>,
    actions: &mut Vec<Action>,
    drop_targets: &mut DropTargets,
) {
    let index = visible[position];
    let colors = Palette::from_ctx(ui.ctx());
    let EntryOptions {
        columns,
        thumbnails,
        pane,
        split,
        grid,
        folder_columns,
        compact,
        idle,
        zed_available,
    } = options;
    let entry = &tab.entries[index];
    let is_dir = entry.is_dir;
    let path = entry.path.clone();
    let archive_member = path.is_archive();
    let selected = tab.selected.contains(&path);
    let list_width = columns.map_or_else(|| ui.available_width(), |columns| columns.width);
    let size = if compact {
        let width = if grid { 100.0 } else { list_width };
        Vec2::new(width, COMPACT_FILE_ROW_HEIGHT)
    } else if grid {
        Vec2::new(100.0, 98.0)
    } else {
        Vec2::new(list_width, 36.0)
    };
    let (_, rect) = ui.allocate_space(size);
    let sense = if archive_member {
        Sense::click()
    } else {
        Sense::click_and_drag()
    };
    let response = ui.interact(rect, tab.entry_id(&path), sense);
    tab.rendered_rows.insert(response.id, path.clone());
    if entry.is_dir {
        drop_targets.add(
            ui,
            rect,
            pane,
            &path,
            !tab.blocking_load() && tab.error.is_none(),
        );
    }
    if response.has_focus() && (response.gained_focus() || tab.cursor.as_ref() != Some(&path)) {
        tab.cursor = Some(path.clone());
        actions.push(Action::Focus(pane));
    }
    if !grid && position.is_multiple_of(2) {
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
    let icon_rect = if compact {
        let left = if grid { 11.0 } else { 44.0 };
        egui::Rect::from_center_size(rect.left_center() + Vec2::new(left, 0.0), Vec2::splat(16.0))
    } else if grid {
        egui::Rect::from_center_size(rect.center_top() + Vec2::new(0.0, 30.0), Vec2::splat(37.0))
    } else {
        egui::Rect::from_min_size(rect.min + Vec2::new(32.0, 6.0), Vec2::splat(24.0))
    };
    let has_thumbnail = thumbnails.is_some_and(|thumbnails| {
        let thumbnail_rect = if compact {
            icon_rect
        } else {
            egui::Rect::from_center_size(icon_rect.center(), Vec2::new(74.0, 46.0))
        };
        !ui.is_sizing_pass()
            && ui.is_rect_visible(thumbnail_rect)
            && thumbnails.paint(ui, entry, thumbnail_rect)
    });
    if !has_thumbnail {
        let icon = icons::file_icon(&entry.name, entry.is_dir);
        icons::paint(ui, icon_rect, icon, icons::tint(icon, colors));
    }
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
    if grid && compact {
        let clip = egui::Rect::from_min_max(
            rect.left_top() + Vec2::new(23.0, 0.0),
            rect.right_bottom() - Vec2::new(3.0, 0.0),
        );
        ui.painter().with_clip_rect(clip).text(
            clip.left_center(),
            egui::Align2::LEFT_CENTER,
            &entry.name,
            FontId::proportional(12.0),
            name_color,
        );
    } else if grid {
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
    } else if folder_columns {
        let end_padding = if is_dir { 19.0 } else { 4.0 };
        let clip = egui::Rect::from_min_max(
            rect.left_top() + Vec2::new(57.0, 0.0),
            rect.right_bottom() - Vec2::new(end_padding, 0.0),
        );
        if clip.is_positive() {
            ui.painter().with_clip_rect(clip).text(
                clip.left_center(),
                egui::Align2::LEFT_CENTER,
                &entry.name,
                FontId::proportional(13.0),
                name_color,
            );
        }
        if is_dir && rect.width() >= 78.0 {
            ui.painter().text(
                rect.right_center() - Vec2::new(5.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                "›",
                FontId::proportional(15.0),
                colors.muted,
            );
        }
    } else {
        let fallback;
        let columns = if let Some(columns) = columns {
            columns
        } else {
            fallback = ColumnLayout::responsive(rect.width());
            &fallback
        };
        for (column, cell) in columns.cells(rect) {
            if !ui.is_rect_visible(cell) {
                continue;
            }
            let (padding, align, size, color, value) = match column {
                Column::Name => (
                    64.0,
                    egui::Align2::LEFT_CENTER,
                    14.0,
                    name_color,
                    std::borrow::Cow::Borrowed(entry.name.as_str()),
                ),
                Column::Size if !entry.is_dir => (
                    5.0,
                    egui::Align2::RIGHT_CENTER,
                    11.0,
                    colors.muted,
                    human_size(entry.size).into(),
                ),
                _ => (
                    5.0,
                    egui::Align2::RIGHT_CENTER,
                    11.0,
                    colors.muted,
                    column.metadata(entry).unwrap_or("—".into()),
                ),
            };
            let origin = if align == egui::Align2::LEFT_CENTER {
                cell.left_center() + Vec2::new(padding, 0.0)
            } else {
                cell.right_center() - Vec2::new(padding, 0.0)
            };
            ui.painter().with_clip_rect(cell).text(
                origin,
                align,
                value,
                FontId::proportional(size),
                color,
            );
        }
    }
    paint_focus(ui, &response, 5);
    if idle && !archive_member && response.drag_started_by(egui::PointerButton::Primary) {
        egui::DragAndDrop::set_payload(ui.ctx(), DragFiles(tab.drag_paths(index, visible)));
        actions.push(Action::Focus(pane));
    }
    let on_checkbox = !grid
        && response
            .interact_pointer_pos()
            .is_some_and(|p| check_rect.expand(4.0).contains(p));
    if response.clicked() && ui.memory(|memory| memory.allows_interaction(ui.layer_id())) {
        let pointer = response.clicked_by(egui::PointerButton::Primary);
        let mut modifiers = if pointer {
            ui.input(|i| i.modifiers)
        } else {
            egui::Modifiers::NONE
        };
        if !pointer || on_checkbox {
            modifiers.command = true;
        }
        tab.choose(index, visible, modifiers);
        if !pointer {
            response.request_focus();
        }
        actions.push(Action::Focus(pane));
    }
    if response.secondary_clicked() {
        tab.column_reveal = None;
        if !selected {
            tab.selected.clear();
            tab.selected.insert(path.clone());
            tab.anchor = Some(path.clone());
        }
        actions.push(Action::Focus(pane));
    }
    let open = || {
        if folder_columns {
            tab.column_action(pane, ColumnNavigation::Open(path.clone()))
        } else {
            Action::Open(pane, path.clone())
        }
    };
    if response.double_clicked() && !on_checkbox {
        actions.push(open());
    }
    response.widget_info(|| {
        let entry = &tab.entries[index];
        let kind = crate::i18n::text(if entry.is_symlink {
            "Символическая ссылка"
        } else if entry.is_dir {
            "Папка"
        } else {
            "Файл"
        });
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            ui.is_enabled(),
            tab.selected.contains(&path),
            format!("{}, {kind}", entry.name),
        )
    });
    response
        .on_hover_text(path.display().to_string())
        .context_menu(|ui| {
            if (!archive_member || is_dir) && ui.button(crate::i18n::text("Открыть")).clicked() {
                actions.push(open());
                ui.close_menu();
            }
            if archive_member {
                if ui.button(crate::i18n::text("Копировать путь")).clicked() {
                    actions.push(Action::Focus(pane));
                    actions.push(Action::CopyPath);
                    ui.close_menu();
                }
                return;
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
                (crate::i18n::text("Свойства…"), Action::Properties,
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
fn wait_for_conflict_answer(answer: &Receiver<Answer>, cancellation: &Cancellation) -> Answer {
    loop {
        if cancellation.is_requested() {
            break;
        }
        match answer.recv_timeout(Duration::from_millis(30)) {
            Ok(answer) if !cancellation.is_requested() => return answer,
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    Answer {
        decision: Decision::Cancel,
        apply_to_group: false,
    }
}

fn operation_details(ui: &mut egui::Ui, progress: &crate::operations::Snapshot) {
    if let Some(item) = &progress.item {
        let name = item.file_name().map_or_else(
            || item.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let phase = if progress.waiting_for_conflict {
            crate::i18n::text("Ожидается решение")
        } else if progress.verification.is_some() {
            crate::i18n::text("Проверка SHA-256")
        } else {
            crate::i18n::text("Текущий объект")
        };
        ui.add(egui::Label::new(format!("{phase}: {name}")).truncate())
            .on_hover_text(item.display().to_string());
    }
    if let Some(verification) = &progress.verification {
        ui.label(crate::tr_format!(
            "Прочитано для проверки: {} / {}",
            "Read for verification: {} / {}",
            human_size(verification.read),
            human_size(verification.expected)
        ));
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
    if response.gained_focus() {
        response.scroll_to_me(Some(egui::Align::Center));
    }
    paint_focus(ui, &response, 6);
    response.on_hover_text(label).clicked()
}

#[cfg(test)]
mod tests;
