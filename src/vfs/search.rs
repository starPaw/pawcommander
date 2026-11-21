use super::listing::next_sftp_entry;
use super::location::{normalize_remote, validate_name, Endpoint, Location};
use super::sftp::{cached, should_reopen};
use crate::file_error::{
    AccessAction, AccessError, ListingError, ListingErrorKind, ScanError, SearchError,
};
use crate::operations::TransferError;
use libssh2_sys::LIBSSH2_FX_NO_SUCH_FILE;
use ssh2::{Error, ErrorCode, FileStat, Sftp};
use std::cell::Cell;
use std::collections::VecDeque;
use std::path::Path;
use std::time::Instant;

pub(crate) const MAX_ENTRIES: usize = 20_000;
// Bound the pending directory queue even when entries have very long names.
const MAX_PENDING_PATH_BYTES: usize = 16 * 1024 * 1024;

pub(crate) struct SftpSearchConnection {
    endpoint: Endpoint,
    channel: Sftp,
}

impl SftpSearchConnection {
    pub(crate) fn open(location: &Location) -> Result<Self, TransferError> {
        let unavailable = || SearchError::RequiresSftpConnection(location.clone());
        let endpoint = location.sftp_endpoint().ok_or_else(unavailable)?;
        let channel = cached(endpoint)
            .map_err(|error| AccessError::new(location, AccessAction::OpenFile, error))?
            .ok_or_else(unavailable)?;
        Ok(Self {
            endpoint: endpoint.clone(),
            channel,
        })
    }

