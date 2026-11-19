use super::{validate_folder_query, validate_index_query, FilterInput, FolderTarget, Mode, Search};
#[cfg(test)]
use crate::test_ui::ContextExt as _;
use crate::{
    app::{SavedLocation, SavedPath},
    i18n::text,
    vfs::Location,
};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::Path;

const STORAGE_KEY: &str = "pawcommander.saved_searches";

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SavedSearch {
    name: String,
    query: String,
    scope: Scope,
}

#[derive(Clone, Serialize, Deserialize)]
enum Scope {
    Folder(Box<FolderScope>),
    Sftp(Box<SftpScope>),
    Locate,
}

#[derive(Clone, Serialize, Deserialize)]
struct FolderScope {
    path: SavedPath,
    target: FolderTarget,
    #[serde(default)]
    exclusions: String,
    #[serde(default)]
    filters: FilterInput,
}

#[derive(Clone, Serialize, Deserialize)]
struct SftpScope {
    location: SavedLocation,
    seconds: u64,
    exclusions: String,
    filters: FilterInput,
}

impl Scope {
    fn mode(&self) -> Mode {
        match self {
            Self::Folder(_) | Self::Sftp(_) => Mode::Folder,
            Self::Locate => Mode::Index,
        }
    }

    fn label(&self) -> String {
        let folder = match self {
            Self::Folder(folder) => folder.path.clone().into_path().map(Location::Local),
            Self::Sftp(folder) => folder.location.clone().into_location(),
            Self::Locate => return text("Индекс locate").into(),
        };
        if let Some(folder) = folder {
            let path = folder.display();
            crate::tr_format!("Папка поиска: {path}", "Search folder: {path}")
        } else {
            text("Путь сохранённого поиска несовместим с системой.").into()
        }
    }
}

enum EditTarget {
    New(SavedSearch),
    Rename(usize),
}

pub(super) struct Editor {
    target: EditTarget,
    name: String,
    focus: bool,
}

enum Action {
    Run(usize),
    Rename(usize),
    Remove(usize),
}

