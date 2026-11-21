//! Read-only comparison of two folder trees by exact relative name.
pub(super) mod scan;
#[cfg(test)]
mod tests;
#[cfg(all(test, target_os = "macos"))]
pub(super) use tests::comparison_round_trip;

use super::{read_scope::ReadScope, transfer::canonical, Location, MAX_TREE_DEPTH};
use crate::{
    file_error::{Failure, MutationError},
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
};
use scan::{Entry, FileInfo};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

pub(super) const MAX_ENTRIES: usize = 100_000;
pub(super) const MAX_PATH_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ComparisonMode {
    #[default]
    Metadata,
    Contents,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ComparisonStatus {
    Same,
    LeftOnly,
    RightOnly,
    Changed,
    TypeChanged,
    Unreadable(String),
}

#[derive(Debug)]
pub(crate) struct ComparisonRow {
    pub(crate) path: PathBuf,
    pub(crate) status: ComparisonStatus,
}

#[derive(Debug, Default)]
pub(crate) struct Comparison {
    pub(crate) rows: Vec<ComparisonRow>,
    pub(crate) differences: Vec<usize>,
    pub(crate) same: usize,
    pub(crate) left_only: usize,
    pub(crate) right_only: usize,
    pub(crate) changed: usize,
    pub(crate) unreadable: usize,
    path_bytes: usize,
}

impl Comparison {
    fn add(&mut self, path: PathBuf, status: ComparisonStatus) -> Result<(), TransferError> {
        let reason_bytes = match &status {
            ComparisonStatus::Unreadable(reason) => reason.len(),
            _ => 0,
        };
        self.path_bytes += path.as_os_str().len() + reason_bytes;
        if self.path_bytes > MAX_PATH_BYTES {
            return Err(limit());
        }
        match &status {
            ComparisonStatus::Same => self.same += 1,
            ComparisonStatus::LeftOnly => self.left_only += 1,
            ComparisonStatus::RightOnly => self.right_only += 1,
            ComparisonStatus::Changed | ComparisonStatus::TypeChanged => self.changed += 1,
            ComparisonStatus::Unreadable(_) => self.unreadable += 1,
        }
        if status != ComparisonStatus::Same {
            self.differences.push(self.rows.len());
        }
        self.rows.push(ComparisonRow { path, status });
        Ok(())
    }
}

pub(crate) fn compare_folders(
    folders: &[Location; 2],
    mode: ComparisonMode,
    progress: &mut Progress,
) -> Result<Comparison, TransferError> {
    let cancel = progress.cancellation();
    let mut open = |side: usize| {
        cancel.check()?;
        progress.item(folders[side].path());
        ReadScope::open(&folders[side]).map_err(|error| side_error(side, error))
    };
    let scopes = [open(0)?, open(1)?];
    let mut scan = Compare {
        scopes: &scopes,
        mode,
        progress,
        cancel,
        budget: Budget::default(),
        result: Comparison::default(),
    };
    scan.walk(PathBuf::new(), [true, true], 0)?;
    for (side, (scope, folder)) in scopes.iter().zip(folders).enumerate() {
        scan.cancel.check()?;
        scope.parent.verify().map_err(|error| {
            let error = match error {
                TransferError::Failed(Failure::Mutation(ref reason))
                    if matches!(reason.as_ref(), MutationError::UndoUnavailable(_)) =>
                {
                    scan::folder_changed()
                }
                error => error,
            };
            side_error(side, error)
        })?;
        if canonical(folder).map_err(|error| side_error(side, error))?
            != scope.parent.location.path()
        {
            return Err(side_error(side, scan::folder_changed()));
        }
    }
    Ok(scan.result)
}

#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: usize,
    exhausted: bool,
}

impl Budget {
    fn visit(&mut self, bytes: usize) -> Result<(), TransferError> {
        self.entries += 1;
        self.bytes += bytes;
        self.exhausted = self.entries > MAX_ENTRIES || self.bytes > MAX_PATH_BYTES;
        if self.exhausted {
            return Err(limit());
        }
        Ok(())
    }
}

struct Compare<'a> {
    scopes: &'a [ReadScope; 2],
    mode: ComparisonMode,
    progress: &'a mut Progress,
    cancel: Cancellation,
    budget: Budget,
    result: Comparison,
}

