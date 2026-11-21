//! Bounded reads rooted at a folder, with identity and version checks.
use super::{
    transfer::{entry_metadata, EntryMetadata, SourceReader, TransferSourceEntry},
    undo::Parent,
    Location,
};
use crate::{
    file_error::{
        AccessAction, AccessError, Failure, ListingError, ListingErrorKind, MutationError,
        ScanError,
    },
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
};
use std::path::{Component, Path};

#[cfg(unix)]
use std::{io, os::fd::OwnedFd};

pub(super) struct ReadScope {
    pub(super) parent: Parent,
    #[cfg(unix)]
    pub(super) descriptor: Option<OwnedFd>,
}

pub(super) struct CheckedFile<'a> {
    scope: &'a ReadScope,
    path: &'a Path,
    source: Location,
    stamp: TransferSourceEntry,
    input: SourceReader,
}

impl CheckedFile<'_> {
    #[cfg(target_os = "macos")]
    pub(super) fn stamp(&self) -> &TransferSourceEntry {
        &self.stamp
    }

    pub(super) fn stream(&mut self) -> (&Location, u64, &mut SourceReader) {
        (&self.source, self.stamp.size(), &mut self.input)
    }

    pub(super) fn verify(&mut self, cancel: &Cancellation) -> Result<(), TransferError> {
        cancel.check()?;
        verify_version(
            &self.stamp,
            &self.source,
            &self.input.metadata(&self.source)?,
        )?;
        let Some((source, mut current)) = self.scope.input(self.path, cancel)? else {
            return Err(ScanError::FileChanged(self.source.clone()).into());
        };
        verify_version(&self.stamp, &source, &current.metadata(&source)?)
    }
}

impl ReadScope {
    pub(super) fn open(location: &Location) -> Result<Self, TransferError> {
        if location.is_archive() {
            return Err(MutationError::ReadOnlyArchive(location.clone()).into());
        }
        let parent = Parent::capture(location).map_err(folder_error)?;
        #[cfg(unix)]
        let descriptor = parent
            .location
            .local()
            .map(|path| {
                use rustix::fs::{open, Mode};
                open(path, directory_flags(), Mode::empty()).map_err(|error| {
                    TransferError::from(ListingError::new(
                        parent.location.clone(),
                        ListingErrorKind::OpenLocal(error.into()),
                    ))
                })
            })
            .transpose()?;
        Ok(Self {
            parent,
            #[cfg(unix)]
            descriptor,
        })
    }

    pub(super) fn verify_root(&self) -> Result<(), TransferError> {
        self.parent.verify().map_err(folder_error)
    }

    pub(super) fn digest(
        &self,
        path: &Path,
        progress: &mut Progress,
        cancel: &Cancellation,
    ) -> Result<Option<[u8; 32]>, TransferError> {
        self.read_checked(path, cancel, |source, size, input| {
            progress
                .hash_file(source.path(), size, input)
                .map_err(|error| AccessError::read(source, error))?
                .ok_or_else(|| ScanError::FileChanged(source.clone()).into())
        })
    }

    pub(super) fn read_checked<T>(
        &self,
        path: &Path,
        cancel: &Cancellation,
        read: impl FnOnce(&Location, u64, &mut SourceReader) -> Result<T, TransferError>,
    ) -> Result<Option<T>, TransferError> {
        let Some(mut file) = self.open_file(path, cancel)? else {
            return Ok(None);
        };
        let (source, size, input) = file.stream();
        let output = read(source, size, input)?;
        file.verify(cancel)?;
        Ok(Some(output))
    }

    pub(super) fn open_file<'a>(
        &'a self,
        path: &'a Path,
        cancel: &Cancellation,
    ) -> Result<Option<CheckedFile<'a>>, TransferError> {
        let Some((source, mut input)) = self.input(path, cancel)? else {
            return Ok(None);
        };
        let metadata = input.metadata(&source)?;
        if !metadata.is_file() {
            return Err(ScanError::ExpectedRegularFile(source).into());
        }
        if matches!(&metadata, EntryMetadata::Remote(stat) if stat.size.is_none() || stat.mtime.is_none())
        {
            return Err(ScanError::MissingFileVersion(source).into());
        }
        Ok(Some(CheckedFile {
            scope: self,
            path,
            source,
            stamp: TransferSourceEntry {
                metadata,
                children: None,
            },
            input,
        }))
    }

    fn input(
        &self,
        path: &Path,
        cancel: &Cancellation,
    ) -> Result<Option<(Location, SourceReader)>, TransferError> {
        cancel.check()?;
        #[cfg(unix)]
        if let Some(root) = &self.descriptor {
            let source = self.parent.location.join(path);
            return match local_input(root, path, &source, cancel) {
                Ok(file) => Ok(Some((source, SourceReader::Local(file)))),
                Err(TransferError::Failed(Failure::Access(error))) if error.cause.is_absent() => {
                    Ok(None)
                }
                Err(error) => Err(error),
            };
        }

        let unsupported = || ScanError::ExpectedRegularFile(self.parent.location.join(path));
        let parent = path.parent().ok_or_else(unsupported)?;
        let Some(directory) = self.checked_directory(parent, cancel)? else {
            return Ok(None);
        };
        let source = directory.join(path.file_name().ok_or_else(unsupported)?);
        let Some(metadata) = entry_metadata(&source)? else {
            return Ok(None);
        };
        if !metadata.is_file() {
            return Err(ScanError::ExpectedRegularFile(source).into());
        }
        cancel.check()?;
        let mut input = SourceReader::open(&source)?;
        let entry = TransferSourceEntry {
            metadata,
            children: None,
        };
        verify_version(&entry, &source, &input.metadata(&source)?)?;
        Ok(Some((source, input)))
    }

    pub(super) fn checked_directory(
        &self,
        relative: &Path,
        cancel: &Cancellation,
    ) -> Result<Option<Location>, TransferError> {
        let mut directory = self.parent.location.clone();
        for component in relative.components() {
            cancel.check()?;
            let Component::Normal(name) = component else {
                return Err(
                    ScanError::FolderUnavailable(self.parent.location.join(relative)).into(),
                );
            };
            directory = directory.join(name);
            let Some(metadata) = entry_metadata(&directory)? else {
                return Ok(None);
            };
            let is_directory = match metadata {
                EntryMetadata::Local(meta) => meta.is_dir(),
                EntryMetadata::Remote(stat) => stat.is_dir(),
            };
            if !is_directory {
                return Err(ScanError::FolderUnavailable(directory).into());
            }
        }
        Ok(Some(directory))
    }
}

