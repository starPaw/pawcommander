use super::super::state::{PaneLayout, Settings};
use crate::{
    file_error::{ExportProblem, ExportTarget, InvalidPreferences},
    i18n::Language,
    list_columns::ListColumns,
    operations::TransferError,
    shortcuts::Shortcuts,
    theme::Theme,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

const FORMAT: &str = "pawcommander.settings";
const VERSION: u32 = 1;
const MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug)]
pub(super) enum ReadError {
    Io(io::Error),
    NotRegular,
    TooLarge,
    Json(serde_json::Error),
    WrongFormat,
    Version(u32),
    InvalidPreferences(InvalidPreferences),
}

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

    fn validate(&self) -> Result<(), InvalidPreferences> {
        if !Settings::UI_SCALES.contains(&self.ui_scale)
            || self
                .pane_layout
                .fractions
                .iter()
                .any(|value| !(0.0..=1.0).contains(value))
        {
            return Err(InvalidPreferences::Layout);
        }
        let mut columns = self.list_columns.clone();
        columns.validate();
        if columns != self.list_columns {
            return Err(InvalidPreferences::Columns);
        }
        let mut shortcuts = self.shortcuts.clone();
        shortcuts.validate();
        if shortcuts != self.shortcuts {
            return Err(InvalidPreferences::Shortcuts);
        }
        Ok(())
    }

    fn decode(bytes: &[u8]) -> Result<Self, ReadError> {
        let header: Header = serde_json::from_slice(bytes).map_err(ReadError::Json)?;
        if header.format != FORMAT {
            return Err(ReadError::WrongFormat);
        }
        if header.version != VERSION {
            return Err(ReadError::Version(header.version));
        }
        let document: Document = serde_json::from_slice(bytes).map_err(ReadError::Json)?;
        document
            .preferences
            .validate()
            .map_err(ReadError::InvalidPreferences)?;
        Ok(document.preferences)
    }

    pub(super) fn load(path: &Path) -> Result<Self, ReadError> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)
            .map_err(ReadError::Io)?;
        let metadata = file.metadata().map_err(ReadError::Io)?;
        if !metadata.is_file() {
            return Err(ReadError::NotRegular);
        }
        if metadata.len() > MAX_BYTES {
            return Err(ReadError::TooLarge);
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(ReadError::Io)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(ReadError::TooLarge);
        }
        Self::decode(&bytes)
    }

    pub(super) fn save(&self, target: &ExportTarget) -> Result<(), TransferError> {
        self.validate()
            .map_err(|error| target.failure(ExportProblem::InvalidSettings(error)))?;
        let mut contents = serde_json::to_vec_pretty(&Export {
            format: FORMAT,
            version: VERSION,
            preferences: self,
        })
        .map_err(|error| target.failure(ExportProblem::EncodeSettings(error)))?;
        contents.push(b'\n');
        super::super::export_file::write_new(target, &contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::{ExportIoAction, ExportKind, Failure};
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
        let target = ExportTarget {
            kind: ExportKind::Settings,
            path: dir.path().join("settings.json"),
        };
        let prefs = Preferences::capture(&settings);
        prefs.save(&target).unwrap();
        let encoded = fs::read_to_string(&target.path).unwrap();
        assert!(!encoded.contains("SECRET") && !encoded.contains("private-secret"));
        assert_eq!(Preferences::load(&target.path).unwrap(), prefs);
        let error = Preferences::capture(&Settings::default())
            .save(&target)
            .unwrap_err();
        let TransferError::Failed(Failure::Export(error)) = error else {
            panic!("expected an export error");
        };
        assert_eq!(error.target.path, target.path);
        assert!(matches!(error.problem, ExportProblem::Io {
            action: ExportIoAction::Publish, source,
        } if source.raw_os_error() == Some(libc::EEXIST)));
        assert_eq!(fs::read_to_string(&target.path).unwrap(), encoded);
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
        assert!(matches!(Preferences::decode(b"{\n  \"format\":"),
            Err(ReadError::Json(error)) if error.is_eof() && error.line() == 2 && error.column() > 1
        ));
        let mut unsupported = base;
        unsupported["version"] = json!(99);
        assert!(matches!(
            Preferences::decode(unsupported.to_string().as_bytes()),
            Err(ReadError::Version(99))
        ));
    }

    #[test]
    fn settings_import_bounds_file_size_and_rejects_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("large.json");
        File::create(&file).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(matches!(Preferences::load(&file), Err(ReadError::TooLarge)));
        assert!(matches!(
            Preferences::load(dir.path()),
            Err(ReadError::NotRegular)
        ));
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(matches!(Preferences::load(&link), Err(ReadError::Io(error))
            if error.raw_os_error() == Some(libc::ELOOP)));
        assert!(
            matches!(Preferences::load(&dir.path().join("missing.json")), Err(ReadError::Io(error))
            if error.raw_os_error() == Some(libc::ENOENT))
        );
    }
}
