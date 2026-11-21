//! Bounded reads rooted at a folder, with identity and version checks.
use super::{
    transfer::{entry_metadata, EntryMetadata, SourceReader, TransferSourceEntry},
    undo::Parent,
    Location,
};
use crate::{
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
        self.stamp
            .verify_metadata(&self.source, &self.input.metadata()?)
            .map_err(|_| changed_during_read())?;
        let Some(mut current) = self.scope.input(self.path, cancel)? else {
            return Err(changed_during_read());
        };
        self.stamp
            .verify_metadata(&self.source, &current.metadata()?)
            .map_err(|_| changed_during_read())
    }
}

impl ReadScope {
    pub(super) fn open(location: &Location) -> Result<Self, TransferError> {
        if location.is_archive() {
            return Err(crate::archive::readonly().into());
        }
        let parent = Parent::capture(location)?;
        #[cfg(unix)]
        let descriptor = parent
            .location
            .local()
            .map(|path| {
                use rustix::fs::{open, Mode};
                open(path, directory_flags(), Mode::empty()).map_err(io::Error::from)
            })
            .transpose()?;
        Ok(Self {
            parent,
            #[cfg(unix)]
            descriptor,
        })
    }

    pub(super) fn digest(
        &self,
        path: &Path,
        progress: &mut Progress,
        cancel: &Cancellation,
    ) -> Result<Option<[u8; 32]>, TransferError> {
        self.read_checked(path, cancel, |source, size, input| {
            progress
                .hash_file(source.path(), size, input)?
                .ok_or_else(changed_during_read)
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
        let Some(mut input) = self.input(path, cancel)? else {
            return Ok(None);
        };
        let source = self.parent.location.join(path);
        let metadata = input.metadata()?;
        if !metadata.is_file() {
            return Err(unsupported());
        }
        if matches!(&metadata, EntryMetadata::Remote(stat) if stat.size.is_none() || stat.mtime.is_none())
        {
            return Err(text!("Сервер не передал размер или время изменения файла").into());
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
    ) -> Result<Option<SourceReader>, TransferError> {
        cancel.check()?;
        #[cfg(unix)]
        if let Some(root) = &self.descriptor {
            return match local_input(root, path, cancel) {
                Ok(file) => Ok(Some(SourceReader::Local(file))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            };
        }

        let parent = path.parent().ok_or_else(unsupported)?;
        let Some(directory) = self.checked_directory(parent, cancel)? else {
            return Ok(None);
        };
        let source = directory.join(path.file_name().ok_or_else(unsupported)?);
        let Some(metadata) = entry_metadata(&source)? else {
            return Ok(None);
        };
        if !metadata.is_file() {
            return Err(unsupported());
        }
        cancel.check()?;
        let mut input = SourceReader::open(&source)?;
        let entry = TransferSourceEntry {
            metadata,
            children: None,
        };
        entry
            .verify_metadata(&source, &input.metadata()?)
            .map_err(|_| changed_during_read())?;
        Ok(Some(input))
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
                return Err(unsupported());
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
                return Err(unsupported());
            }
        }
        Ok(Some(directory))
    }
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
    descriptor: &OwnedFd,
    cancel: &Cancellation,
    mut visit: impl FnMut(
        &std::ffi::OsStr,
        rustix::io::Result<rustix::fs::Stat>,
    ) -> Result<(), TransferError>,
) -> Result<(), TransferError> {
    use rustix::fs::{fstat, statat, AtFlags, Dir};
    use std::os::unix::ffi::OsStrExt;

    let before = fstat(descriptor).map_err(io::Error::from)?;
    let entries = Dir::read_from(descriptor).map_err(io::Error::from)?;
    for item in entries {
        cancel.check()?;
        let item = item.map_err(io::Error::from)?;
        let name = std::ffi::OsStr::from_bytes(item.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        visit(name, statat(descriptor, name, AtFlags::SYMLINK_NOFOLLOW))?;
    }
    cancel.check()?;
    let after = fstat(descriptor).map_err(io::Error::from)?;
    if before.st_mtime != after.st_mtime
        || before.st_mtime_nsec != after.st_mtime_nsec
        || before.st_ctime != after.st_ctime
        || before.st_ctime_nsec != after.st_ctime_nsec
    {
        return Err(text!("Папка изменилась или недоступна").into());
    }
    Ok(())
}

#[cfg(unix)]
fn local_input(root: &OwnedFd, path: &Path, cancel: &Cancellation) -> io::Result<std::fs::File> {
    use rustix::fs::{openat, statat, AtFlags, FileType, Mode, OFlags};
    let relative = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let directory = local_subdirectory(root, relative, cancel)?;
    let parent = directory.as_ref().unwrap_or(root);
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            text!("Для проверки подходят только обычные файлы без ссылок"),
        ));
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    openat(parent, name, flags, Mode::empty())
        .map(std::fs::File::from)
        .map_err(io::Error::from)
}

pub(super) fn changed_during_read() -> TransferError {
    text!("Файл изменился во время чтения").into()
}

fn unsupported() -> TransferError {
    text!("Для проверки подходят только обычные файлы без ссылок").into()
}