fn verify_version(
    stamp: &TransferSourceEntry,
    source: &Location,
    actual: &EntryMetadata,
) -> Result<(), TransferError> {
    let matches = stamp
        .matches_metadata(actual)
        .map_err(|error| AccessError::new(source, AccessAction::ReadOpenMetadata, error))?;
    if !matches {
        return Err(ScanError::FileChanged(source.clone()).into());
    }
    Ok(())
}

fn folder_error(error: TransferError) -> TransferError {
    if let TransferError::Failed(Failure::Mutation(reason)) = &error
        && let MutationError::UndoUnavailable(location) = reason.as_ref()
    {
        return ScanError::FolderUnavailable(location.clone()).into();
    }
    error
}

#[cfg(unix)]
fn directory_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

#[cfg(unix)]
pub(super) fn local_subdirectory(
    root: &OwnedFd,
    relative: &Path,
    cancel: &Cancellation,
) -> io::Result<Option<OwnedFd>> {
    use rustix::fs::{openat, Mode};

    let mut directory = None;
    for component in relative.components() {
        cancel.check()?;
        let Component::Normal(name) = component else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        directory = Some(
            openat(
                directory.as_ref().unwrap_or(root),
                name,
                directory_flags(),
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
    }
    Ok(directory)
}

#[cfg(unix)]
pub(super) fn visit_local_entries(
    folder: &Location,
    descriptor: &OwnedFd,
    cancel: &Cancellation,
    mut visit: impl FnMut(
        &std::ffi::OsStr,
        rustix::io::Result<rustix::fs::Stat>,
    ) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    use rustix::fs::{fstat, statat, AtFlags, Dir};
    use std::os::unix::ffi::OsStrExt;

    let read_metadata = || -> Result<_, TransferError> {
        fstat(descriptor).map_err(|error| {
            AccessError::new(
                folder,
                AccessAction::ReadOpenMetadata,
                io::Error::from(error),
            )
            .into()
        })
    };
    let before = read_metadata()?;
    let entries = Dir::read_from(descriptor).map_err(|error| {
        ListingError::new(folder.clone(), ListingErrorKind::OpenLocal(error.into()))
    })?;
    for item in entries {
        cancel.check()?;
        let item = item.map_err(|error| {
            ListingError::new(
                folder.clone(),
                ListingErrorKind::ReadLocalEntry(error.into()),
            )
        })?;
        let name = std::ffi::OsStr::from_bytes(item.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        visit(name, statat(descriptor, name, AtFlags::SYMLINK_NOFOLLOW))?;
    }
    cancel.check()?;
    let after = read_metadata()?;
    if before.st_mtime != after.st_mtime
        || before.st_mtime_nsec != after.st_mtime_nsec
        || before.st_ctime != after.st_ctime
        || before.st_ctime_nsec != after.st_ctime_nsec
    {
        return Err(ScanError::FolderUnavailable(folder.clone()).into());
    }
    Ok(())
}

#[cfg(unix)]
fn local_input(
    root: &OwnedFd,
    path: &Path,
    source: &Location,
    cancel: &Cancellation,
) -> Result<std::fs::File, TransferError> {
    use rustix::fs::{openat, statat, AtFlags, FileType, Mode, OFlags};
    let unsupported = || ScanError::ExpectedRegularFile(source.clone());
    let open_error = |error| AccessError::new(source, AccessAction::OpenFile, error);
    let relative = path.parent().ok_or_else(unsupported)?;
    let directory = local_subdirectory(root, relative, cancel).map_err(open_error)?;
    let parent = directory.as_ref().unwrap_or(root);
    let name = path.file_name().ok_or_else(unsupported)?;
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
        AccessError::new(source, AccessAction::ReadMetadata, io::Error::from(error))
    })?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(unsupported().into());
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    openat(parent, name, flags, Mode::empty())
        .map(std::fs::File::from)
        .map_err(|error| open_error(error.into()).into())
}

pub(super) fn changed_during_read() -> TransferError {
    text!("Файл изменился во время чтения").into()
}
