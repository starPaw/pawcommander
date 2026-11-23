mod mode;

pub(crate) use mode::ModeRule;

use super::location::{normalize_remote, Location};
use super::sftp::{remote, should_reopen};
use crate::file_error::{AccessAction, AccessCause, AccessError, Failure, PermissionError};
use crate::operations::{Cancellation, Progress, TransferError};
use crate::result_history::Outcome;
use mode::MODE_BITS;
use ssh2::{FileStat, Sftp};
use std::collections::HashSet;

pub(crate) struct PermissionsPreview {
    directory: Location,
    canonical_directory: Location,
    pub(crate) rows: Vec<PermissionRow>,
    pub(crate) changes: usize,
}

pub(crate) struct PermissionRow {
    pub(crate) source: Location,
    pub(crate) change: Result<PermissionChange, TransferError>,
}

pub(crate) struct PermissionChange {
    before: Stamp,
    pub(crate) after: u32,
}

#[derive(Debug, Default)]
pub(crate) struct PermissionsReport {
    pub(crate) completed: usize,
    pub(crate) warned: usize,
    pub(crate) skipped: usize,
    pub(crate) failed: usize,
    pub(crate) cancelled: bool,
    pub(crate) error: Option<TransferError>,
    #[cfg(target_os = "macos")]
    pub(crate) warning: Option<TransferError>,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    perm: u32,
    owner: Option<u32>,
    group: Option<u32>,
    size: Option<u64>,
    modified: Option<u64>,
    directory: bool,
}

impl PermissionsPreview {
    pub(crate) fn load(
        sources: &[Location],
        rule: &ModeRule,
        cancel: &Cancellation,
    ) -> Result<Self, TransferError> {
        let directory = sources
            .first()
            .filter(|source| source.remote())
            .and_then(Location::parent)
            .ok_or(PermissionError::InvalidSelection)?;
        for source in sources {
            if !source.same_backend(&directory)
                || source.path().parent() != Some(directory.path())
                || normalize_remote(source.path())?.as_os_str() != source.path().as_os_str()
            {
                return Err(PermissionError::InvalidSelection.into());
            }
        }
        let (channel, canonical_directory) = open_directory(&directory, cancel)?;
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        let mut changes = 0;
        for source in sources.iter().filter(|&source| seen.insert(source)) {
            cancel.check()?;
            let name = source
                .path()
                .file_name()
                .ok_or(PermissionError::InvalidSelection)?;
            let target = canonical_directory.join(name);
            let observed = Stamp::read(&channel, &target);
            cancel.check()?;
            let change = match observed {
                Ok(before) => {
                    let after = rule.evaluate(before.perm, before.directory);
                    changes += usize::from(after != before.perm & MODE_BITS);
                    Ok(PermissionChange { before, after })
                }
                Err(error) if read_disconnected(&error) => return Err(error),
                Err(error) => Err(error),
            };
            rows.push(PermissionRow {
                source: source.clone(),
                change,
            });
        }
        Ok(Self {
            directory,
            canonical_directory,
            rows,
            changes,
        })
    }

    pub(crate) fn directory(&self) -> &Location {
        &self.directory
    }

    pub(crate) fn apply(self, progress: &mut Progress) -> Result<PermissionsReport, TransferError> {
        progress
            .results
            .plan_selected_items(self.rows.iter().map(|row| row.source.clone()));
        let cancel = progress.cancellation();
        let (channel, canonical) =
            open_directory(&self.directory, &cancel).inspect_err(|error| {
                progress.results.cancelled = matches!(error, TransferError::Cancelled);
            })?;
        if canonical != self.canonical_directory {
            return Err(PermissionError::FolderChanged(self.directory).into());
        }

        let mut report = PermissionsReport::default();
        for row in self.rows {
            if cancel.check().is_err() {
                report.cancelled = true;
                break;
            }
            progress.results.begin(&row.source);
            progress.item(row.source.path());
            let mut stop = false;
            let (outcome, reason) = match row.change {
                Err(reason) => (Outcome::Skipped, Some(reason)),
                Ok(change) if change.current() == change.after => (Outcome::Skipped, None),
                Ok(change) => {
                    let name = row.source.path().file_name().expect("validated entry path");
                    let target = canonical.join(name);
                    match change.apply(&channel, &target, &cancel) {
                        Ok(None) => (Outcome::Completed, None),
                        Ok(Some(warning)) => (Outcome::CompletedWithWarning, Some(warning)),
                        Err(reason @ TransferError::PublicationUnconfirmed(_)) => {
                            stop = true;
                            (Outcome::PublicationUnconfirmed, Some(reason))
                        }
                        Err(TransferError::Cancelled) => {
                            report.cancelled = true;
                            stop = true;
                            (Outcome::Cancelled, None)
                        }
                        Err(reason) => {
                            stop = read_disconnected(&reason);
                            (Outcome::Failed, Some(reason))
                        }
                    }
                }
            };
            progress
                .results
                .record_with_error(&row.source, outcome, reason.as_ref());
            match outcome {
                Outcome::Completed => report.completed += 1,
                Outcome::CompletedWithWarning => {
                    report.warned += 1;
                    #[cfg(target_os = "macos")]
                    if report.warning.is_none() {
                        report.warning = reason;
                    }
                }
                Outcome::Skipped => report.skipped += 1,
                Outcome::Failed => {
                    report.failed += 1;
                    if report.error.is_none() || stop {
                        report.error = reason;
                    }
                }
                Outcome::PublicationUnconfirmed => report.error = reason,
                _ => {}
            }
            if stop {
                break;
            }
        }
        progress.results.cancelled = report.cancelled;
        Ok(report)
    }
}