impl Compare<'_> {
    fn walk(
        &mut self,
        relative: PathBuf,
        present: [bool; 2],
        depth: usize,
    ) -> Result<(), TransferError> {
        self.cancel.check()?;
        if depth > MAX_TREE_DEPTH {
            return self.result.add(
                relative,
                ComparisonStatus::Unreadable(text!("Достигнут предел вложенности папок").into()),
            );
        }
        let mut children: BTreeMap<OsString, [Option<Entry>; 2]> = BTreeMap::new();
        for (side, scope) in self
            .scopes
            .iter()
            .enumerate()
            .filter(|(side, _)| present[*side])
        {
            self.progress
                .item(scope.parent.location.join(&relative).path());
            let result = scan::visit(scope, &relative, &self.cancel, &mut |name, entry| {
                self.budget.visit(name.len() + entry.extra_path_bytes())?;
                let slot = &mut children.entry(name).or_default()[side];
                if slot.is_some() {
                    return Err(text!("Папка содержит повторяющиеся имена").into());
                }
                *slot = Some(entry);
                Ok(())
            });
            if let Err(error) = result {
                if self.budget.exhausted || matches!(error, TransferError::Cancelled) {
                    return Err(error);
                }
                let error = side_error(side, error);
                return self
                    .result
                    .add(relative, ComparisonStatus::Unreadable(error.to_string()));
            }
        }
        if present[0] != present[1] {
            let status = if present[0] {
                ComparisonStatus::LeftOnly
            } else {
                ComparisonStatus::RightOnly
            };
            self.result.add(relative.clone(), status)?;
        }
        for (name, pair) in children {
            self.cancel.check()?;
            self.compare_entry(relative.join(name), pair, depth)?;
        }
        Ok(())
    }

    fn compare_entry(
        &mut self,
        path: PathBuf,
        pair: [Option<Entry>; 2],
        depth: usize,
    ) -> Result<(), TransferError> {
        use ComparisonStatus::{Changed, LeftOnly, RightOnly, Same, TypeChanged, Unreadable};

        let status = match pair {
            [Some(Entry::Unreadable(reason)), _] => {
                Unreadable(side_error(0, reason.into()).to_string())
            }
            [_, Some(Entry::Unreadable(reason))] => {
                Unreadable(side_error(1, reason.into()).to_string())
            }
            [Some(Entry::Directory), Some(Entry::Directory)] => {
                return self.walk(path, [true, true], depth + 1)
            }
            [Some(Entry::Directory), None] => return self.walk(path, [true, false], depth + 1),
            [None, Some(Entry::Directory)] => return self.walk(path, [false, true], depth + 1),
            [Some(_), None] => LeftOnly,
            [None, Some(_)] => RightOnly,
            [Some(Entry::File(left)), Some(Entry::File(right))] => {
                match self.compare_files(&path, &left, &right) {
                    Ok(true) => Same,
                    Ok(false) => Changed,
                    Err(TransferError::Cancelled) => return Err(TransferError::Cancelled),
                    Err(error) => Unreadable(error.to_string()),
                }
            }
            [Some(Entry::Link(left)), Some(Entry::Link(right))] => {
                if left.as_os_str() == right.as_os_str() {
                    Same
                } else {
                    Changed
                }
            }
            [Some(_), Some(_)] => TypeChanged,
            [None, None] => unreachable!("paired directory entry has at least one side"),
        };
        self.result.add(path, status)
    }

    fn compare_files(
        &mut self,
        path: &Path,
        left: &FileInfo,
        right: &FileInfo,
    ) -> Result<bool, TransferError> {
        let sizes = left.size.zip(right.size);
        if sizes.is_some_and(|(left, right)| left != right) {
            return Ok(false);
        }
        if self.mode == ComparisonMode::Metadata {
            return match left.modified.as_ref().zip(right.modified.as_ref()) {
                Some((left, right)) if !left.matches(right) => Ok(false),
                Some(_) if sizes.is_some() => Ok(true),
                _ => Err(text!("Не удалось получить размер или время изменения").into()),
            };
        }
        let mut digest = |side: usize| {
            self.scopes[side]
                .digest(path, self.progress, &self.cancel)
                .and_then(|digest| digest.ok_or_else(super::read_scope::changed_during_read))
                .map_err(|error| side_error(side, error))
        };
        Ok(digest(0)? == digest(1)?)
    }
}

fn side_error(side: usize, error: TransferError) -> TransferError {
    if matches!(error, TransferError::Cancelled) {
        return error;
    }
    TransferError::Failed(Failure::ComparisonPane {
        right: side != 0,
        cause: Box::new(error),
    })
}

fn limit() -> TransferError {
    text!("Достигнут предел сравнения: 100 000 объектов или 32 МиБ данных о путях").into()
}
