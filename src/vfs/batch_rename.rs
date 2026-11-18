use super::location::validate_name;
use super::transfer::{
    canonical, entry_metadata, remote_source_matches, rename_for_publication, EntryMetadata,
    TransferSourceManifest,
};
use super::undo::{Capture, Parent, MAX_UNDO_ENTRIES};
use super::Location;
use crate::i18n::text;
use crate::operations::{Cancellation, Progress, TransferError};
use crate::result_history::Outcome;
use icu_normalizer::ComposingNormalizerBorrowed;
use regex::RegexBuilder;
use std::collections::{HashMap, HashSet};
use std::path::Path;

const MAX_RULE_BYTES: usize = 4096;

#[derive(Clone)]
pub(crate) struct BatchRenameRules {
    pub template: String,
    pub search: String,
    pub replacement: String,
    pub start: u32,
    pub digits: usize,
}

impl Default for BatchRenameRules {
    fn default() -> Self {
        Self {
            template: "{stem}{ext}".into(),
            search: String::new(),
            replacement: String::new(),
            start: 1,
            digits: 1,
        }
    }
}

#[derive(Clone, Copy)]
enum Part<'a> {
    Literal(&'a str),
    Stem,
    Extension,
    Counter,
}

fn parse_template(input: &str) -> Result<Vec<Part<'_>>, String> {
    let invalid =
        || text("В шаблоне допустимы {stem}, {ext} и {n}. Скобки можно удвоить.").to_owned();
    let mut parts = Vec::new();
    let mut rest = input;
    while let Some(index) = rest.find(['{', '}']) {
        let (literal, token) = rest.split_at(index);
        parts.push(Part::Literal(literal));
        if token.starts_with("{{") || token.starts_with("}}") {
            parts.push(Part::Literal(&token[..1]));
            rest = &token[2..];
        } else {
            let (name, tail) = token
                .strip_prefix('{')
                .and_then(|token| token.split_once('}'))
                .ok_or_else(invalid)?;
            parts.push(match name {
                "stem" => Part::Stem,
                "ext" => Part::Extension,
                "n" => Part::Counter,
                _ => return Err(invalid()),
            });
            rest = tail;
        }
    }
    parts.push(Part::Literal(rest));
    Ok(parts)
}

fn name_parts<'a>(name: &'a str, metadata: &EntryMetadata) -> (&'a str, &'a str) {
    let directory = match metadata {
        EntryMetadata::Local(meta) => meta.is_dir(),
        EntryMetadata::Remote(stat) => stat.is_dir(),
    };
    let path = Path::new(name);
    if !directory && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
        return (stem, &name[stem.len()..]);
    }
    (name, "")
}

pub(crate) struct BatchRenameRow {
    pub source: Location,
    pub name: Option<String>,
    pub problem: Option<String>,
    expected: Option<EntryMetadata>,
}

impl BatchRenameRow {
    pub fn changes_name(&self) -> bool {
        self.name.as_deref().is_some_and(|name| {
            self.source
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                != Some(name)
        })
    }

    fn target(&self) -> Location {
        self.source
            .parent()
            .expect("batch parent")
            .join(self.name.as_ref().expect("preview name"))
    }
}

pub(crate) struct BatchRenamePreview {
    pub rows: Vec<BatchRenameRow>,
    pub changes: usize,
    pub conflicts: usize,
    directory: Location,
    parent: Parent,
}

