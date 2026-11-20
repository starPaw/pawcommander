use crate::{
    i18n::text,
    operations::{Cancellation, TransferError},
    vfs::{err, read_scope::ReadScope, sftp::remote, transfer::remote_source_matches, Location},
};
use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

pub(in crate::vfs) enum Entry {
    Directory,
    File(FileInfo),
    Link(PathBuf),
    Unreadable(String),
}

pub(in crate::vfs) struct FileInfo {
    pub(super) size: Option<u64>,
    pub(super) modified: Option<Modified>,
}

pub(super) struct Modified {
    seconds: i128,
    nanos: Option<u32>,
}

impl Modified {
    pub(super) fn matches(&self, other: &Self) -> bool {
        self.seconds == other.seconds && self.nanos.zip(other.nanos).is_none_or(|(a, b)| a == b)
    }

    fn from_time(time: std::time::SystemTime) -> Self {
        let (seconds, nanos) = match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => (i128::from(duration.as_secs()), duration.subsec_nanos()),
            Err(error) => {
                let duration = error.duration();
                let seconds = -i128::from(duration.as_secs());
                let nanos = duration.subsec_nanos();
                if nanos == 0 {
                    (seconds, 0)
                } else {
                    (seconds - 1, 1_000_000_000 - nanos)
                }
            }
        };
        Self {
            seconds,
            nanos: Some(nanos),
        }
    }
}

impl Entry {
    pub(in crate::vfs) fn extra_path_bytes(&self) -> usize {
        match self {
            Self::Link(path) => path.as_os_str().len(),
            Self::Unreadable(message) => message.len(),
            _ => 0,
        }
    }
}

type Visitor<'a> = &'a mut dyn FnMut(OsString, Entry) -> Result<(), TransferError>;

pub(in crate::vfs) fn visit(
    scope: &ReadScope,
    relative: &Path,
    cancel: &Cancellation,
    visitor: Visitor<'_>,
) -> Result<(), TransferError> {
    cancel.check()?;
    #[cfg(unix)]
    if let Some(root) = &scope.descriptor {
        return visit_local(root, relative, cancel, visitor);
    }
    let folder = scope
        .checked_directory(relative, cancel)?
        .ok_or_else(folder_changed)?;
    if let Some(path) = folder.local() {
        visit_path(path, cancel, visitor)
    } else {
        visit_remote(&folder, cancel, visitor)
    }
}

#[cfg(unix)]
fn visit_local(
    root: &std::os::fd::OwnedFd,
    relative: &Path,
    cancel: &Cancellation,
    visit: Visitor<'_>,
) -> Result<(), TransferError> {
    use crate::vfs::read_scope::local_subdirectory;
    use rustix::fs::{fstat, readlinkat, statat, AtFlags, Dir, FileType};
    use std::{io, os::unix::ffi::OsStrExt};

    let nested = local_subdirectory(root, relative, cancel)?;
    let descriptor = nested.as_ref().unwrap_or(root);
    let before = fstat(descriptor).map_err(io::Error::from)?;
    let entries = Dir::read_from(descriptor).map_err(io::Error::from)?;
    for item in entries {
        cancel.check()?;
        let item = item.map_err(io::Error::from)?;
        let name = OsStr::from_bytes(item.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        let entry = match statat(descriptor, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => Entry::Directory,
                FileType::RegularFile => Entry::File(FileInfo {
                    size: stat.st_size.try_into().ok(),
                    modified: Some(Modified {
                        seconds: i128::from(stat.st_mtime),
                        nanos: stat.st_mtime_nsec.try_into().ok(),
                    }),
                }),
                FileType::Symlink => match readlinkat(descriptor, name, Vec::new()) {
                    Ok(target) => Entry::Link(PathBuf::from(OsStr::from_bytes(target.to_bytes()))),
                    Err(error) => Entry::Unreadable(error.to_string()),
                },
                _ => unsupported(),
            },
            Err(error) => Entry::Unreadable(error.to_string()),
        };
        visit(name.to_owned(), entry)?;
    }
    cancel.check()?;
    let after = fstat(descriptor).map_err(io::Error::from)?;
    if before.st_mtime != after.st_mtime
        || before.st_mtime_nsec != after.st_mtime_nsec
        || before.st_ctime != after.st_ctime
        || before.st_ctime_nsec != after.st_ctime_nsec
    {
        return Err(folder_changed());
    }
    Ok(())
}

fn visit_remote(
    folder: &Location,
    cancel: &Cancellation,
    visit: Visitor<'_>,
) -> Result<(), TransferError> {
    use ssh2::ErrorCode;

    let sftp = remote(folder)?;
    let before = sftp.lstat(folder.path()).map_err(err)?;
    if !before.is_dir() {
        return Err(folder_changed());
    }
    let mut reader = sftp.opendir(folder.path()).map_err(err)?;
    loop {
        cancel.check()?;
        let (name, stat) = match reader.readdir() {
            Ok(entry) => entry,
            // ssh2::File::readdir reports EOF as LIBSSH2_ERROR_FILE.
            Err(error) if error.code() == ErrorCode::Session(-16) => break,
            Err(error) => return Err(error.to_string().into()),
        };
        if name == Path::new(".") || name == Path::new("..") {
            continue;
        }
        let mut parts = name.components();
        let (Some(Component::Normal(segment)), None) = (parts.next(), parts.next()) else {
            return Err(text("Папка содержит недопустимое имя").into());
        };
        if name.as_os_str() != segment {
            return Err(text("Папка содержит недопустимое имя").into());
        }
        let entry = if stat.is_dir() {
            Entry::Directory
        } else if stat.is_file() {
            Entry::File(FileInfo {
                size: stat.size,
                modified: stat.mtime.map(|seconds| Modified {
                    seconds: i128::from(seconds),
                    nanos: None,
                }),
            })
        } else if stat.file_type().is_symlink() {
            let location = folder.join(segment);
            match sftp.readlink(location.path()) {
                Ok(target) => Entry::Link(target),
                Err(error) => Entry::Unreadable(error.to_string()),
            }
        } else {
            unsupported()
        };
        visit(segment.to_owned(), entry)?;
    }
    drop(reader);
    cancel.check()?;
    let after = sftp.lstat(folder.path()).map_err(err)?;
    if !remote_source_matches(&before, &after) {
        return Err(folder_changed());
    }
    Ok(())
}

fn visit_path(path: &Path, cancel: &Cancellation, visit: Visitor<'_>) -> Result<(), TransferError> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_dir() {
        return Err(folder_changed());
    }
    for item in fs::read_dir(path)? {
        cancel.check()?;
        let item = item?;
        let path = item.path();
        let entry = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => Entry::Directory,
            Ok(metadata) if metadata.is_file() => Entry::File(FileInfo {
                size: Some(metadata.len()),
                modified: metadata.modified().ok().map(Modified::from_time),
            }),
            Ok(metadata) if metadata.is_symlink() => match fs::read_link(path) {
                Ok(target) => Entry::Link(target),
                Err(error) => Entry::Unreadable(error.to_string()),
            },
            Ok(_) => unsupported(),
            Err(error) => Entry::Unreadable(error.to_string()),
        };
        visit(item.file_name(), entry)?;
    }
    cancel.check()?;
    let after = fs::symlink_metadata(path)?;
    if !crate::fs_ops::source_metadata_matches(&before, &after)? {
        return Err(folder_changed());
    }
    Ok(())
}

pub(super) fn folder_changed() -> TransferError {
    text("Папка изменилась или недоступна").into()
}

fn unsupported() -> Entry {
    Entry::Unreadable(text("Тип объекта недоступен для сравнения").into())
}
