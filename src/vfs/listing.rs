use super::err;
use super::location::{validate_name, Location};
use super::sftp::{cached_channel, remote, sessions};
use crate::{archive, fs_ops};
use ssh2::{ErrorCode, File, FileStat, Sftp};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: Location,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub metadata: fs_ops::ListingMetadata,
}

impl From<fs_ops::FileEntry> for FileEntry {
    fn from(entry: fs_ops::FileEntry) -> Self {
        Self {
            path: entry.path.into(),
            name: entry.name,
            is_dir: entry.is_dir,
            is_symlink: entry.is_symlink,
            size: entry.size,
            modified: entry.modified,
            metadata: entry.metadata,
        }
    }
}

pub fn visit_directory(
    location: &Location,
    cancelled: &impl Fn() -> bool,
    mut visit: impl FnMut(FileEntry),
) -> Result<(), String> {
    if cancelled() {
        return Ok(());
    }
    if let Some(path) = location.local() {
        return fs_ops::visit_directory(path, cancelled, |entry| visit(entry.into()));
    }
    if let Some(path) = location.archive_source() {
        for entry in archive::Index::load(path, cancelled)?.list(location, cancelled)? {
            visit(entry);
        }
        return Ok(());
    }
    visit_sftp_directory(location, &remote(location)?, cancelled, visit)
}

pub fn read_directory(
    location: &Location,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<FileEntry>, String> {
    if cancelled() {
        return Ok(Vec::new());
    }
    if let Some(path) = location.local() {
        return fs_ops::read_directory(path, cancelled)
            .map(|entries| entries.into_iter().map(FileEntry::from).collect());
    }
    if let Some(path) = location.archive_source() {
        return archive::Index::load(path, cancelled)?.list(location, cancelled);
    }
    read_sftp_directory(location, &remote(location)?, cancelled)
}

/// Poll existing sessions only. A busy poll or unavailable session leaves the
/// previous listing intact and can be retried on the next scheduled refresh.
pub fn read_automatic_directory(
    location: &Location,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<Vec<FileEntry>>, String> {
    if cancelled() {
        return Ok(None);
    }
    let Location::Sftp { endpoint, .. } = location else {
        return read_directory(location, cancelled).map(Some);
    };
    // A poll superseded by navigation may still be in SSH I/O. Keep at most
    // one remote poll in flight across both panes and tabs, without queuing.
    static REMOTE_POLL: Mutex<()> = Mutex::new(());
    let _poll = match REMOTE_POLL.try_lock() {
        Ok(poll) => poll,
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(error) => return Err(err(error)),
    };
    let session = match sessions().try_lock() {
        Ok(sessions) => sessions.get(endpoint).cloned(),
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(error) => return Err(err(error)),
    };
    let Some(session) = session else {
        return Ok(None);
    };
    let Some(channel) = cached_channel(endpoint, &session)? else {
        return Ok(None);
    };
    read_sftp_directory(location, &channel, cancelled).map(Some)
}

/// Browse an exact existing session without initiating authentication.
pub fn visit_connected_directory(
    location: &Location,
    cancelled: &impl Fn() -> bool,
    visit: impl FnMut(FileEntry),
) -> Result<(), String> {
    if cancelled() {
        return Ok(());
    }
    let Location::Sftp { endpoint, .. } = location else {
        return visit_directory(location, cancelled, visit);
    };
    let session = sessions().lock().map_err(err)?.get(endpoint).cloned();
    if cancelled() {
        return Ok(());
    }
    let channel = match session {
        Some(session) => cached_channel(endpoint, &session)?,
        None => None,
    };
    let channel = channel.ok_or_else(|| {
        crate::i18n::text("Для просмотра дерева подключитесь по SFTP.").to_owned()
    })?;
    visit_sftp_directory(location, &channel, cancelled, visit)
}

/// Completion may browse an existing session without initiating authentication.
pub fn visit_address_directory(
    location: &Location,
    allow_unique_session: bool,
    cancelled: &impl Fn() -> bool,
    visit: impl FnMut(FileEntry),
) -> Result<(), String> {
    if cancelled() {
        return Ok(());
    }
    let Location::Sftp { endpoint, path } = location else {
        return visit_directory(location, cancelled, visit);
    };
    let connected = {
        let sessions = sessions().lock().map_err(err)?;
        let exact = sessions.get_key_value(endpoint);
        let matched = exact.or_else(|| {
            if !allow_unique_session {
                return None;
            }
            // A URL carries neither a key path nor an SSH route.
            let mut matching = sessions
                .iter()
                .filter(|(candidate, _)| candidate.same_route(endpoint));
            let only = matching.next()?;
            matching.next().is_none().then_some(only)
        });
        matched.map(|(endpoint, session)| (endpoint.clone(), session.clone()))
    };
    if let Some((endpoint, session)) = connected.filter(|_| !cancelled())
        && let Some(channel) = cached_channel(&endpoint, &session)?
    {
        let location = Location::Sftp {
            endpoint,
            path: path.clone(),
        };
        visit_sftp_directory(&location, &channel, cancelled, visit)?;
    }
    Ok(())
}

pub(super) fn read_sftp_directory(
    location: &Location,
    sftp: &Sftp,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<FileEntry>, String> {
    let mut entries = Vec::new();
    visit_sftp_directory(location, sftp, cancelled, |entry| entries.push(entry))?;
    Ok(entries)
}

pub(super) fn visit_sftp_directory(
    location: &Location,
    sftp: &Sftp,
    cancelled: &impl Fn() -> bool,
    mut visit: impl FnMut(FileEntry),
) -> Result<(), String> {
    if cancelled() {
        return Ok(());
    }
    let mut dir = sftp.opendir(location.path()).map_err(err)?;
    while !cancelled() {
        let Some((filename, stat)) = next_sftp_entry(&mut dir).map_err(err)? else {
            break;
        };
        if cancelled() {
            break;
        }
        if filename == Path::new(".") || filename == Path::new("..") {
            continue;
        }
        let name = filename
            .to_str()
            .ok_or("Имя файла SFTP не является UTF-8")?;
        validate_name(name)?;
        let path = location.join(name);
        let is_symlink = stat.file_type().is_symlink();
        let is_dir = stat.is_dir()
            || (is_symlink && sftp.stat(path.path()).map(|s| s.is_dir()).unwrap_or(false));
        visit(FileEntry {
            path,
            name: name.into(),
            is_dir,
            is_symlink,
            size: stat.size.unwrap_or(0),
            modified: stat
                .mtime
                .and_then(|s| UNIX_EPOCH.checked_add(Duration::from_secs(s))),
            metadata: fs_ops::ListingMetadata {
                mode: stat.perm.map(|mode| (mode & 0o7777) as u16),
                owner_id: stat.uid,
                created: None,
                accessed: stat
                    .atime
                    .and_then(|s| UNIX_EPOCH.checked_add(Duration::from_secs(s))),
                local_file: None,
            },
        });
    }
    Ok(())
}

pub(super) fn next_sftp_entry(file: &mut File) -> Result<Option<(PathBuf, FileStat)>, ssh2::Error> {
    match file.readdir() {
        Ok(entry) => Ok(Some(entry)),
        // ssh2 reports directory EOF as LIBSSH2_ERROR_FILE.
        Err(error) if error.code() == ErrorCode::Session(libssh2_sys::LIBSSH2_ERROR_FILE) => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