impl BatchRenamePreview {
    pub fn build(
        sources: &[Location],
        rules: &BatchRenameRules,
        cancel: &Cancellation,
    ) -> Result<Self, String> {
        if !(2..=MAX_UNDO_ENTRIES).contains(&sources.len()) {
            return Err(text("Выберите от 2 до 100 000 элементов.").into());
        }
        if [&rules.template, &rules.search, &rules.replacement]
            .iter()
            .any(|input| input.len() > MAX_RULE_BYTES)
        {
            return Err(text("Шаблон или регулярное выражение слишком длинные.").into());
        }
        cancel.check().map_err(|error| error.to_string())?;
        let directory = sources[0]
            .parent()
            .ok_or_else(|| text("Для массового переименования выберите элементы в одной папке."))?;
        let mut unique = HashSet::new();
        if directory.is_archive()
            || sources.iter().any(|source| {
                source.parent().as_ref() != Some(&directory) || !unique.insert(source)
            })
        {
            return Err(
                text("Для массового переименования выберите элементы в одной папке.").into(),
            );
        }
        let template = parse_template(&rules.template)?;
        let regex = (!rules.search.is_empty())
            .then(|| {
                RegexBuilder::new(&rules.search)
                    .size_limit(1024 * 1024)
                    .build()
            })
            .transpose()
            .map_err(|error| {
                crate::tr_format!(
                    "Недопустимое регулярное выражение: {error}",
                    "Invalid regular expression: {error}"
                )
            })?;
        let parent = Parent::capture(&directory)?;
        let mut rows = Vec::with_capacity(sources.len());
        for (index, source) in sources.iter().enumerate() {
            cancel.check().map_err(|error| error.to_string())?;
            let mut row = BatchRenameRow {
                source: source.clone(),
                name: None,
                problem: None,
                expected: None,
            };
            let result = (|| {
                let old = source
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| text("Массовое переименование требует имена в UTF-8."))?;
                let actual_source = parent.location.join(old);
                let metadata = entry_metadata(&actual_source)?
                    .ok_or_else(|| changed_since_preview(&row.source))?;
                let (stem, extension) = name_parts(old, &metadata);
                let renamed_stem = regex
                    .as_ref()
                    .map_or(std::borrow::Cow::Borrowed(stem), |regex| {
                        regex.replace_all(stem, rules.replacement.as_str())
                    });
                let counter = u64::from(rules.start) + index as u64;
                let counter = format!("{counter:0width$}", width = rules.digits.clamp(1, 12));
                let mut name = String::new();
                for part in &template {
                    name.push_str(match part {
                        Part::Literal(value) => value,
                        Part::Stem => &renamed_stem,
                        Part::Extension => extension,
                        Part::Counter => &counter,
                    });
                    if name.len() > MAX_RULE_BYTES {
                        return Err(text("Новое имя слишком длинное.").into());
                    }
                }
                row.name = Some(name);
                let name = row.name.as_deref().unwrap();
                validate_name(name).map_err(|_| {
                    text("Новое имя должно быть допустимым именем файла или папки.")
                })?;
                if name != old {
                    let target = parent.location.join(name);
                    ensure_vacant(&target)?;
                    row.expected = Some(metadata);
                }
                Ok::<_, String>(())
            })();
            if let Err(error) = result {
                row.problem = Some(error);
            }
            rows.push(row);
        }
        mark_duplicate_targets(&mut rows, case_sensitive(&parent.location));
        parent.verify()?;
        cancel.check().map_err(|error| error.to_string())?;
        let changes = rows.iter().filter(|row| row.changes_name()).count();
        let conflicts = rows.iter().filter(|row| row.problem.is_some()).count();
        Ok(Self {
            rows,
            changes,
            conflicts,
            directory,
            parent,
        })
    }

    pub fn can_apply(&self) -> bool {
        self.changes != 0 && self.conflicts == 0
    }

    pub fn directory(&self) -> &Location {
        &self.directory
    }

    pub fn apply(self, progress: &mut Progress) -> Result<String, String> {
        self.apply_using(progress, rename_for_publication)
    }

    fn apply_using(
        self,
        progress: &mut Progress,
        mut rename: impl FnMut(&Location, &Location) -> Result<(), TransferError>,
    ) -> Result<String, String> {
        if !self.can_apply() {
            return Err(text("Сначала проверьте новые имена.").into());
        }
        let changes: Vec<_> = self.rows.iter().filter(|row| row.changes_name()).collect();
        progress.results.plan(
            changes
                .iter()
                .map(|row| (row.source.clone(), Some(row.target().display()))),
        );
        let cancel = progress.cancellation();
        for row in &changes {
            progress.item(row.source.path());
            if let Err(error) = self.check_row(row, &cancel) {
                return finish_failure(progress, row, error, 0, changes.len());
            }
        }
        for (completed, row) in changes.iter().enumerate() {
            progress.item(row.source.path());
            progress.results.begin(&row.source);
            let mut apply_row = || -> Result<(), TransferError> {
                let (source, target) = self.check_row(row, &cancel)?;
                let capture = Capture::before(&source, &target)?;
                let manifest = TransferSourceManifest::capture_move_observed(
                    &source,
                    &cancel,
                    &mut |_, _| {},
                )?;
                if !progress.undo.can_fit(manifest.entries.len()) {
                    return Err(
                        text("Слишком много файлов для безопасной отмены переименования.").into(),
                    );
                }
                manifest.verify(&cancel, |_, _| {})?;
                self.check_row(row, &cancel)?;
                rename(&source, &target)?;
                let receipt = capture
                    .finish(&source, &manifest)
                    .map_err(TransferError::PublicationUnconfirmed)?;
                progress.results.descendants(
                    &source,
                    &row.target(),
                    manifest.entries.keys().cloned(),
                );
                progress.undo.push(receipt);
                Ok(())
            };
            if let Err(error) = apply_row() {
                return finish_failure(progress, row, error, completed, changes.len());
            }
            progress
                .results
                .record(&row.source, None, Outcome::Completed, None);
        }
        let count = changes.len();
        Ok(crate::tr_format!(
            "Переименовано элементов: {count}",
            "Items renamed: {count}"
        ))
    }

    fn check_row(
        &self,
        row: &BatchRenameRow,
        cancel: &Cancellation,
    ) -> Result<(Location, Location), TransferError> {
        cancel.check()?;
        self.parent.verify()?;
        if self.directory.path() != self.parent.location.path()
            && canonical(&self.directory)? != self.parent.location.path()
        {
            return Err(changed_since_preview(&self.directory).into());
        }
        let source = self
            .parent
            .location
            .join(row.source.path().file_name().unwrap());
        let target = self.parent.location.join(row.name.as_ref().unwrap());
        let expected = row.expected.as_ref().expect("eligible rename");
        let actual = entry_metadata(&source)?.ok_or_else(|| changed_since_preview(&row.source))?;
        let matches = match (expected, &actual) {
            (EntryMetadata::Local(before), EntryMetadata::Local(after)) => {
                expected.matches_local_move(&actual)
                    && before.len() == after.len()
                    && before.modified().ok() == after.modified().ok()
                    && before.permissions() == after.permissions()
            }
            (EntryMetadata::Remote(before), EntryMetadata::Remote(after)) => {
                remote_source_matches(before, after)
            }
            _ => false,
        };
        if !matches {
            return Err(changed_since_preview(&row.source).into());
        }
        ensure_vacant(&target)?;
        cancel.check()?;
        Ok((source, target))
    }
}

