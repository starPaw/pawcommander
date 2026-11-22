use crate::{
    file_error::{AccessAction, AccessError, ListingError, ListingErrorKind, ScanError},
    operations::{Cancellation, TransferError},
    vfs::{
        listing::next_sftp_entry, read_scope::ReadScope, sftp::remote,
        transfer::remote_source_matches, Location,
    },
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
    Unreadable(TransferError),
}

pub(in crate::vfs) struct FileInfo {
    pub(in crate::vfs) size: Option<u64>,
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
            Self::Unreadable(error) => super::diagnostic_bytes(error),
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
        return visit_local(scope, root, relative, cancel, visitor);
    }
    let folder = scope
        .checked_directory(relative, cancel)?
        .ok_or_else(|| ScanError::FolderUnavailable(scope.parent.location.join(relative)))?;
    if let Some(path) = folder.local() {
        visit_path(path, cancel, visitor)
    } else {
        visit_remote(&folder, cancel, visitor)
    }
}

#[cfg(unix)]
fn visit_local(
    scope: &ReadScope,
    root: &std::os::fd::OwnedFd,
    relative: &Path,
    cancel: &Cancellation,
    visit: Visitor<'_>,
) -> Result<(), TransferError> {
    use crate::vfs::read_scope::{local_subdirectory, visit_local_entries};
    use rustix::fs::{readlinkat, FileType};
    use std::os::unix::ffi::OsStrExt;

    let folder = &scope.parent.location.join(relative);
    #[cfg(target_os = "macos")]
    scope.verify_mount()?;
    let nested = local_subdirectory(root, relative, cancel)
        .map_err(|error| AccessError::new(folder, AccessAction::ReadChildren, error))?;
    let descriptor = nested.as_ref().unwrap_or(root);
    #[cfg(target_os = "macos")]
    let mount_device = scope
        .owned_smb_mount()
        .map(|mount| {
            mount.verify_directory(descriptor, folder.path(), folder.path())?;
            let metadata = rustix::fs::fstat(descriptor).map_err(|error| {
                AccessError::new(
                    folder,
                    AccessAction::ReadOpenMetadata,
                    std::io::Error::from(error),
                )
            })?;
            Ok::<_, TransferError>((mount, metadata.st_dev))
        })
        .transpose()?;
    visit_local_entries(folder, descriptor, cancel, |name, stat| {
        #[cfg(target_os = "macos")]
        if let (Some((mount, device)), Ok(stat)) = (mount_device, &stat)
            && stat.st_dev != device
        {
            let error = mount.changed(folder.join(name).path());
            return visit(name.to_owned(), Entry::Unreadable(error));
        }
        let entry = match stat {
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
                    Err(error) => Entry::Unreadable(
                        AccessError::new(
                            &folder.join(name),
                            AccessAction::ReadLink,
                            std::io::Error::from(error),
                        )
                        .into(),
                    ),
                },
                _ => unsupported(folder.join(name)),
            },
            Err(error) => Entry::Unreadable(
                AccessError::new(
                    &folder.join(name),
                    AccessAction::ReadMetadata,
                    std::io::Error::from(error),
                )
                .into(),
            ),
        };
        visit(name.to_owned(), entry)
    })
}

fn visit_remote(
    folder: &Location,
    cancel: &Cancellation,
    visit: Visitor<'_>,
) -> Result<(), TransferError> {
    let sftp = remote(folder)
        .map_err(|error| AccessError::new(folder, AccessAction::ReadChildren, error))?;
    let metadata = || -> Result<_, TransferError> {
        sftp.lstat(folder.path())
            .map_err(|error| AccessError::new(folder, AccessAction::ReadMetadata, error).into())
    };
    let before = metadata()?;
    if !before.is_dir() {
        return Err(ScanError::FolderUnavailable(folder.clone()).into());
    }
    let mut reader = sftp
        .opendir(folder.path())
        .map_err(|error| ListingError::new(folder.clone(), ListingErrorKind::OpenSftp(error)))?;
    loop {
        cancel.check()?;
        let Some((name, stat)) = next_sftp_entry(&mut reader).map_err(|error| {
            ListingError::new(folder.clone(), ListingErrorKind::ReadSftp(error))
        })?
        else {
            break;
        };
        if name == Path::new(".") || name == Path::new("..") {
            continue;
        }
        let mut parts = name.components();
        let (Some(Component::Normal(segment)), None) = (parts.next(), parts.next()) else {
            return Err(ScanError::InvalidSftpEntry {
                directory: folder.clone(),
                name,
            }
            .into());
        };
        if name.as_os_str() != segment {
            return Err(ScanError::InvalidSftpEntry {
                directory: folder.clone(),
                name,
            }
            .into());
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
                Err(error) => Entry::Unreadable(
                    AccessError::new(&location, AccessAction::ReadLink, error).into(),
                ),
            }
        } else {
            unsupported(folder.join(segment))
        };
        visit(segment.to_owned(), entry)?;
    }
    drop(reader);
    cancel.check()?;
    let after = metadata()?;
    if !remote_source_matches(&before, &after) {
        return Err(ScanError::FolderUnavailable(folder.clone()).into());
    }
    Ok(())
}

fn visit_path(path: &Path, cancel: &Cancellation, visit: Visitor<'_>) -> Result<(), TransferError> {
    let folder = Location::Local(path.to_owned());
    let metadata_error = |error| AccessError::new(&folder, AccessAction::ReadMetadata, error);
    let before = fs::symlink_metadata(path).map_err(metadata_error)?;
    if !before.is_dir() {
        return Err(ScanError::FolderUnavailable(folder).into());
    }
    for item in fs::read_dir(path)
        .map_err(|error| ListingError::new(folder.clone(), ListingErrorKind::OpenLocal(error)))?
    {
        cancel.check()?;
        let item = item.map_err(|error| {
            ListingError::new(folder.clone(), ListingErrorKind::ReadLocalEntry(error))
        })?;
        let path = item.path();
        let entry = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => Entry::Directory,
            Ok(metadata) if metadata.is_file() => Entry::File(FileInfo {
                size: Some(metadata.len()),
                modified: metadata.modified().ok().map(Modified::from_time),
            }),
            Ok(metadata) if metadata.is_symlink() => match fs::read_link(&path) {
                Ok(target) => Entry::Link(target),
                Err(error) => Entry::Unreadable(
                    AccessError::new(&Location::Local(path), AccessAction::ReadLink, error).into(),
                ),
            },
            Ok(_) => unsupported(Location::Local(path)),
            Err(error) => Entry::Unreadable(
                AccessError::new(&Location::Local(path), AccessAction::ReadMetadata, error).into(),
            ),
        };
        visit(item.file_name(), entry)?;
    }
    cancel.check()?;
    let after = fs::symlink_metadata(path).map_err(metadata_error)?;
    if !crate::fs_ops::source_metadata_matches(&before, &after).map_err(metadata_error)? {
        return Err(ScanError::FolderUnavailable(folder).into());
    }
    Ok(())
}

fn unsupported(location: Location) -> Entry {
    Entry::Unreadable(ScanError::UnsupportedComparisonType(location).into())
}