impl Search {
    pub(crate) fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        Self {
            saved: storage
                .and_then(|storage| eframe::get_value(storage, STORAGE_KEY))
                .unwrap_or_default(),
            ..Self::default()
        }
    }

    pub(crate) fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_KEY, &self.saved);
    }

    fn saved_candidate(&self) -> Result<SavedSearch, String> {
        let scope = match self.mode {
            Mode::Folder => {
                let folder = self.folder.as_ref().ok_or_else(|| {
                    text("Откройте локальную папку или папку SFTP для поиска.").to_owned()
                })?;
                let target = self.target();
                let filters = self.filters.parse(target)?;
                validate_folder_query(target, &self.query, &self.exclusions, &filters)?;
                if folder.remote() {
                    Scope::Sftp(Box::new(SftpScope {
                        location: folder.into(),
                        seconds: self.remote_time_limit(),
                        exclusions: self.exclusions.clone(),
                        filters: self.filters.clone(),
                    }))
                } else {
                    Scope::Folder(Box::new(FolderScope {
                        path: folder.path().into(),
                        target,
                        exclusions: self.exclusions.clone(),
                        filters: self.filters.clone(),
                    }))
                }
            }
            Mode::Index => {
                validate_index_query(&self.query)?;
                Scope::Locate
            }
        };
        Ok(SavedSearch {
            name: String::new(),
            query: self.query.clone(),
            scope,
        })
    }

    fn run_saved(&mut self, index: usize, cli: Option<&Path>, ctx: &egui::Context) {
        let Some(saved) = self.saved.get(index) else {
            return;
        };
        let mode = saved.scope.mode();
        if self.jobs[mode as usize].is_some() {
            return;
        }
        let saved = saved.clone();
        self.clear();
        self.mode = mode;
        self.query = saved.query;
        match saved.scope {
            Scope::Folder(folder) => {
                self.folder = folder.path.into_path().map(Location::Local);
                self.folder_target = folder.target;
                self.exclusions = folder.exclusions;
                self.filters = folder.filters;
                if self.folder.is_none() {
                    self.error =
                        Some(text("Путь сохранённого поиска несовместим с системой.").into());
                    return;
                }
            }
            Scope::Sftp(folder) => {
                self.folder = folder.location.into_location().filter(Location::remote);
                self.remote_seconds = Some(folder.seconds);
                self.exclusions = folder.exclusions;
                self.filters = folder.filters;
                if self.folder.is_none() {
                    self.error =
                        Some(text("Путь сохранённого поиска несовместим с системой.").into());
                    return;
                }
            }
            Scope::Locate => {
                if cli.is_none() {
                    self.error = Some(text("locate не найден").into());
                    return;
                }
            }
        }
        self.start(cli, ctx);
    }

    fn show_saved_editor(&mut self, ui: &mut egui::Ui) {
        let Some(editor) = self.saved_edit.as_mut() else {
            return;
        };
        let edit_index = match editor.target {
            EditTarget::New(_) => None,
            EditTarget::Rename(index) => Some(index),
        };
        let mut submit = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            let label = text(if edit_index.is_some() {
                "Переименовать поиск"
            } else {
                "Название поиска"
            });
            let response = ui.add(
                egui::TextEdit::singleline(&mut editor.name)
                    .id_salt("saved_search_name")
                    .hint_text(label)
                    .desired_width((ui.available_width() - 170.0).max(70.0)),
            );
            ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_label(label);
            });
            if editor.focus {
                response.request_focus();
                editor.focus = false;
            }
            let name = editor.name.trim();
            let taken = self
                .saved
                .iter()
                .enumerate()
                .any(|(index, saved)| Some(index) != edit_index && saved.name == name);
            let valid = !name.is_empty() && !taken;
            let save = ui.add_enabled(valid, egui::Button::new(text("Сохранить")));
            if taken {
                save.on_disabled_hover_text(text("Поиск с таким названием уже сохранён."));
            } else {
                submit = save.clicked()
                    || (valid
                        && response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter)));
            }
            cancel = ui.button(text("Отмена")).clicked();
        });
        if submit {
            let editor = self.saved_edit.take().expect("visible saved search editor");
            let name = editor.name.trim().to_owned();
            match editor.target {
                EditTarget::New(mut saved) => {
                    saved.name = name;
                    self.saved.push(saved);
                }
                EditTarget::Rename(index) => self.saved[index].name = name,
            }
        } else if cancel {
            self.saved_edit = None;
        }
    }

    pub(super) fn show_saved(&mut self, ui: &mut egui::Ui, cli: Option<&Path>) {
        if self.saved_edit.is_none() && ui.button(text("Сохранить поиск…")).clicked()
        {
            match self.saved_candidate() {
                Ok(saved) => {
                    self.saved_edit = Some(Editor {
                        target: EditTarget::New(saved),
                        name: String::new(),
                        focus: true,
                    });
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
        }
        if self.saved_edit.is_some() || self.saved.is_empty() {
            self.close_saved_popup(ui.ctx());
        }
        self.show_saved_editor(ui);
        if self.saved.is_empty() {
            return;
        }
        let mut action = None;
        let header = egui::CollapsingHeader::new(text("Сохранённые поиски"))
            .id_salt("saved_searches")
            .default_open(true)
            .show(ui, |ui| {
                ui.add_enabled_ui(self.saved_edit.is_none(), |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("saved_search_list")
                        .max_height(160.0)
                        .show(ui, |ui| {
                            for (index, saved) in self.saved.iter().enumerate() {
                                ui.push_id(index, |ui| {
                                    let can_run = self.jobs[saved.scope.mode() as usize].is_none();
                                    if let Some(row_action) =
                                        saved.show_row(ui, index, can_run, &mut self.saved_popup)
                                    {
                                        action = Some(row_action);
                                    }
                                });
                            }
                        });
                });
            });
        if header.header_response.clicked() || header.body_returned.is_none() {
            self.close_saved_popup(ui.ctx());
        }
        match action {
            Some(Action::Run(index)) => self.run_saved(index, cli, ui.ctx()),
            Some(Action::Rename(index)) => {
                self.saved_edit = Some(Editor {
                    target: EditTarget::Rename(index),
                    name: self.saved[index].name.clone(),
                    focus: true,
                });
            }
            Some(Action::Remove(index)) => {
                self.saved.remove(index);
            }
            None => {}
        }
    }

    pub(super) fn close_saved_popup(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.saved_popup.take() {
            egui::Popup::close_id(ctx, id);
        }
    }
}

