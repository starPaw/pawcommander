use super::listing::next_sftp_entry;
use super::location::{normalize_remote, validate_name, Endpoint, Location};
use super::sftp::{cached, should_reopen};
use crate::i18n::text;
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
    pub(crate) fn open(location: &Location) -> Result<Self, String> {
        let unavailable = || text("Для поиска подключитесь к серверу по SFTP.").to_owned();
        let endpoint = location.sftp_endpoint().ok_or_else(unavailable)?;
        let channel = cached(endpoint)?.ok_or_else(unavailable)?;
        Ok(Self {
            endpoint: endpoint.clone(),
            channel,
        })
    }

    pub(crate) fn directory_at(
        &self,
        location: &Location,
        follow_link: bool,
    ) -> Result<Option<bool>, String> {
        if location.sftp_endpoint() != Some(&self.endpoint) {
            return Err(text("Результаты относятся к разным подключениям SFTP.").into());
        }
        let path = location.path();
        let entry = match self.channel.lstat(path) {
            Ok(entry) => entry,
            Err(error) if error.code() == ErrorCode::SFTP(LIBSSH2_FX_NO_SUCH_FILE) => {
                return Ok(None)
            }
            Err(error) => return Err(error.to_string()),
        };
        if follow_link && entry.file_type().is_symlink() {
            return self
                .channel
                .stat(path)
                .map(|stat| Some(stat.is_dir()))
                .map_err(super::err);
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
}

impl Coverage {
    fn record_error(&mut self, error: &Error) {
        self.disconnected = should_reopen(error);
        self.unreadable += 1;
    }
}

pub(crate) fn walk(
    folder: &Location,
    max_depth: usize,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    mut visit: impl FnMut(&Location, &FileStat) -> Visit,
) -> Result<Coverage, String> {
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
                let canonical =
                    folder.with_path(normalize_remote(&canonical.map_err(super::err)?)?);
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
        return Err(text("Поиск отменён").into());
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
) -> Result<Coverage, String> {
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
            match channel.realpath(path) {
                Ok(actual) if actual.as_os_str() == path.as_os_str() => {}
                Ok(_) => {
                    coverage.unreadable += 1;
                    continue;
                }
                Err(error) => {
                    coverage.record_error(&error);
                    if coverage.disconnected {
                        break;
                    }
                    continue;
                }
            }
            if !can_read() {
                break;
            }
        }
        let mut directory = match channel.opendir(path) {
            Ok(directory) => directory,
            Err(_) if !can_read() => break,
            Err(error) if depth == 1 => return Err(error.to_string()),
            Err(error) => {
                coverage.record_error(&error);
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
                    coverage.record_error(&error);
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
            let Some(name) = name.to_str().filter(|name| validate_name(name).is_ok()) else {
                coverage.unreadable += 1;
                continue;
            };
            let child = location.join(name);
            match visit(&child, &stat) {
                Visit::Prune => continue,
                Visit::Stop => break 'walk,
                Visit::Continue => {}
            }
            if stat.perm.is_none() {
                coverage.unreadable += 1;
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