fn changed_since_preview(source: &Location) -> String {
    crate::tr_format!(
        "Элемент изменился или исчез после выбора: {source}",
        "An item changed or disappeared after selection: {source}"
    )
}

fn ensure_vacant(target: &Location) -> Result<(), String> {
    if entry_metadata(target)?.is_some() {
        return Err(crate::tr_format!(
            "Имя уже занято: {target}",
            "Name already exists: {target}"
        ));
    }
    Ok(())
}

fn case_sensitive(parent: &Location) -> bool {
    #[cfg(target_os = "macos")]
    if let Some(local) = parent.local() {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(path) = std::ffi::CString::new(local.as_os_str().as_bytes()) {
            // SAFETY: path is terminated and lives until this synchronous query returns.
            return unsafe { libc::pathconf(path.as_ptr(), libc::_PC_CASE_SENSITIVE) } == 1;
        }
    }
    let _ = parent;
    // SFTP does not expose case sensitivity. Treat possible aliases as conflicts.
    false
}

fn mark_duplicate_targets(rows: &mut [BatchRenameRow], case_sensitive: bool) {
    let mut seen = HashMap::new();
    for index in 0..rows.len() {
        let Some(name) = &rows[index].name else {
            continue;
        };
        let key = if case_sensitive {
            ComposingNormalizerBorrowed::new_nfc()
                .normalize(name)
                .into_owned()
        } else {
            crate::name_match::normalize(name).into_owned()
        };
        if let Some(previous) = seen.insert(key, index) {
            let duplicate = text("Несколько элементов получают совпадающее имя.");
            for duplicate_index in [previous, index] {
                rows[duplicate_index]
                    .problem
                    .get_or_insert_with(|| duplicate.into());
            }
        }
    }
}