impl SavedSearch {
    fn show_row(
        &self,
        ui: &mut egui::Ui,
        index: usize,
        can_run: bool,
        saved_popup: &mut Option<egui::Id>,
    ) -> Option<Action> {
        let mut action = None;
        ui.horizontal(|ui| {
            let name = &self.name;
            let run_label = crate::tr_format!("Запустить поиск «{name}»", "Run search “{name}”");
            let run = ui
                .add_enabled_ui(can_run, |ui| {
                    ui.add_sized(
                        [(ui.available_width() - 34.0).max(0.0), 25.0],
                        egui::Button::new(name).truncate(),
                    )
                })
                .inner
                .on_hover_text(&run_label);
            ui.ctx().accesskit_node_builder(run.id, |node| {
                node.set_label(run_label);
            });
            if run.clicked() {
                action = Some(Action::Run(index));
            }
            let menu = ui.button("…");
            let menu_label = crate::tr_format!("Действия для «{name}»", "Actions for {name}");
            ui.ctx().accesskit_node_builder(menu.id, |node| {
                node.set_label(menu_label.clone());
            });
            let menu = menu.on_hover_text(menu_label);
            let popup_id = menu.id.with("saved_search_actions");
            let menu_action = egui::Popup::from_toggle_button_response(&menu)
                .id(popup_id)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_min_width(170.0);
                    if ui.button(text("Переименовать…")).clicked() {
                        return Some(Action::Rename(index));
                    }
                    if ui.button(text("Удалить сохранённый поиск")).clicked()
                    {
                        return Some(Action::Remove(index));
                    }
                    None
                })
                .and_then(|response| response.inner);
            if let Some(menu_action) = menu_action {
                egui::Popup::close_id(ui.ctx(), popup_id);
                action = Some(menu_action);
            }
            if egui::Popup::is_id_open(ui.ctx(), popup_id) {
                *saved_popup = Some(popup_id);
            } else if *saved_popup == Some(popup_id) {
                *saved_popup = None;
            }
        });
        let scope = self.scope.label();
        ui.add(egui::Label::new(egui::RichText::new(&scope).small()).truncate())
            .on_hover_text(scope);
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, Instant},
    };

    #[derive(Default)]
    struct MemoryStorage(Option<String>);

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            assert_eq!(key, STORAGE_KEY);
            self.0.clone()
        }

        fn set_string(&mut self, key: &str, value: String) {
            assert_eq!(key, STORAGE_KEY);
            self.0 = Some(value);
        }

        fn remove_string(&mut self, key: &str) {
            assert_eq!(key, STORAGE_KEY);
            self.0 = None;
        }

        fn flush(&mut self) {}
    }

    fn save_candidate(search: &mut Search, name: &str) {
        let mut saved = search.saved_candidate().unwrap();
        saved.name = name.into();
        search.saved.push(saved);
    }

    fn receive_paths(search: &mut Search) -> Vec<PathBuf> {
        assert!(search.job().is_some(), "fresh search worker");
        let deadline = Instant::now() + Duration::from_secs(5);
        while search.job().is_some() {
            search.poll();
            assert!(Instant::now() < deadline, "search did not complete");
            std::thread::sleep(Duration::from_millis(1));
        }
        search
            .results
            .as_ref()
            .unwrap_or_else(|| panic!("search failed: {:?}", search.error))
            .matches
            .iter()
            .map(|hit| hit.path.path().to_path_buf())
            .collect()
    }

    #[test]
    fn saved_sftp_search_restores_its_server_folder_and_time_limit() {
        let ctx = egui::Context::default();
        let folder = Location::Sftp {
            endpoint: crate::vfs::Endpoint {
                host: "127.0.0.1".into(),
                port: 0,
                user: "fixture".into(),
                key: Some("/keys/explicit-key".into()),
            },
            path: "/reports".into(),
        };
        let mut search = Search {
            folder_target: FolderTarget::Regex,
            ..Default::default()
        };
        search.open(Some(&folder));
        search.query = "*.txt".into();
        search.exclusions = ".git".into();
        search.remote_seconds = Some(7);
        save_candidate(&mut search, "Reports");
        let mut storage = MemoryStorage::default();
        search.save(&mut storage);

        let mut reopened = Search::load(Some(&storage));
        reopened.run_saved(0, None, &ctx);
        assert_eq!(reopened.folder.as_ref(), Some(&folder));
        assert_eq!(reopened.remote_time_limit(), 7);
        assert!(reopened.target() == FolderTarget::Name);
        assert_eq!(reopened.exclusions, ".git");
        assert!(reopened.job().is_some());

        let local = Location::Local("/local".into());
        search.open(Some(&local));
        assert!(search.target() == FolderTarget::Regex);
    }

    #[test]
    fn saved_folder_search_restores_exact_criteria_and_reruns_against_changed_files() {
        let workspace = tempfile::tempdir().unwrap();
        let saved_folder = workspace.path().join("saved-folder");
        let other_folder = workspace.path().join("current-pane");
        fs::create_dir(&saved_folder).unwrap();
        fs::create_dir(&other_folder).unwrap();
        fs::create_dir(saved_folder.join("nested")).unwrap();
        let old_file = saved_folder.join("old.txt");
        let new_file = saved_folder.join("new.txt");
        for (path, content) in [
            (&old_file, "TODO: 10"),
            (&saved_folder.join("hidden.skip.txt"), "TODO: 11"),
            (&saved_folder.join("wrong-type.rs"), "TODO: 12"),
            (&saved_folder.join("nested/deep.txt"), "TODO: 13"),
            (&saved_folder.join("too-small.txt"), "TODO: 1"),
            (&saved_folder.join("wrong-text.txt"), "todo: 123"),
            (&other_folder.join("wrong-folder.txt"), "TODO: 14"),
        ] {
            fs::write(path, content).unwrap();
        }

        let ctx = egui::Context::default();
        let query = r"TODO: [0-9]+";
        let mut search = Search::default();
        search.open(Some(&Location::Local(saved_folder.clone())));
        search.folder_target = FolderTarget::Regex;
        search.query = query.into();
        search.exclusions = "*.skip.txt".into();
        search.filters = serde_json::from_value(serde_json::json!({
            "extension": "txt",
            "minimum_size": "8",
            "maximum_size": "8",
            "depth": "1"
        }))
        .unwrap();
        save_candidate(&mut search, "Daily TODOs");
        search.run_saved(0, None, &ctx);
        assert_eq!(
            receive_paths(&mut search).as_slice(),
            std::slice::from_ref(&old_file)
        );

        let mut storage = MemoryStorage::default();
        search.save(&mut storage);
        fs::remove_file(&old_file).unwrap();
        fs::write(&new_file, "TODO: 15").unwrap();
        let mut reopened = Search::load(Some(&storage));
        reopened.open(Some(&Location::Local(other_folder)));
        assert_eq!(reopened.saved[0].name, "Daily TODOs");
        assert!(reopened.results.is_none());
        reopened.run_saved(0, None, &ctx);
        assert_eq!(
            reopened.folder.as_ref().and_then(Location::local),
            Some(saved_folder.as_path())
        );
        assert!(reopened.folder_target == FolderTarget::Regex);
        assert_eq!(reopened.query, query);
        assert_eq!(reopened.exclusions, "*.skip.txt");
        assert_eq!(receive_paths(&mut reopened), [new_file]);
    }

    #[test]
    fn saved_scope_preserves_native_paths_and_missing_or_invalid_storage_is_empty() {
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
        let folder = tempfile::tempdir().unwrap().path().join(native);
        assert!(Search::load(None).saved.is_empty());
        let mut storage = MemoryStorage::default();
        assert!(Search::load(Some(&storage)).saved.is_empty());
        storage.0 = Some("invalid".into());
        assert!(Search::load(Some(&storage)).saved.is_empty());

        let mut search = Search::default();
        search.open(Some(&Location::Local(folder.clone())));
        search.query = "report".into();
        save_candidate(&mut search, "Exact path");
        search.save(&mut storage);
        let reopened = Search::load(Some(&storage));
        let Scope::Folder(saved_folder) = &reopened.saved[0].scope else {
            panic!("expected a saved folder search");
        };
        assert_eq!(saved_folder.path.clone().into_path(), Some(folder));
    }

    #[test]
    fn saved_locate_uses_index_scope_and_checks_current_tool_availability() {
        let ctx = egui::Context::default();
        let mut search = Search {
            mode: Mode::Index,
            query: "Cargo".into(),
            ..Search::default()
        };
        save_candidate(&mut search, "Cargo paths");
        let mut storage = MemoryStorage::default();
        search.save(&mut storage);
        let mut reopened = Search::load(Some(&storage));
        assert!(matches!(reopened.saved[0].scope, Scope::Locate));
        reopened.run_saved(0, None, &ctx);
        assert!(reopened.mode == Mode::Index);
        assert_eq!(reopened.query, "Cargo");
        assert_eq!(reopened.error.as_deref(), Some(text("locate не найден")));
        assert!(reopened.job().is_none());
    }

    #[test]
    fn folder_search_rejects_invalid_filters_before_saving() {
        let mut search = Search::default();
        search.open(Some(&Location::Local(Path::new("/").into())));
        search.query = "report".into();
        search.filters = serde_json::from_value(serde_json::json!({
            "minimum_size": "9",
            "maximum_size": "8"
        }))
        .unwrap();
        assert!(search.saved_candidate().is_err());
    }

    #[test]
    fn escape_closes_saved_search_actions_before_the_search_window() {
        use egui::accesskit::{Action, ActionRequest};

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        ctx.global_style_mut(|style| style.animation_time = 0.0);
        let mut search = Search {
            visible: true,
            mode: Mode::Index,
            query: "report".into(),
            ..Default::default()
        };
        let name = "Reports";
        save_candidate(&mut search, name);
        let draw = |search: &mut Search, events| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            ctx.run_test_ui(input, |root_ui| {
                let ctx = &root_ui.ctx().clone();
                search.show(ctx, None);
            })
        };
        let _ = draw(&mut search, vec![]);
        let label = crate::tr_format!("Действия для «{name}»", "Actions for {name}");
        let click = |output: egui::FullOutput, label: &str| {
            let (target, _) = output
                .platform_output
                .accesskit_update
                .unwrap()
                .nodes
                .into_iter()
                .find(|(_, node)| {
                    node.label() == Some(label) && node.supports_action(Action::Click)
                })
                .expect("accessible saved search control");
            vec![egui::Event::AccessKitActionRequest(ActionRequest {
                action: Action::Click,
                target_node: target,
                target_tree: egui::accesskit::TreeId::ROOT,
                data: None,
            })]
        };
        let open = click(draw(&mut search, vec![]), &label);
        let _ = draw(&mut search, open);
        assert!(egui::Popup::is_any_open(&ctx));

        let escape = |pressed| egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let _ = draw(&mut search, vec![escape(true), escape(false)]);
        assert!(!egui::Popup::is_any_open(&ctx));
        assert!(search.visible);
        let open = click(draw(&mut search, vec![]), &label);
        let popup = draw(&mut search, open);
        assert!(egui::Popup::is_any_open(&ctx));
        let collapse = click(popup, text("Сохранённые поиски"));
        let _ = draw(&mut search, collapse);
        assert!(!egui::Popup::is_any_open(&ctx));
        let _ = draw(&mut search, vec![escape(true)]);
        assert!(!search.visible);
    }
}
