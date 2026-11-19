use super::bookmarks::{
    reserve_bookmark_ids, validate_bookmark_shortcuts, Bookmark, SavedBookmark,
};
use super::directory::FolderVisit;
use super::state::{NamedWorkspace, SavedFolderView, SavedLocation, Settings, WorkspaceSession};
use super::tabs::{Pane, Tab};
use super::PawCommander;
use crate::directory_tree::DirectoryTree;
use crate::file_clipboard::SystemClipboard;
use crate::preview::Preview;
use crate::result_history::History;
use crate::thumbnails::Thumbnails;
use eframe::egui;
use std::collections::VecDeque;
use std::path::PathBuf;

impl PawCommander {
    pub(super) const SETTINGS_KEY: &str = "pawcommander.settings";

    pub(super) const SESSION_KEY: &str = "pawcommander.session";

    pub(super) const WORKSPACES_KEY: &str = "pawcommander.workspaces";

    pub(super) const BOOKMARKS_KEY: &str = "pawcommander.bookmarks";

    pub(super) const FOLDER_VIEWS_KEY: &str = "pawcommander.folder_views";

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
            // Both panes share one launch visit. Whichever listing first
            // succeeds records it, even if the other pane navigated away.
            let visit = FolderVisit::SharedOpen(Default::default());
            let mut left = Tab::new(path.clone().into(), &cc.egui_ctx);
            left.folder_visit = visit.clone();
            let mut right = Tab::new(path.into(), &cc.egui_ctx);
            right.folder_visit = visit;
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
            retry_jobs: VecDeque::new(),
            undo_jobs: VecDeque::new(),
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
            editor_job: None,
            tools: crate::tools::Tools::detect(),
            search: crate::search::Search::load(cc.storage),
        }
    }

    pub(super) fn open_workspace(&mut self, index: usize, ctx: &egui::Context) {
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
