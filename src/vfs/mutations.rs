use super::location::{normalize_remote, validate_name, Location};
use super::sftp::remote;
use super::transfer::rename_for_publication;
use super::MAX_TREE_DEPTH;
use crate::file_error::{ListingError, ListingErrorKind, MutationError, SftpRemovalAction};
use crate::fs_ops;
use crate::operations::{Progress, TransferError};
use ssh2::{ErrorCode, Sftp};

pub fn create_directory(parent: &Location, name: &str) -> Result<Location, TransferError> {
    validate_name(name)?;
    if let Some(path) = parent.local() {
        return fs_ops::create_directory(path, name).map(Location::Local);
    }
    if parent.is_archive() {
        return Err(MutationError::ReadOnlyArchive(parent.clone()).into());
    }
    let target = parent.join(name);
    remote(parent)?
        .mkdir(target.path(), 0o755)
        .map_err(|source| MutationError::CreateSftp {
            destination: target.clone(),
            source,
        })?;
    Ok(target)
}

pub fn rename_entry(source: &Location, name: &str) -> Result<Location, TransferError> {
    validate_name(name)?;
    if let Some(path) = source.local() {
        return fs_ops::rename_entry(path, name).map(Location::Local);
    }
    let target = source
        .parent()
        .ok_or_else(|| MutationError::RenameRoot(source.clone()))?
        .join(name);
    if &target != source {
        rename_for_publication(source, &target)?;
    }
    Ok(target)
}

pub(super) fn remove_remote_tree(
    sftp: &Sftp,
    location: &Location,
    remaining_depth: usize,
) -> Result<(), TransferError> {
    RemoteRemoval {
        sftp,
        progress: None,
        partial: false,
    }
    .tree(location, remaining_depth)
}

struct RemoteRemoval<'a> {
    sftp: &'a Sftp,
    progress: Option<&'a mut Progress>,
    partial: bool,
}

impl RemoteRemoval<'_> {
    fn tree(&mut self, location: &Location, remaining_depth: usize) -> Result<(), TransferError> {
        let result = (|| {
            if let Some(progress) = &mut self.progress {
                progress.checkpoint()?;
                progress.results.removing(location);
                progress.item(location.path());
            }
            self.remove(location, remaining_depth)
        })();
        if let Some(progress) = &mut self.progress {
            progress.results.removed(location, &result);
        }
        result
    }

    fn remove(&mut self, location: &Location, remaining_depth: usize) -> Result<(), TransferError> {
        let path = location.path();
        let stat = self
            .sftp
            .lstat(path)
            .map_err(|source| MutationError::RemoveSftp {
                location: location.clone(),
                action: SftpRemovalAction::Inspect,
                source,
            })?;
        if stat.is_dir() {
            let entries = self.sftp.readdir(path).map_err(|error| {
                ListingError::new(location.clone(), ListingErrorKind::ReadSftp(error))
            })?;
            for (child, _) in entries {
                let child_depth = remaining_depth
                    .checked_sub(1)
                    .ok_or_else(|| MutationError::SftpRemovalDepth(location.clone()))?;
                let name = child.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
                    ListingError::new(
                        location.clone(),
                        ListingErrorKind::SftpNameEncoding(child.clone()),
                    )
                })?;
                validate_name(name).map_err(|error| {
                    ListingError::new(location.clone(), ListingErrorKind::InvalidSftpName(error))
                })?;
                let result = self.tree(&location.join(name), child_depth);
                self.partial |= result.is_ok()
                    || matches!(result, Err(TransferError::PublicationUnconfirmed(_)));
                result?;
            }
        }
        if let Some(progress) = &mut self.progress {
            progress.checkpoint()?;
            progress.item(path);
        }
        let (result, action) = if stat.is_dir() {
            (self.sftp.rmdir(path), SftpRemovalAction::RemoveDirectory)
        } else {
            (self.sftp.unlink(path), SftpRemovalAction::RemoveFile)
        };
        result.map_err(|source| {
            // A negative server status confirms rejection. A transport error
            // can also occur after the server has applied the request.
            let rejected = matches!(source.code(), ErrorCode::SFTP(code) if code > 0
                && code != libssh2_sys::LIBSSH2_FX_NO_CONNECTION
                && code != libssh2_sys::LIBSSH2_FX_CONNECTION_LOST);
            let error = MutationError::RemoveSftp {
                location: location.clone(),
                action,
                source,
            };
            if rejected {
                error.into()
            } else {
                TransferError::PublicationUnconfirmed(error.into())
            }
        })
    }
}

#[derive(Debug)]
pub(crate) struct DeletionFailure {
    pub(crate) error: TransferError,
    pub(crate) completed: usize,
    pub(crate) partial_remote: bool,
}

impl From<TransferError> for DeletionFailure {
    fn from(error: TransferError) -> Self {
        Self {
            error,
            completed: 0,
            partial_remote: false,
        }
    }
}

#[cfg(test)]
pub fn trash_entries(paths: &[Location]) -> Result<usize, DeletionFailure> {
    trash_entries_with_progress(paths, &mut Progress::default())
}

pub fn trash_entries_with_progress(
    paths: &[Location],
    progress: &mut Progress,
) -> Result<usize, DeletionFailure> {
    progress
        .results
        .plan(paths.iter().map(|path| (path.clone(), None)));
    if let Some(path) = paths.iter().find(|path| path.is_archive()) {
        return Err(TransferError::from(MutationError::ReadOnlyArchive(path.clone())).into());
    }
    if paths.iter().any(Location::remote) {
        if let Some(path) = paths.iter().find(|p| {
            !p.remote()
                || p.parent().is_none()
                || !normalize_remote(p.path()).is_ok_and(|canonical| p.path() == canonical)
        }) {
            return Err(
                TransferError::from(MutationError::InvalidSftpRemoval(path.clone())).into(),
            );
        }
        for (completed, path) in paths.iter().enumerate() {
            progress.checkpoint().map_err(|error| DeletionFailure {
                error: error.into(),
                completed,
                partial_remote: false,
            })?;
            progress.results.begin(path);
            progress.item(path.path());
            let sftp = remote(path).map_err(|reason| DeletionFailure {
                error: MutationError::OpenSftpRemoval {
                    location: path.clone(),
                    reason,
                }
                .into(),
                completed,
                partial_remote: false,
            })?;
            let mut removal = RemoteRemoval {
                sftp: &sftp,
                progress: Some(progress),
                partial: false,
            };
            removal
                .tree(path, MAX_TREE_DEPTH)
                .map_err(|error| DeletionFailure {
                    error,
                    completed,
                    partial_remote: removal.partial,
                })?;
        }
        return Ok(paths.len());
    }
    fs_ops::trash_entries(
        &paths.iter().map(|p| p.path().into()).collect::<Vec<_>>(),
        progress,
    )
}
