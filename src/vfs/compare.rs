//! Read-only comparison of two folder trees by exact relative name.
pub(super) mod scan;
#[cfg(test)]
mod tests;
#[cfg(all(test, target_os = "macos"))]
pub(super) use tests::{comparison_round_trip, comparison_sftp_denials};

use super::{read_scope::ReadScope, transfer::canonical, Location, MAX_TREE_DEPTH};
use crate::{
    file_error::{Failure, ScanError},
    operations::{Cancellation, Progress, TransferError},
};
use scan::{Entry, FileInfo};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

pub(super) const MAX_ENTRIES: usize = 100_000;
pub(super) const MAX_PATH_BYTES: usize = 32 * 1024 * 1024;

#[cfg(target_os = "macos")]
pub(super) fn scan_limit(
    root: &Location,
    path: &Path,
    entries: usize,
    path_bytes: usize,
) -> Option<crate::file_error::ScanLimit> {
    use crate::file_error::{ScanLimit, ScanLimitKind};

    let (kind, maximum) = if entries > MAX_ENTRIES {
        (ScanLimitKind::Entries, MAX_ENTRIES)
    } else if path_bytes > MAX_PATH_BYTES {
        (ScanLimitKind::PathBytes, MAX_PATH_BYTES)
    } else if path.components().count() > MAX_TREE_DEPTH {
        (ScanLimitKind::Depth, MAX_TREE_DEPTH)
    } else {
        return None;
    };
    Some(ScanLimit {
        location: root.join(path),
        kind,
        maximum,
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ComparisonMode {
    #[default]
    Metadata,
    Contents,
}

#[derive(Debug)]
pub(crate) enum ComparisonStatus {
    Same,
    LeftOnly,
    RightOnly,
    Changed,
    TypeChanged,
    Unreadable(Box<TransferError>),
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
            ComparisonStatus::Unreadable(error) => diagnostic_bytes(error),
            _ => 0,
        };
        self.path_bytes += path.as_os_str().len() + reason_bytes;
        if self.path_bytes > MAX_PATH_BYTES {
            return Err(ScanError::ComparisonLimit.into());
        }
        match &status {
            ComparisonStatus::Same => self.same += 1,
            ComparisonStatus::LeftOnly => self.left_only += 1,
            ComparisonStatus::RightOnly => self.right_only += 1,
            ComparisonStatus::Changed | ComparisonStatus::TypeChanged => self.changed += 1,
            ComparisonStatus::Unreadable(_) => self.unreadable += 1,
        }
        if !matches!(status, ComparisonStatus::Same) {
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
        scope
            .verify_root()
            .map_err(|error| side_error(side, error))?;
        if canonical(folder).map_err(|error| side_error(side, error))?
            != scope.parent.location.path()
        {
            return Err(side_error(
                side,
                ScanError::FolderUnavailable(folder.clone()).into(),
            ));
        }
    }
    Ok(scan.result)
}

#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: usize,
}

impl Budget {
    fn visit(&mut self, bytes: usize) -> Result<(), TransferError> {
        self.entries += 1;
        self.bytes += bytes;
        if self.entries > MAX_ENTRIES || self.bytes > MAX_PATH_BYTES {
            return Err(ScanError::ComparisonLimit.into());
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
            let location = self.scopes[usize::from(!present[0])]
                .parent
                .location
                .join(&relative);
            let error = TransferError::from(ScanError::DepthLimit(location));
            return self
                .result
                .add(relative, ComparisonStatus::Unreadable(Box::new(error)));
        }
        let mut children: BTreeMap<OsString, [Option<Entry>; 2]> = BTreeMap::new();
        for (side, scope) in self
            .scopes
            .iter()
            .enumerate()
            .filter(|(side, _)| present[*side])
        {
            let folder = scope.parent.location.join(&relative);
            self.progress.item(folder.path());
            let result = scan::visit(scope, &relative, &self.cancel, &mut |name, entry| {
                self.budget.visit(name.len() + entry.extra_path_bytes())?;
                let ends = match children.entry(name) {
                    std::collections::btree_map::Entry::Occupied(entry)
                        if entry.get()[side].is_some() =>
                    {
                        return Err(ScanError::DuplicateEntry {
                            directory: folder.clone(),
                            name: entry.key().clone(),
                        }
                        .into());
                    }
                    entry => entry.or_default(),
                };
                ends[side] = Some(entry);
                Ok(())
            });
            if let Err(error) = result {
                if matches!(error, TransferError::Cancelled)
                    || matches!(&error, TransferError::Failed(Failure::Scan(reason))
                        if matches!(reason.as_ref(), ScanError::ComparisonLimit))
                {
                    return Err(error);
                }
                let error = side_error(side, error);
                return self
                    .result
                    .add(relative, ComparisonStatus::Unreadable(Box::new(error)));
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
            [Some(Entry::Unreadable(error)), _] => Unreadable(Box::new(side_error(0, error))),
            [_, Some(Entry::Unreadable(error))] => Unreadable(Box::new(side_error(1, error))),
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
                    Err(error) => Unreadable(Box::new(error)),
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
            if left
                .modified
                .as_ref()
                .zip(right.modified.as_ref())
                .is_some_and(|(left, right)| !left.matches(right))
            {
                return Ok(false);
            }
            for (side, info) in [left, right].into_iter().enumerate() {
                if info.size.is_none() || info.modified.is_none() {
                    let source = self.scopes[side].parent.location.join(path);
                    return Err(side_error(
                        side,
                        ScanError::MissingComparisonMetadata(source).into(),
                    ));
                }
            }
            return Ok(true);
        }
        let mut digest = |side: usize| {
            let scope = &self.scopes[side];
            scope
                .digest(path, self.progress, &self.cancel)
                .and_then(|digest| {
                    digest.ok_or_else(|| {
                        ScanError::FileChanged(scope.parent.location.join(path)).into()
                    })
                })
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

fn diagnostic_bytes(error: &TransferError) -> usize {
    struct Counter(usize);

    impl fmt::Write for Counter {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.0 += text.len();
            Ok(())
        }
    }

    // Bound stored diagnostics along with paths before the UI renders them.
    let mut counter = Counter(0);
    let _ = fmt::write(&mut counter, format_args!("{error:?}"));
    counter.0
}