fn finish_failure(
    progress: &mut Progress,
    row: &BatchRenameRow,
    error: TransferError,
    completed: usize,
    total: usize,
) -> Result<String, String> {
    let outcome = match &error {
        TransferError::Cancelled => Outcome::Cancelled,
        TransferError::Failed(_) => Outcome::Failed,
        TransferError::PublicationUnconfirmed(_) => Outcome::PublicationUnconfirmed,
    };
    let remaining = total - completed;
    progress
        .results
        .record(&row.source, None, outcome, Some(error.to_string()));
    if outcome == Outcome::Cancelled {
        progress.results.cancelled = true;
        Ok(crate::tr_format!(
            "Переименование остановлено. Переименовано: {completed}. Осталось: {remaining}.",
            "Rename stopped. Renamed: {completed}. Remaining: {remaining}."
        ))
    } else {
        Err(crate::tr_format!(
            "Переименовано: {completed} из {total}. {error}",
            "Renamed: {completed} of {total}. {error}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(root: &Path, names: &[&str]) -> Vec<Location> {
        names
            .iter()
            .map(|name| {
                let path = root.join(name);
                fs::write(&path, name.as_bytes()).unwrap();
                Location::Local(path)
            })
            .collect()
    }

    fn numbered(sources: &[Location]) -> BatchRenamePreview {
        BatchRenamePreview::build(
            sources,
            &BatchRenameRules {
                template: "{n}-{stem}{ext}".into(),
                ..Default::default()
            },
            &Cancellation::default(),
        )
        .unwrap()
    }

    #[test]
    fn batch_rename_previews_groups_counters_and_ext_and_undoes_the_result() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let mut sources = fixture(root, &["cam-12.jpg", "cam-31.png"]);
        let folder = root.join("project.v2");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("marker.txt"), "folder content").unwrap();
        sources.push(folder.into());
        let rules = BatchRenameRules {
            template: "{{set}}-{n}-{stem}{ext}".into(),
            search: "^cam-(\\d+)$".into(),
            replacement: "image-$1".into(),
            start: 7,
            digits: 3,
        };
        let preview =
            BatchRenamePreview::build(&sources, &rules, &Cancellation::default()).unwrap();
        let names = [
            "{set}-007-image-12.jpg",
            "{set}-008-image-31.png",
            "{set}-009-project.v2",
        ];
        assert!(preview.can_apply());
        assert_eq!(preview.changes, 3);
        assert_eq!(
            preview
                .rows
                .iter()
                .map(|row| row.name.as_deref().unwrap())
                .collect::<Vec<_>>(),
            names,
        );
        assert!(names.iter().all(|name| !root.join(name).exists()));
        let mut progress = Progress::default();
        preview.apply(&mut progress).unwrap();
        assert_eq!(progress.results.items.len(), 4);
        assert!(progress.results.items[..3]
            .iter()
            .all(|item| item.descendants_enumerated && item.outcome == Outcome::Completed));
        let descendant = &progress.results.items[3];
        assert_eq!(descendant.parent, Some(2));
        assert_eq!(descendant.outcome, Outcome::Completed);
        let destination = Location::Local(root.join(names[2]).join("marker.txt"));
        assert_eq!(descendant.target, Some(destination.display()));
        assert_eq!(fs::read(root.join(names[0])).unwrap(), b"cam-12.jpg");
        assert_eq!(fs::read(root.join(names[1])).unwrap(), b"cam-31.png");
        assert_eq!(
            fs::read(root.join(names[2]).join("marker.txt")).unwrap(),
            b"folder content"
        );
        assert!(sources.iter().all(|source| !source.path().exists()));
        assert_eq!(progress.undo.restore(&mut Progress::default()).unwrap(), 3);
        assert!(names.iter().all(|name| !root.join(name).exists()));
        assert_eq!(fs::read(sources[0].path()).unwrap(), b"cam-12.jpg");
        assert_eq!(fs::read(sources[1].path()).unwrap(), b"cam-31.png");
        assert_eq!(
            fs::read(sources[2].path().join("marker.txt")).unwrap(),
            b"folder content",
        );
    }

    #[test]
    fn batch_rename_collisions_block_the_whole_preview() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let sources = fixture(root, &["one.txt", "two.txt"]);
        let rules = BatchRenameRules {
            template: "same.txt".into(),
            ..Default::default()
        };
        let duplicate =
            BatchRenamePreview::build(&sources, &rules, &Cancellation::default()).unwrap();
        assert_eq!(duplicate.conflicts, 2);
        assert!(duplicate.apply(&mut Progress::default()).is_err());
        assert!(!root.join("same.txt").exists());
        fs::write(root.join("2-two.txt"), b"existing file").unwrap();
        let occupied = numbered(&sources);
        assert_eq!(occupied.conflicts, 1);
        assert!(occupied.apply(&mut Progress::default()).is_err());
        assert_eq!(fs::read(root.join("2-two.txt")).unwrap(), b"existing file");
        assert!(!root.join("1-one.txt").exists());
        assert!(sources.iter().all(|source| source.path().exists()));
    }

    #[test]
    fn batch_rename_rechecks_stale_previews_before_the_first_change() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let sources = fixture(root, &["one.txt", "two.txt"]);
        let preview = numbered(&sources);
        fs::write(sources[1].path(), b"new content from another app").unwrap();
        let mut progress = Progress::default();
        assert!(preview.apply(&mut progress).is_err());
        assert_eq!(progress.undo.len(), 0);
        assert_eq!(fs::read(sources[0].path()).unwrap(), b"one.txt");
        assert_eq!(
            fs::read(sources[1].path()).unwrap(),
            b"new content from another app"
        );
        assert!(!root.join("1-one.txt").exists());
    }

    #[test]
    fn batch_rename_recovers_confirmed_changes_after_failure_or_stop() {
        for stop in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let sources = fixture(root, &["one.txt", "two.txt", "three.txt"]);
            let preview = numbered(&sources);
            let mut progress = Progress::default();
            let cancel = progress.cancellation();
            let late = root.join("2-two.txt");
            let result = preview.apply_using(&mut progress, |source, target| {
                rename_for_publication(source, target)?;
                if stop {
                    cancel.request();
                } else {
                    fs::write(&late, b"late arrival").unwrap();
                }
                Ok(())
            });
            assert_eq!(result.is_ok(), stop);
            assert_eq!(progress.results.cancelled, stop);
            assert_eq!(progress.undo.len(), 1);
            let rows = progress
                .results
                .root_outcomes()
                .into_iter()
                .collect::<HashMap<_, _>>();
            assert_eq!(rows[&sources[0]], Outcome::Completed);
            assert_eq!(
                rows[&sources[1]],
                if stop {
                    Outcome::Cancelled
                } else {
                    Outcome::Failed
                }
            );
            assert_eq!(rows[&sources[2]], Outcome::NotStarted);
            assert_eq!(fs::read(root.join("1-one.txt")).unwrap(), b"one.txt");
            assert_eq!(fs::read(sources[1].path()).unwrap(), b"two.txt");
            assert_eq!(fs::read(sources[2].path()).unwrap(), b"three.txt");
            assert_eq!(progress.undo.restore(&mut Progress::default()).unwrap(), 1);
            assert_eq!(fs::read(sources[0].path()).unwrap(), b"one.txt");
            assert!(!root.join("1-one.txt").exists());
            if !stop {
                assert_eq!(fs::read(&late).unwrap(), b"late arrival");
            }
        }
    }
}
