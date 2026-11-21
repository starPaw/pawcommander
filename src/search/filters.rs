use super::{
    directory::{EntryKind, EntryMetadata},
    FolderTarget, MAX_OPEN_DIRECTORIES, MAX_PATTERN_BYTES,
};
use crate::{i18n::text, name_match::normalize};
use chrono::{Datelike, Local, NaiveDate};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{ffi::OsStr, io, path::Path};

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum EntryType {
    #[default]
    Any,
    File,
    Directory,
    Other,
}

impl EntryType {
    fn label(self) -> &'static str {
        match self {
            Self::Any => text!("Любой тип"),
            Self::File => text!("Обычные файлы"),
            Self::Directory => text!("Только папки"),
            Self::Other => text!("Ссылки и прочие"),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum SizeUnit {
    #[default]
    Bytes,
    KiB,
    MiB,
    GiB,
}

impl SizeUnit {
    fn label(self) -> &'static str {
        match self {
            Self::Bytes => text!("Б"),
            Self::KiB => text!("КиБ"),
            Self::MiB => text!("МиБ"),
            Self::GiB => text!("ГиБ"),
        }
    }

    fn bytes(self) -> u64 {
        match self {
            Self::Bytes => 1,
            Self::KiB => 1024,
            Self::MiB => 1024 * 1024,
            Self::GiB => 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct FilterInput {
    entry_type: EntryType,
    extension: String,
    minimum_size: String,
    maximum_size: String,
    size_unit: SizeUnit,
    modified_from: String,
    modified_through: String,
    depth: String,
}

impl FilterInput {
    pub fn active(&self, target: FolderTarget) -> bool {
        (target == FolderTarget::Name && self.entry_type != EntryType::Any)
            || [
                &self.extension,
                &self.minimum_size,
                &self.maximum_size,
                &self.modified_from,
                &self.modified_through,
                &self.depth,
            ]
            .iter()
            .any(|input| !input.trim().is_empty())
    }

    pub fn parse(&self, target: FolderTarget) -> Result<Filters, String> {
        let extension = self.extension.trim();
        let extension = extension.strip_prefix('.').unwrap_or(extension);
        if (!self.extension.trim().is_empty() && extension.is_empty())
            || extension.len() > MAX_PATTERN_BYTES
            || extension
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '.' | '/' | '\\' | '*' | '?'))
        {
            return Err(crate::tr_format!(
                "Введите одно расширение без маски, например txt.",
                "Enter one extension without a wildcard, such as txt."
            ));
        }
        let extension = (!extension.is_empty()).then(|| normalize(extension).into_owned());
        let size = Bounds::new(
            parse_size(&self.minimum_size, self.size_unit)?,
            parse_size(&self.maximum_size, self.size_unit)?,
        )
        .ok_or_else(|| {
            crate::tr_format!(
                "Минимальный размер должен быть не больше максимального.",
                "Minimum size must be at most the maximum size."
            )
        })?;
        let modified = Bounds::new(
            parse_date(&self.modified_from)?,
            parse_date(&self.modified_through)?,
        )
        .ok_or_else(|| {
            crate::tr_format!(
                "Начальная дата должна быть не позже конечной.",
                "The starting date must be on or before the ending date."
            )
        })?;
        let depth = parse_number(&self.depth, |depth| {
            (1..=MAX_OPEN_DIRECTORIES as u64)
                .contains(&depth)
                .then_some(depth as usize)
        })
        .map_err(|()| {
            crate::tr_format!(
                "Введите максимальную глубину от 1 до {MAX_OPEN_DIRECTORIES}.",
                "Enter a maximum depth from 1 to {MAX_OPEN_DIRECTORIES}."
            )
        })?;

        let mut entry_type = self.entry_type;
        if target != FolderTarget::Name {
            entry_type = EntryType::File;
        } else if extension.is_some() || size.active() {
            if !matches!(entry_type, EntryType::Any | EntryType::File) {
                return Err(crate::tr_format!(
                    "Для фильтра по размеру или расширению выберите обычные файлы или любой тип.",
                    "Choose Regular files or Any type when filtering by size or extension."
                ));
            }
            entry_type = EntryType::File;
        }
        Ok(Filters {
            entry_type,
            extension,
            size,
            modified,
            depth,
        })
    }

    pub fn show(&mut self, ui: &mut egui::Ui, target: FolderTarget) -> (bool, bool) {
        let active = self.active(target);
        let heading = if active {
            text!("Фильтры (активны)")
        } else {
            text!("Фильтры")
        };
        egui::CollapsingHeader::new(heading)
            .id_salt("local_search_filters")
            .show(ui, |ui| {
                let mut changed = false;
                let mut submit = false;
                let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
                egui::Grid::new("local_search_filter_fields")
                    .num_columns(2)
                    .show(ui, |ui| {
                        let type_label = text!("Тип объекта");
                        ui.label(type_label);
                        if target == FolderTarget::Name {
                            let selected = self.entry_type;
                            let response = egui::ComboBox::from_id_salt("search_entry_type")
                                .selected_text(selected.label())
                                .show_ui(ui, |ui| {
                                    for entry_type in [
                                        EntryType::Any,
                                        EntryType::File,
                                        EntryType::Directory,
                                        EntryType::Other,
                                    ] {
                                        ui.selectable_value(
                                            &mut self.entry_type,
                                            entry_type,
                                            entry_type.label(),
                                        );
                                    }
                                })
                                .response;
                            ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_label(type_label);
                            });
                            changed |= selected != self.entry_type;
                        } else {
                            ui.label(EntryType::File.label());
                        }
                        ui.end_row();

                        for (label, input, hint, width) in [
                            (crate::i18n::key!("Расширение"), &mut self.extension, text!("например txt"), 135.0),
                            (crate::i18n::key!("Минимальный размер"), &mut self.minimum_size, "", 135.0),
                            (crate::i18n::key!("Максимальный размер"), &mut self.maximum_size, "", 135.0),
                            (crate::i18n::key!("Изменено с"), &mut self.modified_from, "YYYY-MM-DD", 135.0),
                            (crate::i18n::key!("Изменено по"), &mut self.modified_through, "YYYY-MM-DD", 135.0),
                            (
                                crate::i18n::key!("Максимальная глубина"),
                                &mut self.depth,
                                text!("1 = прямые потомки"),
                                150.0,
                            ),
                        ] {
                            ui.label(text(label));
                            let response = ui.add(
                                egui::TextEdit::singleline(input)
                                    .id_salt(label)
                                    .hint_text(hint)
                                    .desired_width(width),
                            );
                            ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_label(text(label));
                            });
                            changed |= response.changed();
                            submit |= response.lost_focus() && enter;
                            ui.end_row();
                        }
                        let unit_label = text!("Единица размера");
                        ui.label(unit_label);
                        let unit = self.size_unit;
                        let response = egui::ComboBox::from_id_salt("search_size_unit")
                            .selected_text(unit.label())
                            .show_ui(ui, |ui| {
                                for unit in [
                                    SizeUnit::Bytes,
                                    SizeUnit::KiB,
                                    SizeUnit::MiB,
                                    SizeUnit::GiB,
                                ] {
                                    ui.selectable_value(&mut self.size_unit, unit, unit.label());
                                }
                            })
                            .response;
                        ui.ctx().accesskit_node_builder(response.id, |node| {
                            node.set_label(unit_label);
                        });
                        changed |= unit != self.size_unit;
                        ui.end_row();
                    });
                ui.small(text!("Размер и расширение относятся к обычным файлам. Границы включены. Даты местные."));
                if ui.button(text!("Сбросить фильтры")).clicked() {
                    *self = Self::default();
                    changed = true;
                }
                (changed, submit)
            })
            .body_returned
            .unwrap_or_default()
    }
}

