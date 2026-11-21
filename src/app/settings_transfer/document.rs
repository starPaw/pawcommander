use super::super::state::{PaneLayout, Settings};
use crate::{
    i18n::{text, Language},
    list_columns::ListColumns,
    shortcuts::Shortcuts,
    theme::Theme,
};
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Read, os::unix::fs::OpenOptionsExt, path::Path};

const FORMAT: &str = "pawcommander.settings";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 256 * 1024;

// Keep the transfer file independent from the persisted application profile.
// Locations, commands and connection records can contain private data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::app) struct Preferences {
    pub(super) language: Language,
    pub(super) theme: Theme,
    pub(super) ui_scale: u16,
    pub(super) high_contrast: bool,
    pub(super) split: bool,
    pub(super) pane_layout: PaneLayout,
    pub(super) preview: bool,
    pub(super) hidden: bool,
    pub(super) pane_grid: [bool; 2],
    pub(super) list_columns: ListColumns,
    pub(super) shortcuts: Shortcuts,
    pub(super) verify_copies: bool,
    pub(super) global_kib: u32,
}

#[derive(Deserialize)]
struct Header {
    format: String,
    version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(rename = "format")]
    _format: String,
    #[serde(rename = "version")]
    _version: u32,
    preferences: Preferences,
}

#[derive(Serialize)]
struct Export<'a> {
    format: &'static str,
    version: u32,
    preferences: &'a Preferences,
}

