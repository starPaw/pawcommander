use super::pane_view::workspace_border;
use crate::folder_jump::FolderHistory;
use crate::icons::Icon;
use crate::list_columns::ListColumns;
use crate::shortcuts::Shortcuts;
use crate::theme::{Palette, Theme};
use crate::vfs::{Endpoint, Location};
use eframe::egui;
use eframe::egui::{Sense, Stroke};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Settings {
    pub(super) locations: Vec<Vec<Location>>,
    pub(super) pinned_tab_counts: [usize; 2],
    pub(super) bookmarks: Vec<Location>,
    pub(super) folder_history: FolderHistory,
    pub(super) split: bool,
    pub(super) pane_layout: PaneLayout,
    pub(super) preview: bool,
    pub(super) verify_copies: bool,
    pub(super) diff_tool: String,
    pub(super) hidden: bool,
    pub(super) grid: bool,
    pub(super) pane_grid: Option<[bool; 2]>,
    pub(super) list_columns: ListColumns,
    pub(super) theme: Theme,
    pub(super) ui_scale: u16,
    pub(super) high_contrast: bool,
    pub(super) language: crate::i18n::Language,
    pub(super) sftp_profiles: Vec<Location>,
    pub(super) shortcuts: Shortcuts,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum PaneOrientation {
    #[default]
    SideBySide,
    Stacked,
}

impl PaneOrientation {
    pub(super) fn axis(self) -> usize {
        match self {
            Self::SideBySide => 0,
            Self::Stacked => 1,
        }
    }

    pub(super) fn label(self) -> &'static str {
        crate::i18n::text(match self {
            Self::SideBySide => "Рядом",
            Self::Stacked => "Одна над другой",
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub(super) struct PaneLayout {
    pub(super) orientation: PaneOrientation,
    pub(super) fractions: [f32; 2],
    pub(super) sidebar_visible: bool,
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
    pub(super) fn rects(&mut self, ui: &mut egui::Ui, workspace: egui::Rect) -> [egui::Rect; 2] {
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
        let response = ui.interact(
            divider,
            ui.scope_id().with(("pane_divider", axis)),
            Sense::DRAG,
        );
        let primary = egui::PointerButton::Primary;
        let grab_id = response.id.with("grab_offset");
        if response.drag_started_by(primary)
            && let Some(origin) = ui.input(|input| input.pointer.press_origin())
        {
            ui.data_mut(|data| {
                data.insert_temp(grab_id, origin[axis] - divider.center()[axis]);
            });
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

impl Settings {
    pub(super) const UI_SCALES: [u16; 5] = [90, 100, 110, 125, 150];

    pub(super) fn set_ui_scale(&mut self, ctx: &egui::Context, scale: u16) {
        self.ui_scale = if Self::UI_SCALES.contains(&scale) {
            scale
        } else {
            100
        };
        ctx.set_zoom_factor(f32::from(self.ui_scale) / 100.0);
    }

    pub(super) fn grid_for(&self, index: usize) -> bool {
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
            diff_tool: String::new(),
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
pub(super) enum Sort {
    #[default]
    Name,
    Size,
    Modified,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct FileOrder {
    pub(super) key: Sort,
    pub(super) descending: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Density {
    #[default]
    Standard,
    Compact,
}

impl Density {
    pub(super) fn label(self) -> &'static str {
        crate::i18n::text(match self {
            Self::Standard => "Обычная",
            Self::Compact => "Компактная",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct FolderView {
    pub(super) mode: FolderMode,
    pub(super) hidden: bool,
    pub(super) density: Density,
    pub(super) order: FileOrder,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum FolderMode {
    #[default]
    List,
    Grid,
    Columns,
}

impl FolderMode {
    pub(super) fn from_grid(grid: bool) -> Self {
        if grid {
            Self::Grid
        } else {
            Self::List
        }
    }

    pub(super) fn label(self) -> &'static str {
        crate::i18n::text(match self {
            Self::List => "Список",
            Self::Grid => "Сетка",
            Self::Columns => "Колоночный режим",
        })
    }

    pub(super) fn icon(self) -> Icon {
        match self {
            Self::List => Icon::List,
            Self::Grid => Icon::Grid,
            Self::Columns => Icon::FolderColumns,
        }
    }
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct FolderViewOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) mode: Option<FolderMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) grid: Option<bool>,
    pub(super) hidden: Option<bool>,
    pub(super) density: Option<Density>,
    pub(super) order: Option<FileOrder>,
}

impl FolderViewOverrides {
    pub(super) fn resolve(&self, defaults: FolderView) -> FolderView {
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

pub(super) type FolderViews = BTreeMap<Location, FolderViewOverrides>;

pub(super) fn is_dot_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

#[derive(Serialize, Deserialize)]
pub(super) struct SavedFolderView {
    pub(super) location: SavedLocation,
    pub(super) view: FolderViewOverrides,
}

#[derive(Clone, Copy)]
pub(super) enum FolderViewChange {
    Mode(FolderMode),
    Hidden(bool),
    Density(Density),
    Sort(Sort),
}

#[derive(Serialize, Deserialize)]
pub(super) struct WorkspaceSession {
    pub(super) panes: [PaneSession; 2],
    pub(super) active: usize,
}

#[derive(Serialize, Deserialize)]
pub(super) struct NamedWorkspace {
    pub(super) name: String,
    pub(super) folders: [SavedLocation; 2],
}

#[derive(Serialize, Deserialize)]
pub(super) struct PaneSession {
    pub(super) tabs: Vec<TabSession>,
    pub(super) current: usize,
}

#[derive(Serialize, Deserialize)]
pub(super) struct TabSession {
    pub(super) path: SavedLocation,
    pub(super) pinned: bool,
    pub(super) history: Vec<SavedLocation>,
    pub(super) history_index: usize,
    pub(super) sort: Sort,
    pub(super) descending: bool,
    pub(super) scroll_offset: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) browse_columns: bool,
}

// Local paths or SFTP connection coordinates and a key-file reference. Secrets
// belong to SFTP sessions and the credential store, never to saved locations.
#[derive(Clone, Serialize, Deserialize)]
pub(super) enum SavedLocation {
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
    pub(super) fn into_location(self) -> Option<Location> {
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