fn parse_number<T>(input: &str, convert: impl FnOnce(u64) -> Option<T>) -> Result<Option<T>, ()> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    if !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    input.parse().ok().and_then(convert).map(Some).ok_or(())
}

fn parse_size(input: &str, unit: SizeUnit) -> Result<Option<u64>, String> {
    parse_number(input, |size| size.checked_mul(unit.bytes())).map_err(|()| {
        crate::tr_format!(
            "Введите размер целым неотрицательным числом в выбранных единицах.",
            "Enter a whole nonnegative size that fits in the selected unit."
        )
    })
}

fn parse_date(input: &str) -> Result<Option<NaiveDate>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    if input.len() == 10
        && input.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
        && let Ok(date) = NaiveDate::parse_from_str(input, "%Y-%m-%d")
        && date.year() > 0
    {
        return Ok(Some(date));
    }
    Err(crate::tr_format!(
        "Введите действительную дату в формате ГГГГ-ММ-ДД.",
        "Enter a valid date in YYYY-MM-DD format."
    ))
}

struct Bounds<T> {
    min: Option<T>,
    max: Option<T>,
}

impl<T: Ord> Bounds<T> {
    fn new(min: Option<T>, max: Option<T>) -> Option<Self> {
        if matches!((&min, &max), (Some(min), Some(max)) if min > max) {
            return None;
        }
        Some(Self { min, max })
    }

