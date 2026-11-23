use super::bookmarks::{
    reserve_bookmark_ids, validate_bookmark_shortcuts, Bookmark, SavedBookmark,
};
use super::directory::FolderVisit;
use super::jobs::JobQueue;
use super::state::{NamedWorkspace, SavedFolderView, Settings, WorkspaceSession};
use super::tabs::{Pane, Tab};
use super::PawCommander;
use crate::directory_tree::DirectoryTree;
use crate::file_clipboard::SystemClipboard;
use crate::preview::Preview;
use crate::result_history::History;
use crate::thumbnails::Thumbnails;
use crate::vfs::Location;
use eframe::egui;
use std::path::{Path, PathBuf};

pub(crate) enum Startup {
    Restore,
    Folder {
        path: PathBuf,
        search: Option<String>,
    },
    #[cfg(target_os = "macos")]
    Workspace([Location; 2]),
}

impl From<Option<PathBuf>> for Startup {
    fn from(start: Option<PathBuf>) -> Self {
        start.map_or(Self::Restore, |path| Self::Folder { path, search: None })
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceError {
    Missing,
    Incompatible,
    Corrupt,
}

#[cfg(target_os = "macos")]
pub(crate) fn find_saved_workspace(
    profile: Option<&[u8]>,
    name: &str,
) -> Result<[Location; 2], WorkspaceError> {
    let profile = profile.ok_or(WorkspaceError::Missing)?;
    let entries: std::collections::HashMap<String, String> =
        ron::de::from_bytes(profile).map_err(|_| WorkspaceError::Corrupt)?;
    let workspaces = entries
        .get(PawCommander::WORKSPACES_KEY)
        .ok_or(WorkspaceError::Missing)?;
    let workspaces: Vec<NamedWorkspace> =
        ron::from_str(workspaces).map_err(|_| WorkspaceError::Corrupt)?;
    let workspace = workspaces
        .iter()
        .find(|item| item.name == name)
        .ok_or(WorkspaceError::Missing)?;
    workspace.locations().ok_or(WorkspaceError::Incompatible)
}

impl PawCommander {
    pub(super) const SETTINGS_KEY: &str = "pawcommander.settings";

    pub(super) const SESSION_KEY: &str = "pawcommander.session";

    pub(super) const WORKSPACES_KEY: &str = "pawcommander.workspaces";

    #[cfg(target_os = "macos")]
    pub(super) const SYNC_PROFILES_KEY: &str = "pawcommander.sync_profiles";

    #[cfg(target_os = "macos")]
    pub(super) const COLLECTIONS_KEY: &str = "pawcommander.collections";

    #[cfg(target_os = "macos")]
    pub(super) const FILE_TEMPLATES_KEY: &str = "pawcommander.file_templates";

    #[cfg(target_os = "macos")]
    pub(super) const EXTENSIONS_KEY: &str = "pawcommander.viewer_extensions";

    pub(super) const BOOKMARKS_KEY: &str = "pawcommander.bookmarks";

    pub(super) const FOLDER_VIEWS_KEY: &str = "pawcommander.folder_views";

    #[cfg(test)]
    pub fn new(cc: &eframe::CreationContext<'_>, start: Option<PathBuf>) -> Self {
        Self::start(cc, start.into())
    }

    pub(crate) fn start(cc: &eframe::CreationContext<'_>, startup: Startup) -> Self {
        let mut settings: Settings = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, Self::SETTINGS_KEY))
            .unwrap_or_default();
        let workspaces = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, Self::WORKSPACES_KEY))
            .unwrap_or_default();
        #[cfg(target_os = "macos")]
        let sync_profiles = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, Self::SYNC_PROFILES_KEY))
            .unwrap_or_default();
        #[cfg(target_os = "macos")]
        let collections = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, Self::COLLECTIONS_KEY))
            .unwrap_or_default();
        #[cfg(target_os = "macos")]
        let file_templates = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, Self::FILE_TEMPLATES_KEY))
            .unwrap_or_default();
        #[cfg(target_os = "macos")]
        let extensions = super::extensions::Extensions::restore(
            cc.storage
                .and_then(|storage| eframe::get_value(storage, Self::EXTENSIONS_KEY))
                .unwrap_or_default(),
        );
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
        let mut search = crate::search::Search::load(cc.storage);
        let (panes, active) = match startup {
            Startup::Folder {
                path,
                search: query,
            } => {
                let path = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| home.clone())
                        .join(path)
                };
                let location = Location::Local(path);
                if let Some(query) = query {
                    search.open_name_query(&location, query, &cc.egui_ctx);
                }
                // Record a shared opening of the same directory once.
                let visit = FolderVisit::SharedOpen(Default::default());
                let panes = [location.clone(), location]
                    .into_iter()
                    .map(|location| {
                        let mut tab = Tab::new(location, &cc.egui_ctx);
                        tab.folder_visit = visit.clone();
                        Pane::new(vec![tab])
                    })
                    .collect();
                (panes, 0)
            }
            #[cfg(target_os = "macos")]
            Startup::Workspace(folders) => {
                settings.split = true;
                let panes = folders
                    .into_iter()
                    .map(|folder| Pane::new(vec![Tab::new(folder, &cc.egui_ctx)]))
                    .collect();
                (panes, 0)
            }
            Startup::Restore => restore_panes(cc, &settings, &home),
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
            #[cfg(target_os = "macos")]
            sync_profiles,
            #[cfg(target_os = "macos")]
            collections,
            #[cfg(target_os = "macos")]
            file_templates,
            #[cfg(target_os = "macos")]
            extensions,
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
            jobs: JobQueue::default(),
            notice: bookmark_shortcut_conflict.then(|| {
                (
                    crate::i18n::text!(
                        "Сочетание избранного очищено: клавиша занята другой командой",
                    )
                    .into(),
                    false,
                )
            }),
            show_help: false,
            history: History::new(&cc.egui_ctx),
            recovery: crate::recovery::Recovery::new(&cc.egui_ctx),
            #[cfg(any(target_os = "windows", test))]
            windows_drives: super::drives::WindowsDrives::default(),
            show_history: false,
            comparison: None,
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
            application_job: None,
            #[cfg(target_os = "macos")]
            system_open_jobs: Vec::new(),
            #[cfg(target_os = "macos")]
            system_open_error: None,
            #[cfg(target_os = "macos")]
            smb: super::smb_connections::Connections::load(&cc.egui_ctx),
            #[cfg(target_os = "macos")]
            volumes: Default::default(),
            #[cfg(target_os = "macos")]
            embedded_terminal: None,
            #[cfg(target_os = "macos")]
            remote_editor: Default::default(),
            tools: crate::tools::Tools::detect(),
            search,
        }
    }

    pub(super) fn open_workspace(&mut self, index: usize, ctx: &egui::Context) {
        let Some(workspace) = self.workspaces.get(index) else {
            return;
        };
        let Some(folders) = workspace.locations() else {
            self.notice = Some((
                crate::i18n::text!("Пути этого рабочего пространства несовместимы с системой.")
                    .into(),
                true,
            ));
            return;
        };
        self.settings.split = true;
        for (pane, path) in self.panes.iter_mut().zip(folders) {
            pane.tab_mut().navigate(path, ctx);
        }
    }

    pub(super) fn save_workspace_name(&mut self, editing: Option<usize>, name: String) {
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

    pub(super) fn workspace_name_taken(&self, name: &str, editing: Option<usize>) -> bool {
        let name = name.trim();
        self.workspaces
            .iter()
            .enumerate()
            .any(|(index, workspace)| Some(index) != editing && workspace.name == name)
    }
}

fn restore_panes(
    cc: &eframe::CreationContext<'_>,
    settings: &Settings,
    home: &Path,
) -> (Vec<Pane>, usize) {
    if let Some(session) = cc.storage.and_then(|storage| {
        eframe::get_value::<WorkspaceSession>(storage, PawCommander::SESSION_KEY)
    }) {
        let panes = session
            .panes
            .into_iter()
            .map(|pane| Pane::from_session(pane, home, &cc.egui_ctx))
            .collect();
        let active = if settings.split {
            session.active.min(1)
        } else {
            0
        };
        return (panes, active);
    }

    let panes = (0..2)
        .map(|i| {
            let Some(locations) = settings.locations.get(i).filter(|v| !v.is_empty()) else {
                return Pane::new(vec![Tab::new(home.to_owned().into(), &cc.egui_ctx)]);
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
}
