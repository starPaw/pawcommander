use super::SavedLocation;
use crate::{
    i18n::text,
    name_match::normalize,
    vfs::{Exclusions, Location},
};
use eframe::egui;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum Direction {
    #[default]
    LeftToRight,
    RightToLeft,
    Both,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct Options {
    pub(super) direction: Direction,
    pub(super) delete_extra: bool,
    pub(super) exclusions: String,
}

#[derive(Deserialize, Serialize)]
pub(super) struct SyncProfile {
    name: String,
    folders: [SavedLocation; 2],
    pub(super) options: Options,
}

impl SyncProfile {
    pub(super) fn folders(&self) -> Option<[Location; 2]> {
        let [left, right] = self.folders.clone().map(SavedLocation::into_location);
        let folders = [left?, right?];
        folders
            .iter()
            .all(|folder| folder.path().is_absolute() && !folder.is_archive())
            .then_some(folders)
    }
}

enum Edit {
    Save(String),
    Remove,
}

pub(super) struct ProfilePicker {
    current: Option<[Location; 2]>,
    selected: Option<usize>,
    list_open: bool,
    edit: Option<Edit>,
    error: Option<String>,
}

impl ProfilePicker {
    pub(super) fn new(current: Option<[Location; 2]>, selected: Option<usize>) -> Self {
        Self {
            current,
            selected,
            list_open: false,
            edit: None,
            error: None,
        }
    }

    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        profiles: &mut Vec<SyncProfile>,
        folders: &mut [Location; 2],
        options: &mut Options,
    ) -> bool {
        let mut pair_changed = false;
        let name = self.selected.map(|index| profiles[index].name.as_str());
        ui.horizontal_wrapped(|ui| {
            ui.label(text("Профиль"));
            let selector = format!("{} ▾", name.unwrap_or_else(|| text("Без профиля")));
            if ui
                .add(egui::Button::new(selector).selected(self.list_open))
                .clicked()
            {
                self.list_open = !self.list_open;
                self.edit = None;
                self.error = None;
            }
            let save_label = if name.is_some() {
                "Обновить профиль…"
            } else {
                "Сохранить профиль…"
            };
            if ui.button(text(save_label)).clicked() {
                self.list_open = false;
                self.edit = Some(Edit::Save(name.unwrap_or_default().to_owned()));
                self.error = None;
            }
            if let Some(current) = self.current.as_ref()
                && current != folders
                && ui.button(text("Взять папки из панелей")).clicked()
            {
                *folders = current.clone();
                self.list_open = false;
                self.edit = None;
                self.error = None;
                pair_changed = true;
            }
            if name.is_some() && ui.button(text("Удалить профиль…")).clicked() {
                self.list_open = false;
                self.edit = Some(Edit::Remove);
                self.error = None;
            }
        });

        if self.list_open
            && let Some(selected) = self.show_choices(ui, profiles)
        {
            self.list_open = false;
            self.edit = None;
            self.error = None;
            let pair =
                selected.map_or_else(|| self.current.clone(), |index| profiles[index].folders());
            if let Some(pair) = pair {
                self.selected = selected;
                *folders = pair;
                *options = selected
                    .map(|index| profiles[index].options.clone())
                    .unwrap_or_default();
                pair_changed = true;
            } else {
                self.error = Some(text("Папки профиля несовместимы с этой системой.").to_owned());
            }
        }

        let mut save = false;
        let mut remove = false;
        let mut cancel = false;
        match &mut self.edit {
            Some(Edit::Save(name)) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(text("Название профиля"));
                    ui.add(egui::TextEdit::singleline(name).char_limit(80));
                    save = ui.button(text("Сохранить")).clicked();
                    cancel = ui.button(text("Отмена")).clicked();
                });
            }
            Some(Edit::Remove) => {
                let name = name.unwrap_or_default();
                ui.horizontal_wrapped(|ui| {
                    ui.label(crate::tr_format!(
                        "Удалить профиль «{name}»?",
                        "Remove profile “{name}”?"
                    ));
                    remove = ui.button(text("Удалить")).clicked();
                    cancel = ui.button(text("Отмена")).clicked();
                });
            }
            None => {}
        }
        if save && let Some(Edit::Save(name)) = &self.edit {
            match save_profile(profiles, self.selected, name, folders, options) {
                Ok(index) => {
                    self.selected = Some(index);
                    self.edit = None;
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
        }
        if remove && let Some(index) = self.selected.take() {
            profiles.remove(index);
        }
        if cancel || remove {
            self.edit = None;
            self.error = None;
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        pair_changed
    }

    fn show_choices(&self, ui: &mut egui::Ui, profiles: &[SyncProfile]) -> Option<Option<usize>> {
        let mut pick = None;
        egui::ScrollArea::vertical()
            .id_salt("sync-profile-list")
            .max_height(140.0)
            .show(ui, |ui| {
                if ui
                    .add_enabled(
                        self.current.is_some(),
                        egui::Button::selectable(
                            self.selected.is_none(),
                            text("Текущая пара папок"),
                        ),
                    )
                    .clicked()
                {
                    pick = Some(None);
                }
                for (index, profile) in profiles.iter().enumerate() {
                    if ui
                        .selectable_label(self.selected == Some(index), &profile.name)
                        .clicked()
                    {
                        pick = Some(Some(index));
                    }
                }
            });
        pick
    }
}

fn save_profile(
    profiles: &mut Vec<SyncProfile>,
    editing: Option<usize>,
    name: &str,
    folders: &[Location; 2],
    options: &Options,
) -> Result<usize, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(text("Введите название профиля.").into());
    }
    let key = normalize(name);
    if profiles
        .iter()
        .enumerate()
        .any(|(index, other)| Some(index) != editing && normalize(&other.name) == key)
    {
        return Err(text("Профиль с таким названием уже существует.").into());
    }
    if folders[0] == folders[1] {
        return Err(text("Выберите две разные папки для профиля.").into());
    }
    Exclusions::parse(&options.exclusions).map_err(|error| error.to_string())?;
    let profile = SyncProfile {
        name: name.to_owned(),
        folders: std::array::from_fn(|side| SavedLocation::from(&folders[side])),
        options: options.clone(),
    };
    if let Some(index) = editing {
        profiles[index] = profile;
        return Ok(index);
    }
    let index = profiles.len();
    profiles.push(profile);
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Endpoint;
    use std::path::PathBuf;

    #[test]
    fn saved_profile_round_trips_the_pair_and_options_and_rejects_duplicates() {
        let pair = [
            Location::Local(PathBuf::from("/tmp/local pair")),
            Location::Sftp {
                endpoint: Endpoint {
                    host: "profile.example.test".into(),
                    port: 2203,
                    user: "sync-user".into(),
                    key: Some("/tmp/test-key".into()),
                },
                path: "/backup/shared files".into(),
            },
        ];
        let mut profiles = Vec::new();
        let mut options = Options {
            direction: Direction::RightToLeft,
            delete_extra: true,
            exclusions: "*.tmp\nbuild/cache".into(),
        };
        assert_eq!(
            save_profile(&mut profiles, None, "  Daily sync  ", &pair, &options).unwrap(),
            0
        );
        assert!(save_profile(&mut profiles, None, "DAILY SYNC", &pair, &options).is_err());
        let saved = ron::to_string(&profiles).unwrap();
        let mut restored: Vec<SyncProfile> = ron::from_str(&saved).unwrap();
        assert_eq!(restored[0].name, "Daily sync");
        assert!(restored[0].folders().as_ref() == Some(&pair));
        assert!(restored[0].options.direction == Direction::RightToLeft);
        assert!(restored[0].options.delete_extra);
        assert_eq!(restored[0].options.exclusions, options.exclusions);
        options.exclusions = "../whole-disk".into();
        assert!(save_profile(&mut restored, Some(0), "Renamed", &pair, &options).is_err());
        assert_eq!(restored[0].name, "Daily sync");
    }
}
