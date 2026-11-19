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
            return Err(text("Сервер не передал размер или время изменения файла").into());
        }
        let entry = TransferSourceEntry {
            metadata,
            children: None,
        };
        let output = read(&source, entry.size(), &mut input)?;
        cancel.check()?;
        entry
            .verify_metadata(&source, &input.metadata()?)
            .map_err(|_| changed_during_read())?;
        let Some(mut current) = self.input(path, cancel)? else {
            return Err(changed_during_read());
        };
        entry
            .verify_metadata(&source, &current.metadata()?)
            .map_err(|_| changed_during_read())?;
        Ok(Some(output))
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
            text("Для проверки подходят только обычные файлы без ссылок"),
        ));
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    openat(parent, name, flags, Mode::empty())
        .map(std::fs::File::from)
        .map_err(io::Error::from)
}

pub(super) fn changed_during_read() -> TransferError {
    text("Файл изменился во время чтения").into()
}

fn unsupported() -> TransferError {
    text("Для проверки подходят только обычные файлы без ссылок").into()
}
