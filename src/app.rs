use crate::command_palette::{CommandPalette, PaletteOutcome};
use crate::conflict::{Answer, ConflictDialog, Decision};
use crate::drag_drop::{DragFiles, DropRequest, DropTargets};
use crate::file_clipboard::{FileList, SystemClipboard};
use crate::folder_jump::{FolderHistory, FolderJump, JumpOutcome};
use crate::fs_ops::TransferKind;
use crate::icons::{self, Icon};
use crate::list_columns::{Column, ColumnLayout, ListColumns};
use crate::operations::{Progress, ProgressView};
use crate::preview::Preview;
use crate::shortcuts::{self, Command, ShortcutEditor, Shortcuts};
use crate::tab_drag::{clamp_tab_slot, DragTab, TabDrop, TabDropTargets};
use crate::theme::{appearance_menu, paint_focus, Palette, Theme};
use crate::vfs::{self as fs_ops, Endpoint, FileEntry, Location};
use eframe::egui::{self, Color32, FontId, RichText, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
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
    grid: bool,
    hidden: bool,
    density: Density,
    order: FileOrder,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
struct FolderViewOverrides {
    grid: Option<bool>,
    hidden: Option<bool>,
    density: Option<Density>,
    order: Option<FileOrder>,
}

impl FolderViewOverrides {
    fn resolve(&self, defaults: FolderView) -> FolderView {
        FolderView {
            grid: self.grid.unwrap_or(defaults.grid),
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
    Grid(bool),
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
}

// Preserve native names that PathBuf's serializer cannot handle.
#[derive(Clone, Serialize, Deserialize)]
enum SavedPath {
    Utf8(String),
    Unix(Vec<u8>),
    Windows(Vec<u16>),
}

impl From<&Location> for SavedLocation {
    fn from(location: &Location) -> Self {
        match location {
            Location::Local(path) => Self::Local(path.as_path().into()),
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
    fn into_path(self) -> Option<PathBuf> {
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
    anchor: Option<Location>,
    cursor: Option<Location>,
    rendered_rows: HashMap<egui::Id, Location>,
    scroll_to: Option<Location>,
    scroll_offset: f32,
    scroll_restore: Option<f32>,
    file_scroll_scope: Option<egui::Id>,
    file_scroll_geometry: Option<(usize, f32)>,
    reveal_after_load: Option<Location>,
    history: Vec<Location>,
    history_index: usize,
    loading: Option<DirectoryLoad>,
    has_complete_listing: bool,
    folder_visit: FolderVisit,
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
            anchor: None,
            cursor: None,
            rendered_rows: HashMap::new(),
            scroll_to: None,
            scroll_offset: 0.0,
            scroll_restore: None,
            file_scroll_scope: None,
            file_scroll_geometry: None,
            reveal_after_load: None,
            history: vec![path],
            history_index: 0,
            loading: None,
            has_complete_listing: false,
            folder_visit: FolderVisit::Unseen,
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
                self.reveal_after_load = None;
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
        self.has_complete_listing = false;
        self.folder_visit = FolderVisit::CountOnOpen;
        self.selected.clear();
        self.anchor = None;
        self.cursor = None;
        self.scroll_to = None;
        self.scroll_offset = 0.0;
        self.scroll_restore = None;
        self.file_scroll_scope = None;
        self.file_scroll_geometry = None;
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
            grid: settings.grid_for(pane),
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
        ] {
            if target.as_ref().is_some_and(|path| is_dot_file(path.path())) {
                *target = None;
            }
        }
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

    fn entry_id(&self, path: &Location) -> egui::Id {
        egui::Id::new(("file_entry", self.id, path))
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
}
impl Pane {
    const MAX_CLOSED_TABS: usize = 16;

    fn new(tabs: Vec<Tab>) -> Self {
        Self {
            tabs,
            current: 0,
            closed_tabs: VecDeque::new(),
            tab_context_menu: None,
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
enum Action {
    Shortcut(Command),
    MoveSelection {
        delta: isize,
        extend: bool,
    },
    ClearSelection,
    Focus(usize),
    FolderView {
        pane: usize,
        tab: u64,
        location: Location,
        change: FolderViewChange,
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
enum Dialog {
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
    bookmarks: Vec<Bookmark>,
    bookmark_menu_open: Option<u64>,
    bookmark_editor: Option<BookmarkEditor>,
    workspaces: Vec<NamedWorkspace>,
    preview: Preview,
    properties: Option<crate::properties::Properties>,
    clipboard: Option<Clipboard>,
    system_clipboard: SystemClipboard,
    dialog: Option<Dialog>,
    dialog_focus: bool,
    dialog_file_focus: Option<(u64, Location)>,
    job: Option<Job>,
    transfer: Option<TransferInteraction>,
    notice: Option<(String, bool)>,
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
            bookmarks,
            bookmark_menu_open: None,
            bookmark_editor: None,
            workspaces,
            preview: Preview::new(),
            properties: None,
            clipboard: None,
            system_clipboard: SystemClipboard::new(cc),
            dialog: None,
            dialog_focus: false,
            dialog_file_focus: None,
            job: None,
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
            search: Default::default(),
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
            FolderViewChange::Grid(grid) => view.grid = Some(grid),
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
                |location, covered| {
                    if cut_generation.is_some() {
                        let _ =
                            events.send(TransferEvent::Moved(location.clone(), covered.to_vec()));
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
                Ok(TransferEvent::Moved(location, covered)) => {
                    if let Some(generation) = transfer.cut_generation {
                        self.consume_cut_source(&location, &covered, generation);
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
        let now = Instant::now();
        for (pane_index, pane) in self.panes.iter_mut().enumerate() {
            let displayed = self.settings.split || pane_index == 0;
            for (tab_index, tab) in pane.tabs.iter_mut().enumerate() {
                let current = displayed && tab_index == pane.current;
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
        match action {
            Action::Shortcut(command) => self.run_shortcut(command, ctx),
            Action::MoveSelection { delta, extend } => {
                let view = self.folder_view(self.active);
                self.tab_mut().move_selection(delta, view, extend);
                self.tab().focus_cursor(ctx);
            }
            Action::ClearSelection => {
                let tab = self.tab_mut();
                tab.selected.clear();
                tab.cursor = None;
                tab.anchor = None;
                tab.filter.clear();
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
                self.remember_dialog_file_focus(ctx);
                self.dialog = Some(Dialog::NewFolder {
                    parent: self.tab().path.clone(),
                    name: String::new(),
                });
                self.dialog_focus = true;
            }
            Action::Rename if self.job.is_none() => {
                let selected = self.selected();
                if selected.len() == 1 {
                    self.remember_dialog_file_focus(ctx);
                    let source = selected[0].clone();
                    let name = file_name(&source);
                    self.dialog = Some(Dialog::Rename { source, name });
                    self.dialog_focus = true;
                }
            }
            Action::Trash if self.job.is_none() => {
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
        self.tools = crate::tools::Tools::detect();
        self.search.open();
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
            Command::SelectAll => {
                let view = self.folder_view(self.active);
                let tab = self.tab_mut();
                tab.selected = tab
                    .visible(view)
                    .iter()
                    .map(|&index| tab.entries[index].path.clone())
                    .collect();
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
            || self.shortcut_editor.is_some()
            || self.bookmark_editor.is_some()
            || self.bookmark_menu_open.is_some_and(|id| {
                self.bookmarks.iter().any(|bookmark| bookmark.id == id)
            })
            || self.command_palette.is_some()
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
        let idle = self.job.is_none();
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
                    !path.remote()
                        || tab
                            .entries
                            .iter()
                            .any(|entry| entry.path == *path && entry.is_dir)
                        || (self.tools.zed.is_some() && self.editor_job.is_none())
                }),
                Command::OtherPane => self.settings.split,
                Command::NewFolder => idle,
                Command::Rename => idle && has_selection && selected.len() == 1,
                Command::Trash => idle && has_selection,
                Command::Properties => has_selection && selected.iter().all(|path| !path.remote()),
                Command::CopyToPane | Command::MoveToPane => {
                    idle && self.transfer.is_none() && self.settings.split && has_selection
                }
                Command::EditInZed => {
                    has_selection
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
            .map(|(pane, path)| (self.panes[pane].tab().id, path.clone()));
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
                        .on_hover_text(self.settings.shortcuts.hint(ctx, Command::Search, crate::i18n::text("Суперпоиск по индексу locate"))).clicked() {
                        self.open_search();
                    }
                    if ui.add_enabled_ui(self.job.is_none(), |ui| icons::button(ui, Icon::Server, "SFTP", self.tab().path.remote())).inner
                        .on_hover_text(crate::i18n::text("Подключиться по SFTP")).clicked() {
                        actions.push(Action::ConnectSftp);
                    }
                    for (icon, tip, enabled, action) in [
                        (Icon::NewFolder, self.settings.shortcuts.hint(ctx, Command::NewFolder, Command::NewFolder.label()), self.job.is_none(), Action::NewFolder),
                        (Icon::Copy, crate::i18n::text("Копировать · ⌘/Ctrl+C").into(), !self.tab().selected.is_empty(), Action::Copy(false)),
                        (Icon::Paste, crate::i18n::text("Вставить · ⌘/Ctrl+V").into(), self.job.is_none(), Action::Paste(false)),
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
                        self.workspace_places(ui, actions);
                        ui.add_space(6.0);
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
                                RichText::new(crate::i18n::text("Двойной щелчок — открыть"))
                                    .size(11.0)
                                    .color(colors.muted),
                            );
                        });
                    });
                }
            });
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
                Dialog::WorkspaceName { .. } => !self.dialog_focus,
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
                Dialog::WorkspaceName { editing, name } => {
                    self.save_workspace_name(editing, name);
                }
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
                        self.pane_key_presses.push((event_index, event.clone()));
                        return false;
                    }
                }
                egui::Event::Key {
                    key: key @ (egui::Key::ArrowUp | egui::Key::ArrowDown),
                    modifiers,
                    pressed: true,
                    ..
                } if pane_keys => {
                    let chord = shortcuts::chord(*key, *modifiers);
                    if matches!(
                        chord.modifiers,
                        egui::Modifiers::NONE | egui::Modifiers::SHIFT
                    ) || self.bound_pane_command(chord).is_some()
                    {
                        // Arrow navigation moves file selection rather than geometric widget focus.
                        self.pane_key_presses.push((event_index, event.clone()));
                        return false;
                    }
                }
                egui::Event::WindowFocused(false) => self.tab_held = false,
                _ => {}
            }
            true
        });
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
                    );
                }
                ui.advance_cursor_after_rect(workspace);
            });
        if self.dialog.is_none()
            && !self.conflict_open()
            && self.sftp_form.is_none()
            && !self.show_help
            && self.shortcut_editor.is_none()
            && self.command_palette.is_none()
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
        self.restore_dialog_file_focus(ctx);
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
    }
}

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
            ui.label(crate::i18n::text("Чтение папки…"));
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
            if !compact {
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
            let tab = pane.tab_mut();
            let view = tab.folder_view(settings, pane_index, folder_views);
            let FolderView {
                grid,
                order,
                density,
                ..
            } = view;
            let compact_rows = compact || density == Density::Compact;
            tab.rendered_rows.clear();
            address_bar(ui, tab, pane_index, actions, drop_targets);
            let response = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let mut controls = 2.0 * (icons::COMPACT_SIZE + ui.spacing().item_spacing.x);
                    if compact {
                        let font = egui::TextStyle::Button.resolve(ui.style());
                        let label_width = ui
                            .painter()
                            .layout_no_wrap(sort_label.into(), font, Color32::WHITE)
                            .size()
                            .x;
                        controls += label_width
                            + 2.0 * ui.spacing().button_padding.x
                            + ui.spacing().item_spacing.x;
                    }
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
                        actions
                            .push(tab.folder_view_action(pane_index, FolderViewChange::Grid(true)));
                    }
                    if icons::compact_button(ui, Icon::List, crate::i18n::text("Список"), !grid)
                        .clicked()
                    {
                        actions.push(
                            tab.folder_view_action(pane_index, FolderViewChange::Grid(false)),
                        );
                    }
                    if compact {
                        let menu =
                            ui.menu_button(sort_label, |ui| {
                                sort_controls(ui, order, |sort| {
                                    actions.push(tab.folder_view_action(
                                        pane_index,
                                        FolderViewChange::Sort(sort),
                                    ));
                                });
                            });
                        paint_focus(ui, &menu.response, 4);
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
                let files_id = ui.id().with(("file_list", tab.id));
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
                                                        pane: pane_index,
                                                        split: settings.split,
                                                        grid: true,
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
                                            pane: pane_index,
                                            split: settings.split,
                                            grid: false,
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
            let visible_count = if scrolling_columns {
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
    pane: usize,
    split: bool,
    grid: bool,
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
        pane,
        split,
        grid,
        compact,
        idle,
        zed_available,
    } = options;
    let entry = &tab.entries[index];
    let is_dir = entry.is_dir;
    let path = entry.path.clone();
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
    let response = ui.interact(rect, tab.entry_id(&path), Sense::click_and_drag());
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
    if idle && response.drag_started_by(egui::PointerButton::Primary) {
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

    #[test]
    fn unrevisioned_file_clipboard_uses_observed_contents() {
        let file_list = FileList {
            paths: vec![PathBuf::from("first.txt")],
            cut: true,
            revision: None,
        };
        let mut different = file_list.clone();
        different.paths.push(PathBuf::from("second.txt"));
        let clipboard =
            Clipboard::new(Vec::new(), true, ClipboardOrigin::System(file_list.clone()));
        assert!(clipboard.matches(Some(&file_list)));
        assert!(!clipboard.matches(Some(&different)));
        assert!(!clipboard.matches(None));
    }

    fn wait_for_directory(tab: &mut Tab) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while tab.loading.is_some() && Instant::now() < deadline {
            tab.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(tab.loading.is_none(), "directory read did not finish");
    }

    #[test]
    fn progressive_directory_shows_first_entry_before_stall_and_keeps_partial_results() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("first"), b"first").unwrap();
        std::fs::write(root.path().join("second"), b"second").unwrap();
        let ctx = egui::Context::default();
        let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
        let mut entries = fs_ops::read_directory(&tab.path, &|| false)
            .unwrap()
            .into_iter();
        let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Progressive);
        tab.loading = Some(load);
        let (first_sent, first_received) = mpsc::channel();
        let (resume, remainder) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            send_progressive_listing(
                |visit| {
                    visit(entries.next().unwrap());
                    first_sent.send(()).unwrap();
                    remainder.recv().unwrap();
                    visit(entries.next().unwrap());
                    Err("fixture read failed".into())
                },
                |update| tx.send(update).is_ok(),
            );
        });
        first_received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(tab.poll());
        assert!(tab.blocking_load());
        assert_eq!(tab.entries.len(), 1);
        assert_eq!(tab.entries[0].name, "first");
        assert_eq!(tab.entries[0].size, 5);
        let first = tab.entries[0].path.clone();
        tab.selected.insert(first.clone());
        resume.send(()).unwrap();
        wait_for_directory(&mut tab);
        worker.join().unwrap();
        assert_eq!(tab.entries.len(), 2);
        assert_eq!(tab.selected, BTreeSet::from([first]));
        let error = tab.error.as_deref().unwrap();
        assert!(error.contains("incomplete folder listing"));
        assert!(error.contains("fixture read failed"));

        tab.auto_refresh(&ctx, tab.next_auto_refresh);
        wait_for_directory(&mut tab);
        assert!(tab.has_complete_listing);
        assert!(tab.error.is_none());
    }

    #[test]
    fn navigating_cancels_progressive_directory_and_discards_queued_entries() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        std::fs::write(old.path().join("old-first"), b"old").unwrap();
        std::fs::write(old.path().join("old-second"), b"old").unwrap();
        std::fs::write(new.path().join("current"), b"new").unwrap();
        let ctx = egui::Context::default();
        let mut tab = Tab::new(old.path().to_path_buf().into(), &ctx);
        let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Progressive);
        let cancelled = load.cancelled.clone();
        tab.loading = Some(load);
        let location = tab.path.clone();
        let (first_sent, first_received) = mpsc::channel();
        let (resume, remainder) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut visited = 0;
            let cancelled = || cancelled.load(Ordering::Relaxed);
            send_progressive_listing(
                |visit| {
                    fs_ops::visit_directory(&location, &cancelled, |entry| {
                        visited += 1;
                        visit(entry);
                        if visited == 1 {
                            first_sent.send(()).unwrap();
                            remainder.recv().unwrap();
                        }
                    })
                },
                |update| !cancelled() && tx.send(update).is_ok(),
            );
            visited
        });
        first_received.recv_timeout(Duration::from_secs(5)).unwrap();
        tab.navigate(new.path().to_path_buf().into(), &ctx);
        resume.send(()).unwrap();
        wait_for_directory(&mut tab);
        assert_eq!(worker.join().unwrap(), 1);
        assert_eq!(tab.entries.len(), 1);
        assert_eq!(tab.entries[0].name, "current");
        assert!(tab.error.is_none());
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
        assert!(tab.automatic_loading());
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
    fn automatic_refresh_is_single_flight_and_ignores_stale_reads() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("current"), "current").unwrap();
        let ctx = egui::Context::default();
        let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
        wait_for_directory(&mut tab);
        let (stale_tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Automatic);
        let cancelled = load.cancelled.clone();
        tab.loading = Some(load);
        tab.auto_refresh(&ctx, tab.next_auto_refresh);
        assert!(tab.automatic_loading());
        // Sending to the original receiver proves no second scan replaced it.
        stale_tx
            .send(DirectoryRead::Finished(Ok(Some(vec![]))))
            .unwrap();
        tab.navigate(other.path().to_path_buf().into(), &ctx);
        assert!(cancelled.load(Ordering::Relaxed));
        wait_for_directory(&mut tab);
        assert_eq!(tab.entries.len(), 1);
        assert_eq!(tab.entries[0].name, "current");
    }

    #[test]
    fn automatic_refresh_schedules_remote_tab_without_reconnecting_or_clearing_it() {
        let root = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.settings.split = false;
        let tab = app.tab_mut();
        wait_for_directory(tab);
        tab.path = Location::Sftp {
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 0,
                user: "test".into(),
                key: None,
            },
            path: "/".into(),
        };
        let keep = tab.path.join("keep.txt");
        tab.entries.replace(vec![FileEntry {
            path: keep.clone(),
            name: "keep.txt".into(),
            is_dir: false,
            is_symlink: false,
            size: 42,
            modified: None,
            metadata: Default::default(),
        }]);
        tab.selected.insert(keep.clone());
        tab.next_auto_refresh = Instant::now();
        app.poll(&ctx);
        let tab = app.tab_mut();
        assert!(tab.automatic_loading() && !tab.blocking_load());
        let started = Instant::now();
        wait_for_directory(tab);
        assert_eq!(tab.entries.len(), 1);
        assert_eq!(tab.selected, BTreeSet::from([keep]));
        assert!(tab.error.is_none(), "poll should skip an uncached session");
        assert!(tab.next_auto_refresh >= started + SFTP_REFRESH_INTERVAL);
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
    fn pane_shortcuts_leave_text_and_modal_input_alone() {
        let root = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let focus = egui::Id::new("test_filter");
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let run = |app: &mut PawCommander, events, focused| {
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let response =
                            ui.add(egui::TextEdit::singleline(&mut app.tab_mut().filter).id(focus));
                        if focused {
                            response.request_focus();
                        } else {
                            response.surrender_focus();
                        }
                    });
                    app.keyboard(ctx);
                },
            );
        };
        run(
            &mut app,
            vec![key(egui::Key::F3, egui::Modifiers::NONE)],
            true,
        );
        assert!(
            app.settings.split,
            "a focused text field blocks pane commands"
        );
        run(
            &mut app,
            vec![key(egui::Key::L, egui::Modifiers::COMMAND)],
            true,
        );
        assert!(
            app.tab().edit_address,
            "the original text navigation chord still works"
        );

        app.shortcut_editor = Some(ShortcutEditor::new(&app.settings.shortcuts));
        run(
            &mut app,
            vec![key(egui::Key::F3, egui::Modifiers::NONE)],
            false,
        );
        assert!(
            app.settings.split,
            "the shortcut editor blocks pane commands"
        );
        app.shortcut_editor = None;
        run(
            &mut app,
            vec![key(egui::Key::F3, egui::Modifiers::SHIFT)],
            false,
        );
        assert!(app.settings.split, "extra modifiers do not match a binding");
        run(
            &mut app,
            vec![
                egui::Event::Key {
                    key: egui::Key::F3,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
                key(egui::Key::F3, egui::Modifiers::NONE),
            ],
            false,
        );
        assert!(!app.settings.split);
    }

    #[test]
    fn custom_toolbar_shortcut_does_not_consume_text_editing_keys() {
        let root = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let enter = egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Enter);
        app.settings.shortcuts = serde_json::from_value(serde_json::json!({
            "overrides": {"Open": null, "FocusToolbar": enter}
        }))
        .unwrap();
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.text_edit_singleline(&mut app.tab_mut().address)
                        .request_focus();
                });
                app.keyboard(ctx);
                assert!(ctx.input(|input| input.key_pressed(egui::Key::Enter)));
            },
        );
        assert!(!app.focus_toolbar);
    }

    #[test]
    fn command_palette_filters_navigates_runs_and_restores_focus() {
        use egui::{Event, Key, Modifiers};

        let root = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.action(Action::NewTab(0), &ctx);
        app.settings.shortcuts = serde_json::from_value(serde_json::json!({
            "overrides": {"NewTab": shortcuts::chord(Key::F9, Modifiers::NONE)}
        }))
        .unwrap();
        app.tab_mut().focus_filter = true;

        let mut run = |app: &mut PawCommander, events| {
            let mut input = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1440.0, 860.0),
                )),
                ..Default::default()
            };
            eframe::App::raw_input_hook(app, &ctx, &mut input);
            ctx.run(input, |ctx| eframe::App::update(app, ctx, &mut frame))
        };
        let key = |key, pressed, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        };
        let open = || {
            let modifiers = Modifiers::COMMAND | Modifiers::SHIFT;
            vec![key(Key::P, true, modifiers), key(Key::P, false, modifiers)]
        };

        run(&mut app, vec![]);
        let previous = ctx.memory(|memory| memory.focused()).unwrap();
        run(&mut app, open());
        assert!(app.command_palette.is_some());
        run(&mut app, vec![]);
        run(&mut app, vec![Event::Text("not-a-command".into())]);
        run(&mut app, vec![key(Key::Escape, true, Modifiers::NONE)]);
        assert!(app.command_palette.is_none());
        assert!(ctx.memory(|memory| memory.has_focus(previous)));
        assert!(app.tab().filter.is_empty());
        run(
            &mut app,
            vec![
                key(Key::Escape, true, Modifiers::NONE),
                key(Key::Escape, false, Modifiers::NONE),
            ],
        );
        assert!(ctx.memory(|memory| memory.has_focus(previous)));

        run(&mut app, open());
        run(&mut app, vec![]);
        let output = run(&mut app, vec![Event::Text("TAB".into())]);
        let tree = output.platform_output.accesskit_update.unwrap();
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label() == Some(Command::NewTab.label()) && node.keyboard_shortcut() == Some("F9")
        }));
        assert!(!tree.nodes.iter().any(|(_, node)| {
            node.label() == Some(Command::NewFolder.label()) && node.keyboard_shortcut().is_some()
        }));
        run(&mut app, vec![key(Key::ArrowDown, true, Modifiers::NONE)]);
        run(&mut app, vec![key(Key::Enter, true, Modifiers::NONE)]);
        assert!(app.command_palette.is_none());
        assert_eq!(app.panes[0].tabs.len(), 1);
    }

    #[test]
    fn folder_jump_uses_successful_visits_and_opens_in_the_active_pane() {
        use egui::{Event, Key, Modifiers};

        let root = tempfile::tempdir().unwrap();
        let base: Location = root.path().to_path_buf().into();
        let target = base.join("PawCommander");
        let missing = base.join("Missing");
        std::fs::create_dir(target.path()).unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        wait_for_directory(app.tab_mut());
        app.poll(&ctx);
        app.action(Action::Navigate(0, target.clone()), &ctx);
        wait_for_directory(app.tab_mut());
        app.poll(&ctx);
        app.action(Action::Navigate(0, missing.clone()), &ctx);
        wait_for_directory(app.tab_mut());
        app.poll(&ctx);
        assert!(app.settings.folder_history.ranked("missing").is_empty());

        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        let restored: Settings = eframe::get_value(&storage, "pawcommander.settings").unwrap();
        assert_eq!(restored.folder_history.ranked("pwcm"), vec![&target]);

        app.active = 1;
        let mut run = |app: &mut PawCommander, events| {
            let mut input = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1440.0, 860.0),
                )),
                ..Default::default()
            };
            eframe::App::raw_input_hook(app, &ctx, &mut input);
            ctx.run(input, |ctx| eframe::App::update(app, ctx, &mut frame))
        };
        let press = |key, modifiers| {
            [true, false].map(|pressed| Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers,
            })
        };
        let command_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        run(&mut app, press(Key::J, command_shift).to_vec());
        assert!(app.folder_jump.is_some());
        run(&mut app, press(Key::Escape, Modifiers::NONE).to_vec());
        assert!(app.folder_jump.is_none());
        assert_eq!(app.tab().path, base);

        run(&mut app, press(Key::P, command_shift).to_vec());
        run(&mut app, vec![]);
        run(
            &mut app,
            vec![Event::Text(Command::FolderJump.label().into())],
        );
        run(&mut app, press(Key::Enter, Modifiers::NONE).to_vec());
        assert!(app.command_palette.is_none());
        assert!(app.folder_jump.is_some());
        assert_eq!(app.tab().path, base);
        run(&mut app, vec![]);
        run(&mut app, vec![]);
        run(&mut app, vec![Event::Text("pwcm".into())]);
        run(&mut app, press(Key::Enter, Modifiers::NONE).to_vec());
        assert!(app.folder_jump.is_none());
        assert_eq!(app.active, 1);
        assert_eq!(app.tab().path, target);
        assert_eq!(app.panes[0].tab().path, missing);
    }

    #[test]
    fn folder_jump_seeds_visible_restored_tabs_once() {
        let root = tempfile::tempdir().unwrap();
        let base: Location = root.path().to_path_buf().into();
        let left = base.join("Left current");
        let right = base.join("Right current");
        let background = base.join("Background tab");
        for location in [&left, &right, &background] {
            std::fs::create_dir(location.path()).unwrap();
        }
        let ctx = egui::Context::default();
        let mut storage = MemoryStorage::default();
        let legacy = Settings {
            locations: vec![vec![left.clone(), background.clone()], vec![right.clone()]],
            ..Settings::default()
        };
        eframe::set_value(&mut storage, "pawcommander.settings", &legacy);
        let open = |storage: &MemoryStorage| {
            let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
            cc.storage = Some(storage);
            let mut app = PawCommander::new(&cc, None);
            for pane in &mut app.panes {
                for tab in &mut pane.tabs {
                    wait_for_directory(tab);
                }
            }
            app.poll(&ctx);
            app
        };
        let mut app = open(&storage);
        assert_eq!(
            app.settings.folder_history.ranked("current"),
            vec![&right, &left]
        );
        assert!(app.settings.folder_history.ranked("background").is_empty());
        app.panes[0].select_tab(1);
        app.poll(&ctx);
        assert_eq!(
            app.settings.folder_history.ranked("background"),
            vec![&background]
        );

        let before = serde_json::to_string(&app.settings.folder_history).unwrap();
        let tab = app.panes[0].tab_mut();
        tab.auto_refresh(&ctx, tab.next_auto_refresh);
        assert!(tab.automatic_loading());
        wait_for_directory(tab);
        app.poll(&ctx);
        assert_eq!(
            serde_json::to_string(&app.settings.folder_history).unwrap(),
            before
        );
        eframe::App::save(&mut app, &mut storage);
        let reopened = open(&storage);
        assert_eq!(
            serde_json::to_string(&reopened.settings.folder_history).unwrap(),
            before
        );
    }

    #[test]
    fn accessibility_tree_keeps_file_focus_and_selection_distinct_in_list_and_grid() {
        use egui::accesskit::{Action as AxAction, ActionRequest, Role, Toggled};

        let root = tempfile::tempdir().unwrap();
        let folder: Location = root.path().join("Folder").into();
        let file: Location = root.path().join("Note.txt").into();
        std::fs::create_dir(folder.path()).unwrap();
        std::fs::write(file.path(), "example").unwrap();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        wait_for_directory(app.tab_mut());
        wait_for_directory(app.panes[1].tab_mut());
        app.tab_mut().selected.insert(file.clone());
        app.panes[1].tab_mut().selected.insert(file.clone());
        let ax_id = |id: egui::Id| egui::accesskit::NodeId(id.value());
        let folder_id = ax_id(app.tab().entry_id(&folder));
        let file_id = ax_id(app.tab().entry_id(&file));
        assert_ne!(file_id, ax_id(app.panes[1].tab().entry_id(&file)));

        let mut run = |app: &mut PawCommander, events| {
            let mut input = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1440.0, 860.0),
                )),
                ..Default::default()
            };
            eframe::App::raw_input_hook(app, &ctx, &mut input);
            ctx.run(input, |ctx| eframe::App::update(app, ctx, &mut frame))
                .platform_output
                .accesskit_update
                .unwrap()
        };
        let request = |action| {
            vec![egui::Event::AccessKitActionRequest(ActionRequest {
                action,
                target: folder_id,
                data: None,
            })]
        };
        let key = |key| {
            [true, false]
                .into_iter()
                .map(move |pressed| egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                })
                .collect::<Vec<_>>()
        };
        let list = run(&mut app, vec![]);
        let row = list.nodes.iter().find(|(id, _)| *id == file_id).unwrap();
        assert_eq!(row.1.role(), Role::CheckBox);
        assert!(row.1.label().unwrap().starts_with("Note.txt, "));
        assert_eq!(row.1.toggled(), Some(Toggled::True));

        let focused = run(&mut app, request(AxAction::Focus));
        assert_eq!(focused.focus, folder_id);
        assert_eq!(app.selected(), vec![file.clone()]);
        let mut switch_then_delete = key(egui::Key::Tab);
        switch_then_delete.extend(key(egui::Key::Delete));
        let switched = run(&mut app, switch_then_delete);
        assert_eq!(app.active, 1);
        assert_eq!(switched.focus, ax_id(app.tab().entry_id(&folder)));
        assert_eq!(app.selected(), vec![file.clone()]);
        assert!(
            app.dialog.is_none(),
            "Delete must respect the destination row after Tab"
        );
        run(&mut app, request(AxAction::Focus));
        run(&mut app, key(egui::Key::Delete));
        assert!(
            app.dialog.is_none(),
            "Delete must not act on the other selected file"
        );

        app.settings.pane_grid = Some([true, false]);
        let grid = run(&mut app, request(AxAction::Click));
        assert_eq!(grid.focus, folder_id);
        assert_eq!(app.selected(), vec![folder.clone(), file]);
        let row = grid.nodes.iter().find(|(id, _)| *id == folder_id).unwrap();
        assert_eq!(row.1.role(), Role::CheckBox);
        assert_eq!(row.1.toggled(), Some(Toggled::True));
        app.tab_mut().selected = BTreeSet::from([folder.clone()]);
        let spatial_focus = run(&mut app, key(egui::Key::ArrowRight));
        assert_eq!(spatial_focus.focus, file_id);
        run(&mut app, key(egui::Key::Delete));
        assert!(
            app.dialog.is_none(),
            "spatial focus cannot delete the old selection"
        );
        run(&mut app, request(AxAction::Focus));
        run(&mut app, key(egui::Key::F2));
        assert!(matches!(app.dialog, Some(Dialog::Rename { .. })));
        run(&mut app, vec![]);
        run(&mut app, key(egui::Key::Escape));
        let returned = run(&mut app, vec![]);
        assert_eq!(returned.focus, folder_id);
        assert_eq!(app.selected(), vec![folder.clone()]);
        run(&mut app, key(egui::Key::Enter));
        assert_eq!(app.tab().path, folder, "Enter opens the focused folder");
    }

    #[test]
    fn accessibility_settings_migrate_and_round_trip() {
        let mut settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.ui_scale, 100);
        assert!(!settings.high_contrast);
        let ctx = egui::Context::default();
        settings.set_ui_scale(&ctx, 999);
        assert_eq!(settings.ui_scale, 100);
        settings.set_ui_scale(&ctx, 150);
        settings.high_contrast = true;
        let mut storage = MemoryStorage::default();
        eframe::set_value(&mut storage, "settings", &settings);
        let restored: Settings = eframe::get_value(&storage, "settings").unwrap();
        assert_eq!(restored.ui_scale, 150);
        assert!(restored.high_contrast);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        assert_eq!(ctx.zoom_factor(), 1.5);
    }

    #[test]
    fn pane_tab_keeps_event_order_without_traversing_widget_focus() {
        let root = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let key = |key, modifiers, pressed| egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        };
        let tab = |pressed| key(egui::Key::Tab, egui::Modifiers::NONE, pressed);
        let run = |app: &mut PawCommander, events| {
            let mut input = egui::RawInput {
                events,
                ..Default::default()
            };
            eframe::App::raw_input_hook(app, &ctx, &mut input);
            let _ = ctx.run(input, |ctx| {
                app.keyboard(ctx);
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = ui.button("Widget focus target");
                });
            });
        };
        run(
            &mut app,
            vec![tab(true), key(egui::Key::T, egui::Modifiers::COMMAND, true)],
        );
        assert_eq!(app.active, 1);
        assert_eq!(app.panes[1].tabs.len(), 2, "Tab dispatched before New tab");
        assert!(!ctx.wants_keyboard_input());
        run(&mut app, vec![tab(true)]);
        assert_eq!(app.active, 1, "held Tab does not switch panes again");
        run(&mut app, vec![tab(false), tab(true)]);
        assert_eq!(app.active, 0);

        app.settings.shortcuts = serde_json::from_value(serde_json::json!({
            "overrides": { "OtherPane": null }
        }))
        .unwrap();
        run(&mut app, vec![tab(false), tab(true)]);
        assert_eq!(app.active, 0);
        assert!(ctx.wants_keyboard_input(), "unbound Tab traverses widgets");
    }

    #[test]
    fn starting_filter_input_blocks_a_later_file_shortcut_in_the_same_frame() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("keep.txt");
        std::fs::write(&path, b"data").unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        wait_for_directory(app.tab_mut());
        app.tab_mut().selected.insert(path.into());
        assert_eq!(app.selected().len(), 1);

        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let _ = ctx.run(
            egui::RawInput {
                events: vec![
                    key(egui::Key::F, egui::Modifiers::COMMAND),
                    key(egui::Key::Delete, egui::Modifiers::NONE),
                ],
                ..Default::default()
            },
            |ctx| app.keyboard(ctx),
        );
        assert!(app.tab().focus_filter);
        assert!(app.dialog.is_none());
    }

    #[test]
    fn settings_round_trip_shortcut_profile_and_overrides() {
        let mut storage = MemoryStorage::default();
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "shortcuts": {
                "profile": "Commander",
                "overrides": {
                    "Rename": {
                        "modifiers": {
                            "alt": false,
                            "ctrl": false,
                            "shift": false,
                            "mac_cmd": false,
                            "command": false
                        },
                        "logical_key": "F9"
                    },
                    "Refresh": null
                }
            }
        }))
        .unwrap();
        eframe::set_value(&mut storage, "settings", &settings);
        let mut restored: Settings = eframe::get_value(&storage, "settings").unwrap();
        restored.shortcuts.validate();
        assert_eq!(restored.shortcuts, settings.shortcuts);
        for (key, command) in [
            (egui::Key::F2, None),
            (egui::Key::F5, Some(Command::CopyToPane)),
            (egui::Key::F9, Some(Command::Rename)),
        ] {
            assert_eq!(
                restored
                    .shortcuts
                    .command(shortcuts::chord(key, egui::Modifiers::NONE)),
                command
            );
        }
        assert_eq!(
            restored
                .shortcuts
                .command(shortcuts::chord(egui::Key::R, egui::Modifiers::COMMAND)),
            None
        );
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
        assert!(app.panes[0].tabs[0].automatic_loading());
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
            app.panes[1].tab().automatic_loading(),
            "displayed pane resumes scanning"
        );
        wait_for_directory(app.panes[1].tab_mut());
    }

    #[test]
    fn automatic_refresh_and_session_restore_preserve_scroll_in_list_and_grid() {
        fn draw(
            ctx: &egui::Context,
            pane: &mut Pane,
            settings: &Settings,
            index: usize,
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
                                index,
                                active: true,
                                settings,
                                folder_views: &FolderViews::new(),
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
            let mut pane = Pane::new(vec![tab]);
            let settings = Settings {
                grid,
                ..Default::default()
            };
            draw(&ctx, &mut pane, &settings, 0);
            pane.tab_mut().scroll_to = Some(selected.clone());
            draw(&ctx, &mut pane, &settings, 0);
            let before = draw(&ctx, &mut pane, &settings, 0);
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
                draw(&ctx, &mut pane, &settings, 0),
                before,
                "no layout shift while reading"
            );
            wait_for_directory(pane.tab_mut());
            assert_eq!(
                draw(&ctx, &mut pane, &settings, 0),
                before,
                "refresh retains pixel offset and rows"
            );
            assert!(pane.tab().selected.contains(&selected));
            assert!(pane.tab().scroll_to.is_none());

            let saved = pane.session();
            let offset = saved.tabs[0].scroll_offset;
            assert!(offset > 0.0);
            let reopened_ctx = egui::Context::default();
            let mut reopened = Pane::from_session(saved, root.path(), &reopened_ctx);
            let (_tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Progressive);
            reopened.tab_mut().loading = Some(load);
            reopened
                .tab_mut()
                .entries
                .extend(vec![pane.tab().entries[0].clone()]);
            draw(&reopened_ctx, &mut reopened, &settings, 0);
            assert_eq!(reopened.tab().scroll_restore, Some(offset));
            assert_eq!(reopened.session().tabs[0].scroll_offset, offset);

            reopened
                .tab_mut()
                .finish_load(Ok(Some(pane.tab().entries.to_vec())));
            draw(&reopened_ctx, &mut reopened, &settings, 0);
            assert_eq!(draw(&reopened_ctx, &mut reopened, &settings, 0), before);
            assert_eq!(reopened.tab().scroll_restore, None);
            assert_eq!(reopened.tab().scroll_offset, offset);
            let previous_scope = reopened.tab().file_scroll_scope;
            draw(&reopened_ctx, &mut reopened, &settings, 1);
            assert_ne!(reopened.tab().file_scroll_scope, previous_scope);
            assert_eq!(reopened.tab().scroll_offset, offset);
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
            Pane::new(vec![make_tab("alpha"), make_tab("beta")]),
            Pane::new(vec![make_tab("gamma")]),
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
            Pane::new(vec![moved, other]),
            Pane::new(vec![Tab::new(path.clone(), &ctx)]),
        ];
        panes[0].select_tab(1);
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
    fn tab_moves_respect_pin_group_boundaries_in_both_panes() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let make_tab = |name| Tab::new(root.path().join(name).into(), &ctx);
        let mut panes = [
            Pane::new(vec![make_tab("a"), make_tab("b"), make_tab("c")]),
            Pane::new(vec![make_tab("d"), make_tab("e")]),
        ];
        for (pane, index) in [(0, 0), (0, 1), (1, 0)] {
            panes[pane].toggle_pin(index);
        }
        let a = panes[0].tabs[0].id;
        let c = panes[0].tabs[2].id;
        assert!(!move_tab(
            &mut panes,
            TabDrop {
                source: 0,
                id: c,
                destination: 0,
                slot: 0,
            },
            &ctx,
        ));
        assert!(move_tab(
            &mut panes,
            TabDrop {
                source: 0,
                id: a,
                destination: 0,
                slot: 3,
            },
            &ctx,
        ));
        assert_eq!(panes[0].tabs[1].id, a);
        assert_eq!(panes[0].pinned_count(), 2);

        for (id, slot) in [(a, 2), (c, 0)] {
            assert!(move_tab(
                &mut panes,
                TabDrop {
                    source: 0,
                    id,
                    destination: 1,
                    slot,
                },
                &ctx,
            ));
        }
        assert_eq!(panes[0].pinned_count(), 1);
        assert_eq!(panes[1].pinned_count(), 2);
        assert_eq!(panes[1].tabs[1].id, a);
        assert_eq!(panes[1].tabs[2].id, c);
        assert!(panes.iter().all(|pane| pane.closed_tabs.is_empty()));
    }

    #[test]
    fn tab_context_menu_blocks_pane_shortcuts_until_escape_dismisses_it() {
        use egui::{Event, Key, Modifiers, PointerButton};

        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("tab-menu");
        std::fs::create_dir(&folder).unwrap();
        let file = folder.join("selected.txt");
        std::fs::write(&file, "example").unwrap();
        let file: Location = file.into();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = PawCommander::new(&cc, Some(folder));
        app.action(Action::NewTab(0), &ctx);
        wait_for_directory(app.tab_mut());
        app.tab_mut().selected.insert(file.clone());

        let mut run = |app: &mut PawCommander, events| {
            let mut input = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1440.0, 860.0),
                )),
                ..Default::default()
            };
            eframe::App::raw_input_hook(app, &ctx, &mut input);
            ctx.run(input, |ctx| eframe::App::update(app, ctx, &mut frame))
        };
        let key = |key, modifiers| {
            [true, false].map(|pressed| Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers,
            })
        };
        let output = run(&mut app, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text() == "tab-menu" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap();
        for pressed in [true, false] {
            run(
                &mut app,
                vec![
                    Event::PointerMoved(pos),
                    Event::PointerButton {
                        pos,
                        button: PointerButton::Secondary,
                        pressed,
                        modifiers: Modifiers::NONE,
                    },
                ],
            );
        }
        assert!(ctx.is_context_menu_open());
        assert!(app.keyboard_blocked(&ctx));

        run(&mut app, key(Key::W, Modifiers::COMMAND).to_vec());
        assert_eq!(app.panes[0].tabs.len(), 2);
        assert!(ctx.is_context_menu_open());
        run(&mut app, key(Key::Escape, Modifiers::NONE).to_vec());
        assert!(!ctx.is_context_menu_open());
        assert!(app.tab().selected.contains(&file));
        run(&mut app, key(Key::W, Modifiers::COMMAND).to_vec());
        assert_eq!(app.panes[0].tabs.len(), 1);
    }

    #[test]
    fn favorites_menu_blocks_alt_digit_and_releases_after_escape() {
        use egui::{Event, Key, Modifiers, PointerButton};

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let mut bookmark = Bookmark::new(target.clone().into());
        bookmark.shortcut = Some(1);
        app.bookmarks.push(bookmark);

        let mut run = |app: &mut PawCommander, events| {
            let mut input = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1440.0, 860.0),
                )),
                ..Default::default()
            };
            eframe::App::raw_input_hook(app, &ctx, &mut input);
            ctx.run(input, |ctx| eframe::App::update(app, ctx, &mut frame))
        };
        let output = run(&mut app, vec![]);
        let menu_position = output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text() == "⋯" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .expect("favorite menu button");
        for pressed in [true, false] {
            run(
                &mut app,
                vec![
                    Event::PointerMoved(menu_position),
                    Event::PointerButton {
                        pos: menu_position,
                        button: PointerButton::Primary,
                        pressed,
                        modifiers: Modifiers::NONE,
                    },
                ],
            );
        }
        run(&mut app, vec![]);
        let current = app.tab().path.clone();
        assert!(app.bookmark_menu_open.is_some());
        assert!(app.keyboard_blocked(&ctx));
        for pressed in [true, false] {
            run(
                &mut app,
                vec![Event::Key {
                    key: Key::Num1,
                    physical_key: Some(Key::Num1),
                    pressed,
                    repeat: false,
                    modifiers: Modifiers::ALT,
                }],
            );
        }
        assert_eq!(app.tab().path, current, "Alt+1 stays inside the open menu");
        for pressed in [true, false] {
            run(
                &mut app,
                vec![Event::Key {
                    key: Key::Escape,
                    physical_key: Some(Key::Escape),
                    pressed,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
            );
        }
        assert!(app.bookmark_menu_open.is_none());
        for pressed in [true, false] {
            run(
                &mut app,
                vec![Event::Key {
                    key: Key::Num1,
                    physical_key: Some(Key::Num1),
                    pressed,
                    repeat: false,
                    modifiers: Modifiers::ALT,
                }],
            );
        }
        assert_eq!(app.tab().path, target.into());
    }

    #[test]
    fn removed_favorite_does_not_leave_keyboard_blocked_by_menu_state() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let bookmark = Bookmark::new(root.path().join("favorite").into());
        app.bookmark_menu_open = Some(bookmark.id);
        app.bookmarks.push(bookmark);
        assert!(app.keyboard_blocked(&ctx));
        app.bookmarks.clear();
        assert!(!app.keyboard_blocked(&ctx));
    }

    #[test]
    fn tab_actions_duplicate_and_reopen_exact_locations_and_persist_pins() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let local: Location = root.path().to_path_buf().into();
        let remote = Location::Sftp {
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 2222,
                user: "test".into(),
                key: Some(root.path().join("key with spaces")),
            },
            path: "/папка/two words".into(),
        };
        let open = |storage: &MemoryStorage, start| {
            let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
            cc.storage = Some(storage);
            PawCommander::new(&cc, start)
        };
        let mut storage = MemoryStorage::default();
        let mut app = open(&storage, Some(root.path().into()));
        app.panes[0].tabs.push(Tab::new(remote.clone(), &ctx));
        let local_id = app.tab().id;
        let remote_id = app.panes[0].tabs[1].id;

        app.action(Action::TogglePinTab(0, 1), &ctx);
        assert_eq!(app.tab().id, local_id);
        assert_eq!(app.panes[0].tabs[0].id, remote_id);
        assert!(app.panes[0].tabs[0].pinned);
        assert!(!app.panes[0].can_close_tab(0));
        app.action(Action::CloseTab(0, 0), &ctx);
        app.panes[0].select_tab(0);
        app.run_shortcut(Command::CloseTab, &ctx);
        assert_eq!(app.panes[0].tabs.len(), 2);
        assert!(app.panes[0].closed_tabs.is_empty());

        app.action(Action::DuplicateTab(0, 0), &ctx);
        let duplicate_id = app.tab().id;
        assert_ne!(duplicate_id, remote_id);
        assert_eq!(app.tab().path, remote);
        assert!(!app.tab().pinned);
        app.action(Action::CloseTab(0, 1), &ctx);
        app.action(Action::CloseTab(0, 1), &ctx);
        app.action(Action::CloseTab(0, 0), &ctx);
        app.action(Action::ReopenTab(1), &ctx);
        assert_eq!(app.active, 0);
        assert_eq!(app.panes[1].tabs.len(), 1);

        app.run_shortcut(Command::ReopenTab, &ctx);
        assert_eq!(app.tab().path, local);
        app.run_shortcut(Command::ReopenTab, &ctx);
        assert_eq!(app.panes[0].current, 1);
        assert_eq!(app.tab().path, remote);
        assert_ne!(app.tab().id, duplicate_id);
        assert!(app.panes[0].closed_tabs.is_empty());
        app.action(Action::CloseTab(0, 1), &ctx);

        eframe::App::save(&mut app, &mut storage);
        let restored = open(&storage, None);
        assert!(restored.panes[0].closed_tabs.is_empty());
        assert_eq!(restored.panes[0].pinned_count(), 1);
        assert_eq!(restored.panes[0].tabs[0].path, remote);
        assert_eq!(restored.panes[0].tabs[1].path, local);
        assert_eq!(restored.panes[1].pinned_count(), 0);
        let explicit_start = open(&storage, Some(root.path().into()));
        assert!(!explicit_start.tab().pinned);
        assert_eq!(explicit_start.tab().path, local);
    }

    #[test]
    fn pane_layout_defaults_and_persists_through_settings() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut storage = MemoryStorage::default();
        eframe::Storage::set_string(
            &mut storage,
            PawCommander::SETTINGS_KEY,
            "(split:false,preview:false)".into(),
        );
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        cc.storage = Some(&storage);
        let mut app = PawCommander::new(&cc, Some(directory.path().into()));
        assert!(!app.settings.split);
        assert!(!app.settings.preview);
        assert_eq!(
            app.settings.pane_layout.orientation,
            PaneOrientation::SideBySide
        );
        assert_eq!(app.settings.pane_layout.fractions, [0.5, 0.5]);
        assert!(app.settings.pane_layout.sidebar_visible);

        app.settings.pane_layout.orientation = PaneOrientation::Stacked;
        app.settings.pane_layout.fractions = [0.35, 0.6];
        app.settings.pane_layout.sidebar_visible = false;
        eframe::App::save(&mut app, &mut storage);
        let mut cc = eframe::CreationContext::_new_kittest(ctx);
        cc.storage = Some(&storage);
        let reopened = PawCommander::new(&cc, None);
        assert_eq!(
            reopened.settings.pane_layout.orientation,
            PaneOrientation::Stacked
        );
        assert_eq!(reopened.settings.pane_layout.fractions, [0.35, 0.6]);
        assert!(!reopened.settings.pane_layout.sidebar_visible);
        assert!(!reopened.settings.split);
        assert!(!reopened.settings.preview);
    }

    #[test]
    fn pane_layout_drag_is_bounded_and_remembers_both_orientations() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 720.0));
        let draw = |layout: &mut PaneLayout, events| {
            let mut result = [egui::Rect::NOTHING; 2];
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        result = layout.rects(ui, ui.available_rect_before_wrap());
                    });
                },
            );
            (result, output.platform_output.cursor_icon)
        };
        let mut layout = PaneLayout::default();
        for orientation in [PaneOrientation::SideBySide, PaneOrientation::Stacked] {
            layout.orientation = orientation;
            let axis = orientation.axis();
            let (panes, _) = draw(&mut layout, vec![]);
            assert_eq!(panes[0].size(), panes[1].size());
            assert_eq!(panes[1].min[axis] - panes[0].max[axis], 8.0);
            let mut center = panes[0].center();
            center[axis] = panes[0].max[axis] + 4.0;
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                pressed,
                button: egui::PointerButton::Primary,
                modifiers: egui::Modifiers::NONE,
            };
            let (_, cursor) = draw(&mut layout, vec![egui::Event::PointerMoved(center)]);
            assert_eq!(
                cursor,
                if axis == 0 {
                    egui::CursorIcon::ResizeHorizontal
                } else {
                    egui::CursorIcon::ResizeVertical
                }
            );
            draw(&mut layout, vec![button(center, true)]);
            let mut edge = center;
            edge[axis] = screen.max[axis] - 1.0;
            let (resized, _) = draw(&mut layout, vec![egui::Event::PointerMoved(edge)]);
            draw(&mut layout, vec![button(edge, false)]);
            assert!(resized[0].size()[axis] > panes[0].size()[axis]);
            assert!(resized[1].size()[axis] >= [180.0, 220.0][axis]);
            assert_eq!(resized[1].min[axis] - resized[0].max[axis], 8.0);
        }
        assert!(layout.fractions.into_iter().all(|fraction| fraction > 0.5));
        layout.orientation = PaneOrientation::SideBySide;
        let (panes, _) = draw(&mut layout, vec![]);
        assert!(panes[0].width() > panes[1].width());
    }

    #[test]
    fn custom_columns_scroll_without_losing_rows_or_compact_headers() {
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let folder = tempfile::tempdir().unwrap();
        let mut app = PawCommander::new(&cc, Some(folder.path().into()));
        let mut frame = eframe::Frame::_new_kittest();
        app.settings.preview = false;
        app.settings.pane_layout.sidebar_visible = false;
        app.settings.list_columns = serde_json::from_value(serde_json::json!({
            "custom": true,
            "name_width": 600,
            "columns": [
                {"column":"Extension", "width":100, "visible":true},
                {"column":"Permissions", "width":100, "visible":true}
            ]
        }))
        .unwrap();
        app.settings.list_columns.validate();
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        let restored = eframe::get_value::<Settings>(&storage, PawCommander::SETTINGS_KEY).unwrap();
        assert_eq!(restored.list_columns, app.settings.list_columns);
        let path = app.tab().path.clone();
        let row = path.join("item040.txt");
        let entries = (0..100)
            .map(|index| {
                let name = format!("item{index:03}.txt");
                FileEntry {
                    path: path.join(&name),
                    name,
                    is_dir: false,
                    is_symlink: false,
                    size: 1,
                    modified: None,
                    metadata: Default::default(),
                }
            })
            .collect();
        let tab = app.tab_mut();
        tab.loading = None;
        tab.has_complete_listing = true;
        tab.entries.replace(entries);
        tab.scroll_to = Some(row.clone());
        let row_id = tab.entry_id(&row);
        let mut time = 0.0;
        let mut draw = |app: &mut PawCommander, events| {
            let mut response = None;
            time += 0.08;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(720.0, 430.0),
                    )),
                    events,
                    time: Some(time),
                    ..Default::default()
                },
                |ctx| {
                    eframe::App::update(app, ctx, &mut frame);
                    response = ctx.read_response(row_id);
                },
            );
            (output, response)
        };
        let scroll = |delta| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta,
            modifiers: egui::Modifiers::NONE,
        };
        let (_, initial) = draw(&mut app, vec![]);
        let initial = initial.unwrap();
        let y_before = app.tab().scroll_offset;
        assert!(initial.interact_rect.is_positive());
        assert!(app.tab().rendered_rows.len() < 20);
        draw(
            &mut app,
            vec![
                egui::Event::PointerMoved(initial.interact_rect.center()),
                scroll(egui::vec2(-700.0, 0.0)),
            ],
        );
        let (_, scrolled) = draw(&mut app, vec![]);
        let scrolled = scrolled.unwrap();
        assert!(
            scrolled.rect.left() < initial.rect.left(),
            "{initial:?} -> {scrolled:?}"
        );
        assert_eq!(app.tab().scroll_offset, y_before);
        draw(&mut app, vec![scroll(egui::vec2(0.0, -110.0))]);
        assert!(app.tab().scroll_offset > y_before);

        app.settings.pane_layout.orientation = PaneOrientation::Stacked;
        app.tab_mut().scroll_to = Some(row);
        let (compact, response) = draw(&mut app, vec![]);
        assert!(response.unwrap().interact_rect.is_positive());
        assert!(compact.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text) if matches!(
                text.galley.text(), "Extension" | "Расширение"
            ) && shape.clip_rect.contains(text.pos))
        }));
    }

    #[test]
    fn pane_layout_preserves_selection_and_scroll_at_minimum_size() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..100 {
            std::fs::write(root.path().join(format!("file{index:03}")), b"test").unwrap();
        }
        for grid in [false, true] {
            let ctx = egui::Context::default();
            let cc = eframe::CreationContext::_new_kittest(ctx.clone());
            let mut app = PawCommander::new(&cc, Some(root.path().into()));
            let mut frame = eframe::Frame::_new_kittest();
            let selected = app.tab().path.join("file040");
            let mut draw = |app: &mut PawCommander, height, events| {
                let mut input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(640.0, height),
                    )),
                    events,
                    ..Default::default()
                };
                eframe::App::raw_input_hook(app, &ctx, &mut input);
                let mut rows = [None, None];
                let output = ctx.run(input, |ctx| {
                    eframe::App::update(app, ctx, &mut frame);
                    for (index, pane) in app.panes.iter().enumerate() {
                        rows[index] = ctx.read_response(pane.tab().entry_id(&selected));
                    }
                });
                (output, rows)
            };
            wait_for_directory(app.panes[0].tab_mut());
            wait_for_directory(app.panes[1].tab_mut());
            app.active = 1;
            app.settings.pane_grid = Some([false, grid]);
            let tab_id = app.tab().id;
            app.tab_mut().scroll_to = Some(selected.clone());
            app.tab_mut().cursor = Some(selected.clone());
            app.tab_mut().selected.insert(selected.clone());
            let (_, rows) = draw(&mut app, 400.0, vec![]);
            let scope = app.tab().file_scroll_scope;
            let row = app.tab().entry_id(&selected);
            assert!(rows[1].as_ref().unwrap().interact_rect.is_positive());
            ctx.memory_mut(|memory| memory.request_focus(row));

            app.settings.pane_layout.orientation = PaneOrientation::Stacked;
            app.settings.pane_layout.sidebar_visible = false;
            let (_, rows) = draw(&mut app, 400.0, vec![]);
            assert_eq!(app.active, 1);
            assert_eq!(app.tab().id, tab_id);
            assert_eq!(app.tab().file_scroll_scope, scope);
            assert_eq!(app.tab().selected, BTreeSet::from([selected.clone()]));
            assert_eq!(app.file_row_focus(&ctx), Some((1, &selected)));
            assert!(rows[1].as_ref().unwrap().interact_rect.is_positive());

            draw(
                &mut app,
                400.0,
                vec![egui::Event::Key {
                    key: egui::Key::ArrowDown,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert_eq!(app.active, 1);
            assert_eq!(
                app.tab().selected,
                BTreeSet::from([app.tab().path.join("file041")])
            );

            app.settings.pane_layout.sidebar_visible = true;
            app.settings.pane_grid = Some([grid; 2]);
            for (height, fraction) in [
                (400.0, 0.0),
                (400.0, 1.0),
                (560.0, 0.0),
                (560.0, 1.0),
                (670.0, 0.5),
                (690.0, 0.5),
            ] {
                app.settings.pane_layout.fractions[1] = fraction;
                for pane in &mut app.panes {
                    pane.tab_mut().scroll_to = Some(selected.clone());
                }
                let (output, rows) = draw(&mut app, height, vec![]);
                for row in rows {
                    let row = row.unwrap();
                    assert!(
                        row.interact_rect.height() >= COMPACT_FILE_ROW_HEIGHT - 1.0,
                        "clickable file row at height {height}, fraction {fraction}, grid {grid}: {row:?}"
                    );
                    let labels = output
                        .shapes
                        .iter()
                        .filter_map(|clipped| match &clipped.shape {
                            egui::Shape::Text(text) if text.galley.text() == "file040" => {
                                Some((text.visual_bounding_rect(), clipped.clip_rect))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    assert!(labels.iter().any(|(label, clip)| {
                        row.interact_rect.contains_rect(*label) && clip.contains_rect(*label)
                    }), "filename must be visible in both panes at height {height}, fraction {fraction}, grid {grid}. Row {row:?}, labels {labels:?}");
                }
            }
        }
    }

    #[test]
    fn pane_layout_accepts_tab_drops_between_stacked_panes() {
        let root = tempfile::tempdir().unwrap();
        let top: Location = root.path().join("top").into();
        let bottom: Location = root.path().join("bottom").into();
        std::fs::create_dir(top.path()).unwrap();
        std::fs::create_dir(bottom.path()).unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(bottom.path().into()));
        app.settings.pane_layout.orientation = PaneOrientation::Stacked;
        let tab = Tab::new(top, &ctx);
        let id = tab.id;
        app.panes[0].tabs.push(tab);

        let mut frame = eframe::Frame::_new_kittest();
        let mut draw = |app: &mut PawCommander, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 700.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| eframe::App::update(app, ctx, &mut frame),
            )
        };
        let output = draw(&mut app, vec![]);
        let mut top_tab = None;
        let mut bottom_tab = None;
        for clipped in &output.shapes {
            if let egui::Shape::Text(text) = &clipped.shape {
                let center = text.pos + text.galley.size() * 0.5;
                if text.galley.text() == "top" {
                    top_tab = Some(center);
                }
                if text.galley.text() == "bottom"
                    && bottom_tab.is_none_or(|previous: egui::Pos2| previous.y < center.y)
                    && clipped.clip_rect.contains(center)
                {
                    bottom_tab = Some(center);
                }
            }
        }
        let start = top_tab.unwrap();
        let end = bottom_tab.unwrap();
        assert!(start.y < end.y);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        };
        draw(
            &mut app,
            vec![egui::Event::PointerMoved(start), button(start, true)],
        );
        draw(&mut app, vec![egui::Event::PointerMoved(end)]);
        draw(&mut app, vec![button(end, false)]);
        assert_eq!(app.panes[0].tabs.len(), 1);
        assert_eq!(app.panes[1].tab().id, id);
        assert_eq!(app.active, 1);
    }

    #[test]
    fn pane_views_migrate_and_persist_independently() {
        let mut settings = Settings {
            grid: true,
            ..Default::default()
        };
        assert!(settings.grid_for(0) && settings.grid_for(1));
        settings.pane_grid = Some([false, true]);
        let mut storage = MemoryStorage::default();
        eframe::set_value(&mut storage, "views", &settings);
        let restored: Settings = eframe::get_value(&storage, "views").unwrap();
        assert!(!restored.grid_for(0));
        assert!(restored.grid_for(1));
    }

    #[test]
    fn folder_view_follows_location_across_panes_refresh_and_restart() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let other = root.path().join("other");
        for folder in [&first, &other] {
            std::fs::create_dir(folder).unwrap();
        }
        for (name, data) in [("small", "s"), ("big", "large"), (".hidden", "xx")] {
            std::fs::write(first.join(name), data).unwrap();
        }
        let hidden: Location = first.join(".hidden").into();
        let big: Location = first.join("big").into();
        let small: Location = first.join("small").into();
        let other: Location = other.into();
        let ctx = egui::Context::default();
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(first.clone()));

        app.action(
            app.folder_view_action(0, FolderViewChange::Grid(true)),
            &ctx,
        );
        app.action(
            app.folder_view_action(1, FolderViewChange::Density(Density::Compact)),
            &ctx,
        );
        app.action(
            app.folder_view_action(1, FolderViewChange::Sort(Sort::Size)),
            &ctx,
        );
        assert!(!app.folder_view(0).order.descending);
        app.action(
            app.folder_view_action(0, FolderViewChange::Sort(Sort::Size)),
            &ctx,
        );
        app.run_shortcut(Command::Hidden, &ctx);
        let expected = FolderView {
            grid: true,
            hidden: true,
            density: Density::Compact,
            order: FileOrder {
                key: Sort::Size,
                descending: true,
            },
        };
        assert_eq!(app.folder_view(0), expected);
        assert_eq!(app.folder_view(1), expected);
        app.action(Action::Refresh, &ctx);
        for pane in &mut app.panes {
            wait_for_directory(pane.tab_mut());
        }
        app.run_shortcut(Command::SelectAll, &ctx);
        assert_eq!(
            app.selected(),
            vec![big.clone(), hidden.clone(), small.clone()]
        );
        app.panes[1].tab_mut().selected.insert(hidden.clone());
        app.run_shortcut(Command::Hidden, &ctx);
        assert!(!app.panes[1].tab().selected.contains(&hidden));
        assert_eq!(app.selected(), vec![big, small]);
        app.run_shortcut(Command::Hidden, &ctx);

        let stale_sort = app.folder_view_action(0, FolderViewChange::Sort(Sort::Name));
        app.action(Action::Navigate(0, other.clone()), &ctx);
        app.action(stale_sort, &ctx);
        assert_eq!(app.folder_view(0), FolderView::default());
        app.action(Action::Back, &ctx);
        assert_eq!(app.folder_view(0), expected);
        app.action(Action::Forward, &ctx);
        assert_eq!(app.folder_view(0), FolderView::default());
        assert_eq!(app.folder_view(1), expected);
        app.action(Action::Navigate(1, other), &ctx);

        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        cc.storage = Some(&storage);
        let mut reopened = PawCommander::new(&cc, None);
        assert_eq!(reopened.folder_view(0), FolderView::default());
        assert_eq!(reopened.folder_view(1), FolderView::default());
        reopened.action(Action::Back, &ctx);
        assert_eq!(reopened.folder_view(1), expected);
        reopened.action(Action::Navigate(0, first.into()), &ctx);
        assert_eq!(reopened.folder_view(0), expected);
    }

    #[test]
    fn folder_view_storage_keeps_native_names_and_distinct_sftp_keys() {
        #[cfg(unix)]
        let native = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(vec![b'x', 0xff])
        };
        #[cfg(windows)]
        let native = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[u16::from(b'x'), 0xd800])
        };
        let root = tempfile::tempdir().unwrap();
        let local: Location = root.path().join(&native).into();
        let remote_path = PathBuf::from("/folder").join(&native);
        let mut endpoint = Endpoint {
            host: "example.invalid".into(),
            port: 2022,
            user: "reader".into(),
            key: None,
        };
        let remote = Location::Sftp {
            endpoint: endpoint.clone(),
            path: remote_path.clone(),
        };
        endpoint.key = Some(root.path().join(&native));
        let keyed = Location::Sftp {
            endpoint,
            path: remote_path,
        };

        let ctx = egui::Context::default();
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.action(Action::Navigate(0, local.clone()), &ctx);
        app.action(
            app.folder_view_action(0, FolderViewChange::Grid(true)),
            &ctx,
        );
        app.action(Action::Navigate(0, remote.clone()), &ctx);
        app.action(
            app.folder_view_action(0, FolderViewChange::Hidden(true)),
            &ctx,
        );
        app.action(Action::Navigate(0, keyed.clone()), &ctx);
        app.action(
            app.folder_view_action(0, FolderViewChange::Density(Density::Compact)),
            &ctx,
        );
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        cc.storage = Some(&storage);
        let mut reopened = PawCommander::new(&cc, Some(root.path().into()));
        for (location, expected) in [
            (
                local,
                FolderView {
                    grid: true,
                    ..Default::default()
                },
            ),
            (
                remote,
                FolderView {
                    hidden: true,
                    ..Default::default()
                },
            ),
            (
                keyed,
                FolderView {
                    density: Density::Compact,
                    ..Default::default()
                },
            ),
        ] {
            reopened.action(Action::Navigate(0, location), &ctx);
            assert_eq!(reopened.folder_view(0), expected);
        }
    }

    #[test]
    fn folder_view_hiding_focused_row_cannot_delete_another_selected_file() {
        use egui::accesskit::{Action as AxAction, ActionRequest, NodeId};

        let root = tempfile::tempdir().unwrap();
        let hidden: Location = root.path().join(".hidden").into();
        let visible: Location = root.path().join("visible").into();
        for path in [&hidden, &visible] {
            std::fs::write(path.path(), "data").unwrap();
        }
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.run_shortcut(Command::Hidden, &ctx);
        wait_for_directory(app.tab_mut());
        app.tab_mut().selected.insert(visible.clone());
        let hidden_id = app.tab().entry_id(&hidden);
        let mut frame = eframe::Frame::_new_kittest();
        let mut run = |app: &mut PawCommander, events| {
            let mut input = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1440.0, 860.0),
                )),
                ..Default::default()
            };
            eframe::App::raw_input_hook(app, &ctx, &mut input);
            let _ = ctx.run(input, |ctx| eframe::App::update(app, ctx, &mut frame));
        };
        run(&mut app, vec![]);
        run(
            &mut app,
            vec![egui::Event::AccessKitActionRequest(ActionRequest {
                action: AxAction::Focus,
                target: NodeId(hidden_id.value()),
                data: None,
            })],
        );
        assert_eq!(app.file_row_focus(&ctx), Some((0, &hidden)));
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        run(
            &mut app,
            vec![
                key(egui::Key::H, egui::Modifiers::COMMAND),
                key(egui::Key::Delete, egui::Modifiers::NONE),
            ],
        );
        assert!(!app.folder_view(0).hidden);
        assert_eq!(app.selected(), vec![visible]);
        assert!(app.dialog.is_none());
    }

    #[test]
    fn workspace_session_restores_selection_sort_and_back_forward_with_missing_path() {
        let root = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let base: Location = root.path().to_path_buf().into();
        let left = base.join("left");
        let forward = base.join("forward");
        let missing = base.join("missing");
        for directory in [&left, &forward, &missing] {
            std::fs::create_dir(directory.path()).unwrap();
        }
        std::fs::write(left.join("present").path(), "data").unwrap();

        app.action(Action::TogglePinTab(0, 0), &ctx);
        app.action(Action::NewTab(0), &ctx);
        app.action(Action::Navigate(0, left.clone()), &ctx);
        app.action(Action::Navigate(0, forward.clone()), &ctx);
        app.action(Action::Back, &ctx);
        app.panes[0].tab_mut().sort = Sort::Size;
        app.panes[0].tab_mut().descending = true;
        app.action(
            app.folder_view_action(0, FolderViewChange::Grid(true)),
            &ctx,
        );
        app.action(Action::NewTab(1), &ctx);
        app.action(Action::Navigate(1, missing.clone()), &ctx);
        app.panes[1].tab_mut().sort = Sort::Modified;

        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        std::fs::remove_dir(missing.path()).unwrap();
        cc.storage = Some(&storage);
        let mut reopened = PawCommander::new(&cc, None);
        assert_eq!(reopened.active, 1);
        assert_eq!(reopened.panes[0].pinned_count(), 1);
        assert_eq!(reopened.panes[0].current, 1);
        assert_eq!(reopened.panes[1].current, 1);
        assert_eq!(reopened.panes[0].tab().path, left);
        assert_eq!(reopened.tab().path, missing);
        assert!(reopened.panes[0].tab().sort == Sort::Size);
        assert!(reopened.panes[0].tab().descending);
        assert!(reopened.tab().sort == Sort::Modified);
        assert!(!reopened.tab().descending);
        assert!(reopened.folder_view(0).grid);
        assert!(!reopened.folder_view(1).grid);
        wait_for_directory(reopened.panes[0].tab_mut());
        wait_for_directory(reopened.tab_mut());
        assert_eq!(reopened.panes[0].tab().entries[0].name, "present");
        assert!(reopened.tab().error.is_some());
        reopened.action(Action::History(0, -1), &ctx);
        assert_eq!(reopened.tab().path, base);
        reopened.action(Action::Forward, &ctx);
        assert_eq!(reopened.tab().path, left);
        reopened.action(Action::Forward, &ctx);
        assert_eq!(reopened.tab().path, forward);
        reopened.action(Action::History(1, -1), &ctx);
        assert_eq!(reopened.tab().path, base);

        let explicit = PawCommander::new(&cc, Some(forward.path().to_path_buf()));
        assert_eq!(explicit.active, 0);
        for pane in &explicit.panes {
            assert_eq!(pane.tabs.len(), 1);
            assert_eq!(pane.tab().path, forward);
            assert_eq!(pane.tab().history, vec![forward.clone()]);
            assert!(!pane.tab().pinned);
            assert!(pane.tab().sort == Sort::Name);
        }

        eframe::Storage::set_string(
            &mut storage,
            PawCommander::SESSION_KEY,
            "invalid session".into(),
        );
        let mut fallback_cc = eframe::CreationContext::_new_kittest(ctx.clone());
        fallback_cc.storage = Some(&storage);
        let legacy_fallback = PawCommander::new(&fallback_cc, None);
        assert_eq!(legacy_fallback.panes[0].pinned_count(), 1);
        assert_eq!(legacy_fallback.panes[0].tabs[1].path, left);
        assert_eq!(legacy_fallback.panes[1].tabs[1].path, missing);
        assert!(
            legacy_fallback.panes[0].tabs[1]
                .folder_view(&legacy_fallback.settings, 0, &legacy_fallback.folder_views)
                .grid
        );
    }

    #[test]
    fn session_preserves_native_local_paths_and_remote_coordinates() {
        #[cfg(unix)]
        let native = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(vec![b'x', 0xff])
        };
        #[cfg(windows)]
        let native = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[u16::from(b'x'), 0xd800])
        };
        let root = tempfile::tempdir().unwrap();
        let local: Location = root.path().join(&native).into();
        let remote = Location::Sftp {
            endpoint: Endpoint {
                host: "example.invalid".into(),
                port: 2022,
                user: "user".into(),
                key: Some(root.path().join(&native)),
            },
            path: PathBuf::from("/folder").join(native),
        };
        let ctx = egui::Context::default();
        let mut cc = eframe::CreationContext::_new_kittest(ctx);
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.tab_mut().navigate(local.clone(), &cc.egui_ctx);
        app.tab_mut().history.push(remote.clone());
        app.tab_mut().pinned = true;
        app.settings.hidden = true;
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        let legacy: Settings = eframe::get_value(&storage, PawCommander::SETTINGS_KEY).unwrap();
        assert!(legacy.locations[0].is_empty());
        assert_eq!(legacy.pinned_tab_counts, [0, 0]);
        cc.storage = Some(&storage);
        let reopened = PawCommander::new(&cc, None);
        assert!(reopened.settings.hidden);
        assert_eq!(reopened.tab().path, local);
        assert_eq!(reopened.tab().history[2], remote);
        assert!(reopened.tab().pinned);
    }

    #[test]
    fn named_workspace_opens_both_folders_in_existing_tabs_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let base: Location = root.path().to_path_buf().into();
        let left = base.join("left");
        let right = base.join("right");
        for folder in [&left, &right] {
            std::fs::create_dir(folder.path()).unwrap();
        }
        let ctx = egui::Context::default();
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        app.action(Action::Navigate(0, left.clone()), &ctx);
        app.action(Action::Navigate(1, right.clone()), &ctx);
        app.save_workspace_name(None, "  Daily work  ".into());

        app.action(Action::NewTab(0), &ctx);
        app.action(Action::TogglePinTab(0, 1), &ctx);
        app.action(Action::Navigate(0, base.clone()), &ctx);
        app.action(Action::Navigate(1, base.clone()), &ctx);
        app.toggle_split();
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        cc.storage = Some(&storage);
        let mut reopened = PawCommander::new(&cc, None);
        assert!(!reopened.settings.split);
        assert_eq!(reopened.workspaces[0].name, "Daily work");
        let tab_ids: Vec<_> = reopened.panes.iter().map(|pane| pane.tab().id).collect();

        reopened.action(Action::OpenWorkspace(0), &ctx);
        assert!(reopened.settings.split);
        assert_eq!(reopened.active, 0);
        assert_eq!(reopened.panes[0].tabs.len(), 2);
        assert_eq!(reopened.panes[1].tabs.len(), 1);
        assert!(reopened.tab().pinned);
        assert_eq!(reopened.tab().path, left);
        assert_eq!(reopened.panes[1].tab().path, right);
        for (pane, previous_id) in reopened.panes.iter().zip(tab_ids) {
            assert_eq!(pane.tab().id, previous_id);
        }
        reopened.action(Action::Back, &ctx);
        assert_eq!(reopened.tab().path, base);
        reopened.action(Action::History(1, -1), &ctx);
        assert_eq!(reopened.tab().path, base);
    }

    #[test]
    fn bookmark_and_workspace_settings_round_trip_native_paths() {
        #[cfg(unix)]
        let native = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(vec![b'x', 0xff])
        };
        #[cfg(windows)]
        let native = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[u16::from(b'x'), 0xd800])
        };
        let root = tempfile::tempdir().unwrap();
        let native_folder: Location = root.path().join(native).into();
        let ctx = egui::Context::default();
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        app.bookmarks.push(Bookmark::new(native_folder.clone()));
        app.settings.ui_scale = 125;
        app.action(Action::Navigate(1, native_folder.clone()), &ctx);
        app.save_workspace_name(None, "Native folders".into());
        eframe::App::save(&mut app, &mut storage);
        assert_ne!(storage.0[PawCommander::SETTINGS_KEY], "{}");

        cc.storage = Some(&storage);
        let reopened = PawCommander::new(&cc, Some(root.path().into()));
        assert_eq!(reopened.bookmarks[0].location, native_folder);
        assert_eq!(reopened.settings.ui_scale, 125);
        assert_eq!(reopened.workspaces[0].name, "Native folders");
        assert_eq!(
            reopened.workspaces[0].folders[1].clone().into_location(),
            Some(native_folder)
        );
    }

    #[test]
    fn named_workspace_dialog_rejects_duplicates_and_supports_rename_and_remove() {
        let root = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        let enter = || egui::RawInput {
            events: [true, false]
                .map(|pressed| egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                })
                .to_vec(),
            ..Default::default()
        };
        let submit = |app: &mut PawCommander, name: &str| {
            let Some(Dialog::WorkspaceName { name: field, .. }) = &mut app.dialog else {
                panic!("expected workspace dialog");
            };
            *field = name.into();
            let _ = ctx.run(enter(), |ctx| app.dialogs(ctx));
        };

        app.action(Action::SaveWorkspace, &ctx);
        submit(&mut app, "   ");
        assert!(app.dialog.is_some());
        assert!(app.workspaces.is_empty());
        submit(&mut app, " Daily ");
        assert!(app.dialog.is_none());
        app.action(Action::SaveWorkspace, &ctx);
        submit(&mut app, "Daily");
        assert!(app.dialog.is_some());
        assert_eq!(app.workspaces.len(), 1);
        submit(&mut app, "Archive");
        assert!(app.dialog.is_none());

        let _ = ctx.run(enter(), |ctx| {
            app.action(Action::RenameWorkspace(0), ctx);
            app.dialogs(ctx);
        });
        assert!(matches!(
            &app.dialog,
            Some(Dialog::WorkspaceName { editing: Some(0), name }) if name == "Daily"
        ));
        submit(&mut app, "Archive");
        assert!(app.dialog.is_some());
        submit(&mut app, " Daily ");
        assert!(app.dialog.is_none());
        app.action(Action::RenameWorkspace(0), &ctx);
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.dialogs(ctx));
        submit(&mut app, "Regular");
        assert_eq!(app.workspaces[0].name, "Regular");
        assert_eq!(app.workspaces[1].name, "Archive");
        app.action(Action::RemoveWorkspace(0), &ctx);
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.workspaces[0].name, "Archive");
    }

    #[test]
    fn named_workspace_persists_sftp_coordinates_without_credentials() {
        let root = tempfile::tempdir().unwrap();
        let local: Location = root.path().to_path_buf().into();
        let key = root.path().join("disposable-key");
        std::fs::write(&key, "synthetic-private-key-material").unwrap();
        let remote = Location::Sftp {
            endpoint: Endpoint {
                host: "workspace-test.invalid".into(),
                port: 2022,
                user: "workspace-test".into(),
                key: Some(key),
            },
            path: PathBuf::from("/folder/папка"),
        };
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut app = PawCommander::new(&cc, Some(root.path().into()));
        // Supply the location without starting a network or keyring request.
        app.panes[1].tab_mut().path = remote.clone();
        app.settings.sftp_profiles = vec![remote.clone()];
        app.sftp_form = Some(SftpForm {
            password: "synthetic-account-password".into(),
            passphrase: "synthetic-key-passphrase".into(),
            save_key: true,
            ..Default::default()
        });
        app.save_workspace_name(None, "With SFTP".into());
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        let stored: Settings = eframe::get_value(&storage, PawCommander::SETTINGS_KEY).unwrap();
        let workspaces: Vec<NamedWorkspace> =
            eframe::get_value(&storage, PawCommander::WORKSPACES_KEY).unwrap();
        let folders = workspaces[0]
            .folders
            .clone()
            .map(SavedLocation::into_location);
        assert_eq!(folders, [Some(local), Some(remote.clone())]);
        assert_eq!(stored.sftp_profiles, vec![remote.clone()]);
        for payload in storage.0.values() {
            for secret in [
                "synthetic-private-key-material",
                "synthetic-account-password",
                "synthetic-key-passphrase",
            ] {
                assert!(!payload.contains(secret));
            }
        }

        let loopback = Location::Sftp {
            endpoint: Endpoint {
                host: "127.0.0.1".into(),
                port: 0,
                user: "test".into(),
                key: None,
            },
            path: PathBuf::from("/workspace"),
        };
        app.sftp_form = None;
        app.panes[1].tab_mut().path = loopback.clone();
        app.save_workspace_name(None, "Loopback".into());
        app.action(Action::Navigate(1, root.path().to_path_buf().into()), &ctx);
        app.action(Action::OpenWorkspace(1), &ctx);
        assert_eq!(app.panes[1].tab().path, loopback);
        assert_eq!(app.settings.sftp_profiles, vec![remote]);
    }

    #[test]
    fn address_editor_submits_typed_paths_completes_folders_and_cancels() {
        let ctx = egui::Context::default();
        Theme::Frappe.apply(&ctx, false);
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("Two Paths")).unwrap();
        std::fs::create_dir_all(root.path().join("Two Words/Nested folder")).unwrap();
        std::fs::write(root.path().join("Two Before Folders.txt"), "").unwrap();
        let original: Location = root.path().to_path_buf().into();
        let mut tab = Tab::new(original.clone(), &ctx);
        tab.begin_address();
        let mut time = 0.0;
        let suggestion_position = std::cell::Cell::new(None);
        let mut render = |tab: &mut Tab, events| {
            time += 0.1;
            let mut actions = vec![];
            let output = ctx.run(
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
            suggestion_position.set(output.shapes.iter().find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape {
                    if text.galley.text() == "Nested folder" {
                        return Some(text.visual_bounding_rect().center());
                    }
                }
                None
            }));
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
        ctx.memory_mut(|memory| {
            let field = memory.focused().expect("path field focus");
            memory.set_focus_lock_filter(field, egui::EventFilter::default());
        });
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
        render(&mut tab, vec![egui::Event::Text("nest".into())]);
        assert!(
            tab.address.ends_with("nest"),
            "Tab keeps the path field focused"
        );
        wait_for_suggestions(&mut tab, &mut render);
        render(&mut tab, vec![key(egui::Key::ArrowDown)]);
        let actions = render(&mut tab, vec![key(egui::Key::Enter)]);
        assert!(actions.iter().any(|a| matches!(
            a, Action::Navigate(1, p) if *p == original.join("Two Words").join("Nested folder")
        )));
        assert!(!tab.edit_address);

        tab.begin_address();
        render(&mut tab, vec![]);
        render(&mut tab, vec![egui::Event::Paste("Two Words/nest".into())]);
        wait_for_suggestions(&mut tab, &mut render);
        // Run another frame so the popup has completed egui's initial sizing pass.
        render(&mut tab, vec![]);
        let pos = suggestion_position.get().expect("visible suggestion");
        let pointer = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        render(
            &mut tab,
            vec![egui::Event::PointerMoved(pos), pointer(true)],
        );
        let actions = render(&mut tab, vec![pointer(false)]);
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
        assert_eq!(legacy.shortcuts, Shortcuts::default());
        assert_eq!(legacy.pinned_tab_counts, [0; 2]);
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
    fn bookmark_store_migrates_legacy_and_round_trips_named_sftp() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let mut storage = MemoryStorage::default();
        #[derive(Serialize)]
        struct Legacy {
            bookmarks: Vec<Location>,
        }
        let legacy_remote = fs_ops::resolve(
            "sftp://alice@example.com:2222/legacy",
            &Location::Local(PathBuf::from("/")),
        )
        .unwrap();
        eframe::set_value(
            &mut storage,
            PawCommander::SETTINGS_KEY,
            &Legacy {
                bookmarks: vec![
                    PathBuf::from("/tmp/legacy-folder").into(),
                    legacy_remote.clone(),
                ],
            },
        );
        cc.storage = Some(&storage);
        let legacy = PawCommander::new(&cc, Some(root.path().into()));
        assert_eq!(legacy.bookmarks.len(), 2);
        assert_eq!(legacy.bookmarks[0].name, "legacy-folder");
        assert_eq!(
            legacy.bookmarks[0].location,
            PathBuf::from("/tmp/legacy-folder").into()
        );
        assert_eq!(legacy.bookmarks[1].location, legacy_remote);
        drop(cc);

        let remote = fs_ops::resolve(
            "sftp://alice@example.com:2222/папка",
            &Location::Local(PathBuf::from("/")),
        )
        .unwrap();
        let mut bookmark = Bookmark::new(remote.clone());
        bookmark.name = "Remote work".into();
        bookmark.group = "Servers".into();
        bookmark.shortcut = Some(4);
        eframe::set_value(
            &mut storage,
            PawCommander::BOOKMARKS_KEY,
            &vec![SavedBookmark::from(&bookmark)],
        );
        let mut cc = eframe::CreationContext::_new_kittest(ctx.clone());
        cc.storage = Some(&storage);
        let restored = PawCommander::new(&cc, Some(root.path().into()));
        assert_eq!(restored.bookmarks.len(), 1);
        assert_eq!(restored.bookmarks[0].location, remote);
        assert_eq!(restored.bookmarks[0].name, "Remote work");
        assert_eq!(restored.bookmarks[0].group, "Servers");
        assert_eq!(restored.bookmarks[0].shortcut, Some(4));
    }

    #[test]
    fn bookmark_shortcuts_reject_conflicts_and_stay_with_bookmark_on_reorder() {
        let mut bookmarks = vec![
            Bookmark::new(PathBuf::from("/a").into()),
            Bookmark::new(PathBuf::from("/b").into()),
            Bookmark::new(PathBuf::from("/c").into()),
        ];
        bookmarks[0].group = "Work".into();
        bookmarks[1].group = "Other".into();
        bookmarks[2].group = "Work".into();
        let edit_id = bookmarks[0].id;
        let chord = shortcuts::chord(egui::Key::Num1, egui::Modifiers::ALT);
        let shortcuts: Shortcuts = serde_json::from_value(serde_json::json!({
            "overrides": {"NewTab": chord}
        }))
        .expect("custom shortcut");
        assert!(!assign_bookmark_shortcut(
            &mut bookmarks,
            0,
            Some(1),
            &shortcuts
        ));
        assert!(assign_bookmark_shortcut(
            &mut bookmarks,
            0,
            Some(2),
            &Shortcuts::default()
        ));
        assert!(!assign_bookmark_shortcut(
            &mut bookmarks,
            1,
            Some(2),
            &Shortcuts::default()
        ));
        let peer = bookmark_peer(&bookmarks, 0, 1).expect("next bookmark in group");
        assert_eq!(peer, 2);
        assert!(move_bookmark(&mut bookmarks, 0, peer));
        assert_eq!(bookmarks[2].location, PathBuf::from("/a").into());
        assert_eq!(bookmarks[2].shortcut, Some(2));
        assert!(apply_bookmark_edit(
            &mut bookmarks,
            edit_id,
            "  Project files ",
            "  Projects  "
        ));
        assert_eq!(bookmarks[2].name, "Project files");
        assert_eq!(bookmarks[2].group, "Projects");
        let later_command: Shortcuts = serde_json::from_value(serde_json::json!({
            "overrides": {"NewTab": shortcuts::chord(egui::Key::Num2, egui::Modifiers::ALT)}
        }))
        .expect("later custom shortcut");
        assert!(validate_bookmark_shortcuts(&mut bookmarks, &later_command));
        assert!(bookmarks.iter().all(|bookmark| bookmark.shortcut.is_none()));
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
        let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Reload);
        tab.loading = Some(load);
        tab.reveal_after_load = Some(location.clone());
        tx.send(DirectoryRead::Finished(
            fs_ops::read_directory(&tab.path, &|| false).map(Some),
        ))
        .unwrap();
        tab.poll();
        assert_eq!(tab.selected, BTreeSet::from([location.clone()]));
        assert_eq!(tab.cursor, Some(location.clone()));
        assert_eq!(tab.scroll_to, Some(location.clone()));
        assert!(tab.reveal_after_load.is_none());
        assert!(visible_rows(&tab, true)
            .iter()
            .any(|i| tab.entries[*i].path == location));

        std::fs::remove_file(path).unwrap();
        let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Reload);
        tab.loading = Some(load);
        tab.reveal_after_load = Some(location);
        tx.send(DirectoryRead::Finished(
            fs_ops::read_directory(&tab.path, &|| false).map(Some),
        ))
        .unwrap();
        tab.poll();
        assert!(tab.selected.is_empty());
        assert!(tab.error.unwrap().contains("index may be outdated"));
    }

    #[test]
    fn relative_address_preserves_spaces() {
        assert_eq!(
            crate::address::Completion::default()
                .resolve("two words", &PathBuf::from("/tmp").into())
                .unwrap(),
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
            tab.entries.replace(vec![FileEntry {
                path: path.clone(),
                name: ".env".into(),
                is_dir: false,
                is_symlink: false,
                size: 12,
                modified: None,
                metadata: Default::default(),
            }]);
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
                                    columns: None,
                                    pane: 1,
                                    split: true,
                                    grid: false,
                                    compact: false,
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
            tab.entries
                .replace(fs_ops::read_directory(&tab.path, &|| false).unwrap());
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
                            let visible = visible_rows(&tab, false);
                            entry_widget(
                                &mut cols[1],
                                &mut tab,
                                0,
                                &visible,
                                EntryOptions {
                                    columns: None,
                                    pane: 1,
                                    split: true,
                                    grid,
                                    compact: false,
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
            let visible = visible_rows(&tab, false);
            let paths = tab.drag_paths(visible[0], &visible);
            assert_eq!(paths.len(), 1);
            assert_eq!(tab.selected.len(), 1);
        }
    }

    fn visible_rows(tab: &Tab, hidden: bool) -> Rc<Vec<usize>> {
        tab.visible(FolderView {
            hidden,
            order: tab.legacy_order(),
            ..Default::default()
        })
    }

    #[test]
    fn filters_sort_and_range_selection() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
        tab.entries.replace(
            [
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
                metadata: Default::default(),
            })
            .collect(),
        );
        let visible = visible_rows(&tab, false);
        assert_eq!(visible.as_slice(), &[1, 2, 0]);
        tab.choose(1, &visible, egui::Modifiers::NONE);
        tab.choose(0, &visible, egui::Modifiers::SHIFT);
        assert_eq!(tab.selected.len(), 3);
        tab.filter = "TXT".into();
        assert_eq!(visible_rows(&tab, false).as_slice(), &[2, 0]);
        tab.descending = true;
        tab.filter.clear();
        assert_eq!(visible_rows(&tab, false).as_slice(), &[1, 0, 2]);
    }

    #[test]
    fn visible_rows_follow_batches_query_changes_and_replacements() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let mut tab = Tab::new(root.path().to_path_buf().into(), &ctx);
        let entry = |name: &str, size: u64, is_dir: bool| FileEntry {
            path: root.path().join(name).into(),
            name: name.into(),
            is_dir,
            is_symlink: false,
            size,
            modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(size)),
            metadata: Default::default(),
        };
        let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Progressive);
        tab.loading = Some(load);
        tx.send(DirectoryRead::Batch(vec![
            entry("z.txt", 20, false),
            entry("Ä.txt", 4, false),
            entry(".hidden", 8, false),
        ]))
        .unwrap();
        assert!(tab.poll());
        let initial = visible_rows(&tab, false);
        assert_eq!(initial.as_slice(), &[0, 1]);
        assert!(Rc::ptr_eq(&initial, &visible_rows(&tab, false)));
        tab.descending = true;
        assert_eq!(visible_rows(&tab, false).as_slice(), &[1, 0]);

        tx.send(DirectoryRead::Batch(vec![
            entry("D", 2, true),
            entry("ä.txt", 4, false),
        ]))
        .unwrap();
        assert!(tab.poll());
        assert_eq!(visible_rows(&tab, false).as_slice(), &[3, 4, 1, 0]);
        assert_eq!(initial.as_slice(), &[0, 1]);
        tab.filter = "Ä.TXT".into();
        assert_eq!(visible_rows(&tab, false).as_slice(), &[4, 1]);
        tab.filter.clear();
        assert_eq!(visible_rows(&tab, true).as_slice(), &[3, 4, 1, 0, 2]);
        tab.sort = Sort::Modified;
        assert_eq!(visible_rows(&tab, true).as_slice(), &[3, 0, 2, 4, 1]);

        tx.send(DirectoryRead::Finished(Ok(None))).unwrap();
        assert!(!tab.poll());
        let (tx, load) = DirectoryLoad::channel(DirectoryLoadMode::Reload);
        tab.loading = Some(load);
        let mut replacement = tab.entries.to_vec();
        replacement[0] = entry("B.txt", 1, false);
        tx.send(DirectoryRead::Finished(Ok(Some(replacement))))
            .unwrap();
        assert!(!tab.poll());
        assert_eq!(visible_rows(&tab, true).as_slice(), &[3, 2, 4, 1, 0]);
        tab.sort = Sort::Name;
        tab.filter = "B.TXT".into();
        assert_eq!(visible_rows(&tab, false).as_slice(), &[0]);

        let next = tempfile::tempdir().unwrap();
        std::fs::write(next.path().join("next"), "").unwrap();
        tab.navigate(next.path().to_path_buf().into(), &ctx);
        assert!(visible_rows(&tab, false).is_empty());
        wait_for_directory(&mut tab);
        assert_eq!(visible_rows(&tab, false).as_slice(), &[0]);
        assert_eq!(tab.entries[0].name, "next");
    }
}