    pub(crate) fn directory_at(
        &self,
        location: &Location,
        follow_link: bool,
    ) -> Result<Option<bool>, TransferError> {
        if location.sftp_endpoint() != Some(&self.endpoint) {
            return Err(SearchError::DifferentSftpConnection(location.clone()).into());
        }
        let path = location.path();
        let check_error =
            |error| AccessError::new(location, AccessAction::CheckSearchResult, error);
        let entry = match self.channel.lstat(path) {
            Ok(entry) => entry,
            Err(error) if error.code() == ErrorCode::SFTP(LIBSSH2_FX_NO_SUCH_FILE) => {
                return Ok(None)
            }
            Err(error) => return Err(check_error(error).into()),
        };
        if follow_link && entry.file_type().is_symlink() {
            return self
                .channel
                .stat(path)
                .map(|stat| Some(stat.is_dir()))
                .map_err(|error| check_error(error).into());
        }
        Ok(Some(entry.is_dir()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Visit {
    Continue,
    Prune,
    Stop,
}

#[derive(Default)]
pub(crate) struct Coverage {
    pub(crate) timed_out: bool,
    pub(crate) entries_limited: bool,
    pub(crate) depth_limited: bool,
    pub(crate) disconnected: bool,
    pub(crate) unreadable: usize,
    pub(crate) failure: Option<TransferError>,
}

impl Coverage {
    fn record_skip(&mut self, failure: impl FnOnce() -> TransferError) {
        self.unreadable += 1;
        self.failure.get_or_insert_with(failure);
    }

    fn record_error(&mut self, error: Error, failure: impl FnOnce(Error) -> TransferError) {
        self.disconnected = should_reopen(&error);
        if self.disconnected {
            // Report the cause that ended the walk even if an earlier branch failed.
            self.unreadable += 1;
            self.failure = Some(failure(error));
        } else {
            self.record_skip(|| failure(error));
        }
    }
}

pub(crate) fn walk(
    folder: &Location,
    max_depth: usize,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    mut visit: impl FnMut(&Location, &FileStat) -> Visit,
) -> Result<Coverage, TransferError> {
    let timed_out = Cell::new(false);
    let can_read = || {
        if Instant::now() >= deadline {
            timed_out.set(true);
        }
        !cancelled() && !timed_out.get()
    };
    let mut coverage = Coverage::default();
    if can_read() {
        let connection = SftpSearchConnection::open(folder);
        if can_read() {
            let connection = connection?;
            let canonical = connection.channel.realpath(folder.path());
            if can_read() {
                let canonical = canonical
                    .map_err(|error| AccessError::new(folder, AccessAction::ResolvePath, error))?;
                let path = normalize_remote(&canonical)?;
                let canonical = folder.with_path(path);
                coverage = walk_from(
                    &connection.channel,
                    canonical,
                    max_depth,
                    &can_read,
                    &mut visit,
                )?;
            }
        }
    }
    if cancelled() {
        return Err(TransferError::Cancelled);
    }
    coverage.timed_out = timed_out.get();
    Ok(coverage)
}

fn walk_from(
    channel: &Sftp,
    root: Location,
    max_depth: usize,
    can_read: &impl Fn() -> bool,
    visit: &mut impl FnMut(&Location, &FileStat) -> Visit,
) -> Result<Coverage, TransferError> {
    let mut coverage = Coverage::default();
    let mut pending_bytes = root.path().as_os_str().len();
    let mut pending = VecDeque::from([(root, 1)]);
    let mut seen = 0;
    // Keep only one directory handle open. A stopped scan may have to wait for
    // the in-flight SFTP request before dropping it on the shared SSH session.
    'walk: while let Some((location, depth)) = pending.pop_front() {
        if !can_read() {
            break;
        }
        let path = location.path();
        pending_bytes -= path.as_os_str().len();
        if depth > 1 {
            // SFTP paths are resolved by the server. Recheck the branch before
            // opening it in case it has been replaced by a symbolic link.
            let resolved = channel.realpath(path);
            if !can_read() {
                break;
            }
            match resolved {
                Ok(actual) if actual.as_os_str() == path.as_os_str() => {}
                Ok(actual) => {
                    coverage.record_skip(|| {
                        SearchError::DirectoryChanged {
                            location: location.clone(),
                            resolved: actual,
                        }
                        .into()
                    });
                    continue;
                }
                Err(error) => {
                    coverage.record_error(error, |error| {
                        AccessError::new(&location, AccessAction::ResolvePath, error).into()
                    });
                    if coverage.disconnected {
                        break;
                    }
                    continue;
                }
            }
        }
        let directory = channel.opendir(path);
        if !can_read() {
            break;
        }
        let open_error =
            |error| ListingError::new(location.clone(), ListingErrorKind::OpenSftp(error)).into();
        let mut directory = match directory {
            Ok(directory) => directory,
            Err(error) if depth == 1 => return Err(open_error(error)),
            Err(error) => {
                coverage.record_error(error, open_error);
                if coverage.disconnected {
                    break;
                }
                continue;
            }
        };
        while can_read() {
            if seen == MAX_ENTRIES {
                coverage.entries_limited = true;
                break 'walk;
            }
            let result = next_sftp_entry(&mut directory);
            if !can_read() {
                break 'walk;
            }
            let (name, stat) = match result {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    coverage.record_error(error, |error| {
                        ListingError::new(location.clone(), ListingErrorKind::ReadSftp(error))
                            .into()
                    });
                    if coverage.disconnected {
                        break 'walk;
                    }
                    break;
                }
            };
            seen += 1;
            if name == Path::new(".") || name == Path::new("..") {
                continue;
            }
            let Some(text) = name.to_str() else {
                coverage.record_skip(|| {
                    ListingError::new(location.clone(), ListingErrorKind::SftpNameEncoding(name))
                        .into()
                });
                continue;
            };
            if let Err(error) = validate_name(text) {
                coverage.record_skip(|| {
                    ListingError::new(location.clone(), ListingErrorKind::InvalidSftpName(error))
                        .into()
                });
                continue;
            }
            let child = location.join(text);
            match visit(&child, &stat) {
                Visit::Prune => continue,
                Visit::Stop => break 'walk,
                Visit::Continue => {}
            }
            if stat.perm.is_none() {
                coverage.record_skip(|| ScanError::MissingSftpFileType(child.clone()).into());
            }
            if stat.is_dir() {
                if depth >= max_depth {
                    coverage.depth_limited = true;
                } else if pending_bytes + child.path().as_os_str().len() <= MAX_PENDING_PATH_BYTES {
                    pending_bytes += child.path().as_os_str().len();
                    pending.push_back((child, depth + 1));
                } else {
                    coverage.entries_limited = true;
                }
            }
        }
    }
    Ok(coverage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::Failure;

    #[test]
    fn terminal_disconnect_keeps_its_original_error_after_a_skipped_branch() {
        let mut coverage = Coverage::default();
        let folder = Location::Local("/fixture".into());
        let format =
            |error| ListingError::new(folder.clone(), ListingErrorKind::ReadSftp(error)).into();
        coverage.record_error(
            Error::from_errno(ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED)),
            format,
        );
        let disconnected = ErrorCode::Session(libssh2_sys::LIBSSH2_ERROR_SOCKET_DISCONNECT);
        coverage.record_error(Error::from_errno(disconnected), format);
        assert_eq!(coverage.unreadable, 2);
        assert!(coverage.disconnected);
        assert!(
            matches!(coverage.failure, Some(TransferError::Failed(Failure::Listing(error)))
            if error.location == folder && matches!(&error.kind, ListingErrorKind::ReadSftp(cause)
                if cause.code() == disconnected))
        );
    }
}