    fn active(&self) -> bool {
        self.min.is_some() || self.max.is_some()
    }

    fn includes(&self, value: T) -> bool {
        self.min.as_ref().is_none_or(|min| value >= *min)
            && self.max.as_ref().is_none_or(|max| value <= *max)
    }
}

pub(super) struct Filters {
    entry_type: EntryType,
    extension: Option<String>,
    size: Bounds<u64>,
    modified: Bounds<NaiveDate>,
    pub depth: Option<usize>,
}

impl Filters {
    pub fn active(&self) -> bool {
        self.entry_type != EntryType::Any
            || self.extension.is_some()
            || self.needs_metadata()
            || self.depth.is_some()
    }

    pub fn matches_kind(&self, kind: Option<EntryKind>) -> bool {
        match self.entry_type {
            EntryType::Any => true,
            EntryType::File => kind == Some(EntryKind::File),
            EntryType::Directory => kind == Some(EntryKind::Directory),
            EntryType::Other => kind == Some(EntryKind::Other),
        }
    }

    pub fn matches_extension(&self, name: &OsStr) -> bool {
        self.extension.as_ref().is_none_or(|expected| {
            Path::new(name)
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| normalize(extension) == expected.as_str())
        })
    }

    pub fn needs_metadata(&self) -> bool {
        self.size.active() || self.modified.active()
    }

    pub fn matches_metadata(&self, metadata: EntryMetadata) -> io::Result<bool> {
        if !self.matches_kind(Some(metadata.kind)) {
            return Ok(false);
        }
        if self.size.active()
            && !self
                .size
                .includes(metadata.size.ok_or(io::ErrorKind::InvalidData)?)
        {
            return Ok(false);
        }
        if !self.modified.active() {
            return Ok(true);
        }
        let utc = metadata.modified.ok_or(io::ErrorKind::InvalidData)?;
        let local = utc.with_timezone(&Local);
        let local_date = utc
            .naive_utc()
            .checked_add_offset(*local.offset())
            .ok_or(io::ErrorKind::InvalidData)?
            .date();
        Ok(self.modified.includes(local_date))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        search::{run_folder, FolderSearch, Search},
        vfs::Location,
    };
    use chrono::TimeZone;
    use std::{fs, io::Write};

    fn write_fixture(path: &Path, size: usize, day: u32, binary: bool) {
        let mut file = fs::File::create(path).unwrap();
        let mut bytes = vec![b' '; size];
        bytes[..7].copy_from_slice(b"needle\n");
        if binary {
            bytes[size - 1] = 0;
        }
        file.write_all(&bytes).unwrap();
        let date = Local.with_ymd_and_hms(2025, 1, day, 0, 5, 0).unwrap();
        file.set_modified(date.into()).unwrap();
    }

    #[test]
    fn applies_every_filter_before_scanning_content() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        for directory in [
            &nested,
            &nested.join("deeper"),
            &nested.join("excluded-dir"),
            &nested.join("needle-folder.txt"),
        ] {
            fs::create_dir(directory).unwrap();
        }
        let matching_path = nested.join("needle-данные.TXT");
        write_fixture(&matching_path, 1024, 17, false);
        for (name, size, day) in [
            ("needle-old.txt", 1024, 16),
            ("needle-new.txt", 1024, 18),
            ("needle-small.txt", 1023, 17),
            ("needle-large.txt", 1025, 17),
            ("needle-other.rs", 1024, 17),
            ("excluded-name-needle.txt", 1024, 17),
            ("excluded-dir/needle.txt", 1024, 17),
            ("deeper/needle.txt", 1024, 17),
        ] {
            write_fixture(&nested.join(name), size, day, true);
        }
        let filtered = FilterInput {
            entry_type: EntryType::File,
            extension: ".txt".into(),
            minimum_size: "1".into(),
            maximum_size: "1".into(),
            size_unit: SizeUnit::KiB,
            modified_from: "2025-01-17".into(),
            modified_through: "2025-01-17".into(),
            depth: "2".into(),
        };
        for (target, query) in [
            (FolderTarget::Name, "needle"),
            (FolderTarget::Text, "needle"),
            (FolderTarget::Regex, "^needle"),
        ] {
            let search = FolderSearch::new(
                target,
                query,
                "excluded-*, *.tmp",
                filtered.parse(target).unwrap(),
            )
            .unwrap();
            let results = run_folder(root.path(), search, &|| false).unwrap();
            assert_eq!(results.matches.len(), 1);
            assert_eq!(results.matches[0].path.path(), matching_path);
            assert!(results.warning.is_empty(), "{}", results.warning);
        }
    }

    #[test]
    fn metadata_only_name_search_includes_folders_at_the_depth_boundary() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("folder");
        fs::create_dir_all(folder.join("nested")).unwrap();
        fs::write(root.path().join("file"), "").unwrap();
        let filtered = FilterInput {
            entry_type: EntryType::Directory,
            depth: "1".into(),
            ..Default::default()
        };
        let search = FolderSearch::new(
            FolderTarget::Name,
            " ",
            "",
            filtered.parse(FolderTarget::Name).unwrap(),
        )
        .unwrap();
        let results = run_folder(root.path(), search, &|| false).unwrap();
        assert_eq!(results.matches.len(), 1);
        assert_eq!(results.matches[0].path.path(), folder);
        assert!(results.warning.is_empty());
    }

    #[test]
    fn rejects_invalid_filters_and_preserves_empty_and_zero_bounds() {
        for input in ["-1", "1.5", "+1", "18446744073709551616"] {
            assert!(parse_size(input, SizeUnit::Bytes).is_err(), "{input}");
        }
        assert!(parse_size("18446744073709551615", SizeUnit::KiB).is_err());
        assert_eq!(parse_size(" 0 ", SizeUnit::MiB).unwrap(), Some(0));
        assert_eq!(parse_size(" ", SizeUnit::GiB).unwrap(), None);
        for input in ["2025-02-29", "2025-2-09", "0000-01-01", "01/17/2025"] {
            assert!(parse_date(input).is_err(), "{input}");
        }
        assert_eq!(
            parse_date(" 2024-02-29 ").unwrap(),
            NaiveDate::from_ymd_opt(2024, 2, 29)
        );
        for filtered in [
            FilterInput {
                minimum_size: "2".into(),
                maximum_size: "1".into(),
                ..Default::default()
            },
            FilterInput {
                modified_from: "2025-01-02".into(),
                modified_through: "2025-01-01".into(),
                ..Default::default()
            },
            FilterInput {
                depth: "0".into(),
                ..Default::default()
            },
            FilterInput {
                depth: (MAX_OPEN_DIRECTORIES + 1).to_string(),
                ..Default::default()
            },
            FilterInput {
                extension: "*.txt".into(),
                ..Default::default()
            },
            FilterInput {
                entry_type: EntryType::Directory,
                maximum_size: "0".into(),
                ..Default::default()
            },
        ] {
            assert!(filtered.parse(FolderTarget::Name).is_err());
        }
    }

    #[test]
    fn validates_before_starting_and_limits_queryless_search_to_name() {
        let root = tempfile::tempdir().unwrap();
        let mut search = Search::default();
        search.open(Some(&Location::Local(root.path().to_path_buf())));
        search.filters.depth = "0".into();
        assert!(search.can_start(None));
        search.start(None, &egui::Context::default());
        assert!(search.error.is_some());
        assert!(search.job().is_none());
        search.filters.depth = "1".into();
        assert!(search.can_start(None));
        search.folder_target = FolderTarget::Text;
        assert!(!search.can_start(None));
    }
}