impl PermissionChange {
    pub(crate) fn current(&self) -> u32 {
        self.before.perm & MODE_BITS
    }

    fn apply(
        &self,
        channel: &Sftp,
        target: &Location,
        cancel: &Cancellation,
    ) -> Result<Option<TransferError>, TransferError> {
        // SFTP SETSTAT uses a path. Check the previewed attributes and the final
        // entry type immediately before requesting the change.
        let current = Stamp::read(channel, target)?;
        if current != self.before {
            return Err(PermissionError::ItemChanged(target.clone()).into());
        }
        cancel.check()?;
        let attributes = FileStat {
            perm: Some(self.after),
            size: None,
            uid: None,
            gid: None,
            atime: None,
            mtime: None,
        };
        channel
            .setstat(target.path(), attributes)
            .map_err(|source| {
                let disconnected = should_reopen(&source);
                let error = TransferError::from(PermissionError::SetMode {
                    location: target.clone(),
                    source,
                });
                if disconnected {
                    error.with_unconfirmed_publication()
                } else {
                    error
                }
            })?;
        match Stamp::read(channel, target) {
            Ok(actual) if actual.perm & MODE_BITS == self.after => Ok(None),
            Ok(actual) => Ok(Some(
                PermissionError::ModeMismatch {
                    location: target.clone(),
                    expected: self.after,
                    actual: actual.perm & MODE_BITS,
                }
                .into(),
            )),
            Err(cause) => {
                let disconnected = read_disconnected(&cause);
                let error = TransferError::from(PermissionError::VerifyMode(cause));
                if disconnected {
                    Err(error.with_unconfirmed_publication())
                } else {
                    Ok(Some(error))
                }
            }
        }
    }
}

impl Stamp {
    fn read(channel: &Sftp, target: &Location) -> Result<Self, TransferError> {
        let stat = channel
            .lstat(target.path())
            .map_err(|error| AccessError::new(target, AccessAction::ReadMetadata, error))?;
        let perm = stat
            .perm
            .ok_or_else(|| PermissionError::MissingMode(target.clone()))?;
        if !stat.is_file() && !stat.is_dir() {
            return Err(PermissionError::UnsupportedType(target.clone()).into());
        }
        Ok(Self {
            perm,
            owner: stat.uid,
            group: stat.gid,
            size: stat.size,
            modified: stat.mtime,
            directory: stat.is_dir(),
        })
    }
}

fn open_directory(
    directory: &Location,
    cancel: &Cancellation,
) -> Result<(Sftp, Location), TransferError> {
    cancel.check()?;
    let channel = remote(directory)
        .map_err(|error| AccessError::new(directory, AccessAction::ResolvePath, error))?;
    cancel.check()?;
    let path = channel
        .realpath(directory.path())
        .map_err(|error| AccessError::new(directory, AccessAction::ResolvePath, error))?;
    cancel.check()?;
    Ok((channel, directory.with_path(path)))
}

fn read_disconnected(error: &TransferError) -> bool {
    matches!(error, TransferError::Failed(Failure::Access(error))
        if matches!(&error.cause, AccessCause::Sftp(source) if should_reopen(source)))
}