impl Preferences {
    pub(super) fn capture(settings: &Settings) -> Self {
        let mut pane_layout = settings.pane_layout.clone();
        for fraction in &mut pane_layout.fractions {
            *fraction = if fraction.is_finite() {
                fraction.clamp(0.0, 1.0)
            } else {
                0.5
            };
        }
        Self {
            language: settings.language,
            theme: settings.theme,
            ui_scale: settings.ui_scale,
            high_contrast: settings.high_contrast,
            split: settings.split,
            pane_layout,
            preview: settings.preview,
            hidden: settings.hidden,
            pane_grid: [settings.grid_for(0), settings.grid_for(1)],
            list_columns: settings.list_columns.clone(),
            shortcuts: settings.shortcuts.clone(),
            verify_copies: settings.verify_copies,
            global_kib: settings.transfer_limits.global_kib,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if !Settings::UI_SCALES.contains(&self.ui_scale)
            || self
                .pane_layout
                .fractions
                .iter()
                .any(|value| !(0.0..=1.0).contains(value))
        {
            return Err(text!("Недопустимый масштаб или размер панелей.").into());
        }
        let mut columns = self.list_columns.clone();
        columns.validate();
        if columns != self.list_columns {
            return Err(text!("Недопустимые размеры или порядок столбцов.").into());
        }
        let mut shortcuts = self.shortcuts.clone();
        shortcuts.validate();
        if shortcuts != self.shortcuts {
            return Err(
                text!("Файл содержит недопустимые или конфликтующие сочетания клавиш.").into(),
            );
        }
        Ok(())
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let invalid_file = || text!("Неверный формат файла настроек PawCommander.").to_owned();
        let header: Header = serde_json::from_slice(bytes).map_err(|_| invalid_file())?;
        if header.format != FORMAT {
            return Err(invalid_file());
        }
        if header.version != VERSION {
            return Err(crate::tr_format!(
                "Unsupported settings version {version}.",
                version = header.version
            ));
        }
        let document: Document = serde_json::from_slice(bytes).map_err(|_| invalid_file())?;
        document.preferences.validate()?;
        Ok(document.preferences)
    }

    pub(super) fn load(path: &Path) -> Result<Self, String> {
        let read_error = |error: std::io::Error| {
            crate::tr_format!("Could not read the settings file ({error}).", error = error)
        };
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)
            .map_err(read_error)?;
        let metadata = file.metadata().map_err(read_error)?;
        if !metadata.is_file() {
            return Err(text!("Выберите обычный файл настроек.").into());
        }
        if metadata.len() > MAX_BYTES {
            return Err(text!("Файл настроек превышает 256 КиБ.").into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(read_error)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(text!("Файл настроек превышает 256 КиБ.").into());
        }
        Self::decode(&bytes)
    }

    pub(super) fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let export_error = |error: std::io::Error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                text!("Файл уже существует. Выберите другое имя для экспорта.").to_owned()
            } else {
                crate::tr_format!("Could not save the settings file ({error}).", error = error)
            }
        };
        let mut contents = serde_json::to_vec_pretty(&Export {
            format: FORMAT,
            version: VERSION,
            preferences: self,
        })
        .map_err(|_| text!("Не удалось подготовить файл настроек."))?;
        contents.push(b'\n');
        super::super::export_file::write_new(path, &contents).map_err(export_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{Endpoint, Location};
    use eframe::egui::{Key, KeyboardShortcut, Modifiers};
    use serde_json::json;
    use std::{fs, path::PathBuf};

    fn sample_document() -> serde_json::Value {
        let prefs = Preferences::capture(&Settings::default());
        json!({ "format": FORMAT, "version": VERSION, "preferences": prefs })
    }

    #[test]
    fn portable_settings_write_only_allowed_data_and_preserve_an_existing_file() {
        let mut settings = Settings {
            theme: Theme::Mocha,
            ui_scale: 125,
            diff_tool: "/private/SECRET_COMMAND".into(),
            bookmarks: vec![PathBuf::from("/private/SECRET_BOOKMARK").into()],
            ..Settings::default()
        };
        let endpoint = Endpoint {
            host: "private-secret.example".into(),
            port: 22,
            user: "SECRET_USER".into(),
            key: Some("/private/SECRET_KEY".into()),
            jump: None,
        };
        settings.transfer_limits.global_kib = 900;
        settings.transfer_limits.set_connection_kib(&endpoint, 300);
        settings.sftp_profiles.push(Location::Sftp {
            endpoint,
            path: "/home".into(),
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let prefs = Preferences::capture(&settings);
        prefs.save(&path).unwrap();
        let encoded = fs::read_to_string(&path).unwrap();
        assert!(!encoded.contains("SECRET") && !encoded.contains("private-secret"));
        assert_eq!(Preferences::load(&path).unwrap(), prefs);
        assert!(Preferences::capture(&Settings::default())
            .save(&path)
            .is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), encoded);
        assert_eq!(dir.path().read_dir().unwrap().count(), 1);
    }

    #[test]
    fn settings_document_rejects_bad_headers_fields_and_values() {
        let base = sample_document();
        assert!(Preferences::decode(base.to_string().as_bytes()).is_ok());
        for (key, value) in [
            ("/format", json!("unrelated")),
            ("/version", json!(VERSION + 1)),
            ("/preferences/ui_scale", json!(999)),
            ("/preferences/pane_layout/fractions", json!([0.3, 1.2])),
            ("/preferences/list_columns/name_width", json!(20)),
            ("/preferences/theme", json!("Unknown")),
        ] {
            let mut wrong = base.clone();
            *wrong.pointer_mut(key).unwrap() = value;
            assert!(
                Preferences::decode(wrong.to_string().as_bytes()).is_err(),
                "{key}"
            );
        }
        let mut extra = base.clone();
        extra["preferences"]["sftp_profiles"] = json!([]);
        assert!(Preferences::decode(extra.to_string().as_bytes()).is_err());
        let mut bad_shortcut = base.clone();
        bad_shortcut["preferences"]["shortcuts"]["overrides"]["CommandPalette"] =
            json!(KeyboardShortcut::new(Modifiers::NONE, Key::Escape));
        assert!(Preferences::decode(bad_shortcut.to_string().as_bytes()).is_err());
        assert!(Preferences::decode(b"{").is_err());
    }

    #[test]
    fn settings_import_bounds_file_size_and_rejects_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("large.json");
        File::create(&file).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(Preferences::load(&file).is_err());
        assert!(Preferences::load(dir.path()).is_err());
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(Preferences::load(&link).is_err());
    }
}
