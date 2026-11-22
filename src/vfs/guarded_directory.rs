//! Directory identity checks and descriptor-relative, no-replacement publication.
use super::{
    read_scope::{local_subdirectory, ReadScope},
    transfer::{
        canonical, entry_metadata, rename_for_publication, EntryMetadata, LocalEntryMetadata,
    },
    Location,
};
use crate::file_error::{
    AccessAction, AccessError, LocalPathAction, MutationError, PublicationError, RecoveryNotice,
};
use crate::operations::{Cancellation, TransferError};
use std::{ffi::OsStr, fs, io, os::fd::OwnedFd, path::Path};

pub(super) struct Directory {
    pub(super) location: Location,
    pub(super) descriptor: Option<OwnedFd>,
    metadata: EntryMetadata,
}

impl Directory {
    pub(super) fn from_scope(scope: &ReadScope) -> Result<Self, TransferError> {
        let location = scope.parent.location.clone();
        let metadata = entry_metadata(&location)?.ok_or_else(|| stale(&location))?;
        Self::at(scope, Path::new(""), &metadata, &Cancellation::default())
    }

    pub(super) fn at(
        scope: &ReadScope,
        relative: &Path,
        expected: &EntryMetadata,
        cancel: &Cancellation,
    ) -> Result<Self, TransferError> {
        scope.parent.verify()?;
        let location = scope
            .checked_directory(relative, cancel)?
            .ok_or_else(|| stale(&scope.parent.location.join(relative)))?;
        let descriptor = scope
            .descriptor
            .as_ref()
            .map(|root| {
                local_subdirectory(root, relative, cancel)?
                    .map_or_else(|| rustix::io::dup(root).map_err(io::Error::from), Ok)
            })
            .transpose()
            .map_err(|error| AccessError::new(&location, AccessAction::OpenFile, error))?;
        let anchor = Self {
            location,
            descriptor,
            metadata: expected.clone(),
        };
        anchor.verify()?;
        if let Some(fd) = &anchor.descriptor {
            let captured = held_metadata(fd).map_err(|error| {
                AccessError::new(&anchor.location, AccessAction::ReadOpenMetadata, error)
            })?;
            if !expected.same_object(&captured) {
                return Err(stale(&anchor.location));
            }
        }
        Ok(anchor)
    }

    pub(super) fn verify(&self) -> Result<(), TransferError> {
        let actual = entry_metadata(&self.location)?.ok_or_else(|| stale(&self.location))?;
        if !actual.is_directory()
            || self.descriptor.is_some() && !self.metadata.same_object(&actual)
            || canonical(&self.location)? != self.location.path()
        {
            return Err(stale(&self.location));
        }
        Ok(())
    }

    pub(super) fn verify_absent(&self, name: &OsStr) -> Result<(), TransferError> {
        self.check_absent(name, |path| stale(&path))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn require_absent(&self, name: &OsStr) -> Result<(), TransferError> {
        self.check_absent(name, |path| MutationError::DestinationExists(path).into())
    }

    fn check_absent(
        &self,
        name: &OsStr,
        occupied: impl FnOnce(Location) -> TransferError,
    ) -> Result<(), TransferError> {
        self.verify()?;
        let path = self.location.join(name);
        if entry_metadata(&path)?.is_some() {
            return Err(occupied(path));
        }
        Ok(())
    }

    pub(super) fn exclusive_file(&self, name: &OsStr) -> Result<fs::File, TransferError> {
        use rustix::fs::{openat, Mode, OFlags};

        self.verify()?;
        let descriptor = self
            .descriptor
            .as_ref()
            .ok_or_else(|| stale(&self.location))?;
        let file = openat(
            descriptor,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| {
            AccessError::new(
                &self.location.join(name),
                AccessAction::CreateFile,
                io::Error::from(error),
            )
        })?;
        Ok(fs::File::from(file))
    }

    pub(super) fn exclusive_directory(&self, name: &OsStr) -> Result<Self, TransferError> {
        use rustix::fs::{mkdirat, openat, Mode, OFlags};

        self.verify()?;
        let parent = self
            .descriptor
            .as_ref()
            .ok_or_else(|| stale(&self.location))?;
        let location = self.location.join(name);
        mkdirat(parent, name, Mode::from_raw_mode(0o700)).map_err(|error| {
            MutationError::LocalIo {
                path: location.path().to_owned(),
                action: LocalPathAction::CreateDirectory,
                source: error.into(),
            }
        })?;
        let result = (|| {
            let descriptor = openat(
                parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| {
                AccessError::new(&location, AccessAction::OpenFile, io::Error::from(error))
            })?;
            let metadata = held_metadata(&descriptor).map_err(|error| {
                AccessError::new(&location, AccessAction::ReadOpenMetadata, error)
            })?;
            let actual = canonical(&location)?;
            let child = Self {
                location: location.with_path(actual),
                descriptor: Some(descriptor),
                metadata,
            };
            child.verify()?;
            Ok::<_, TransferError>(child)
        })();
        result.map_err(|error| {
            TransferError::PublicationUnconfirmed(
                PublicationError::DirectoryUnverified {
                    target: location,
                    cause: error,
                }
                .into(),
            )
        })
    }

    pub(super) fn current_metadata(&self) -> Result<EntryMetadata, TransferError> {
        self.verify()?;
        let descriptor = self
            .descriptor
            .as_ref()
            .ok_or_else(|| stale(&self.location))?;
        held_metadata(descriptor).map_err(|error| {
            AccessError::new(&self.location, AccessAction::ReadOpenMetadata, error).into()
        })
    }

    pub(super) fn rename(
        &self,
        name: &OsStr,
        destination: &Directory,
        new_name: &OsStr,
    ) -> Result<(), TransferError> {
        let from = self.location.join(name);
        let to = destination.location.join(new_name);
        if let (Some(old_fd), Some(new_fd)) = (&self.descriptor, &destination.descriptor) {
            rustix::fs::renameat_with(
                old_fd,
                name,
                new_fd,
                new_name,
                rustix::fs::RenameFlags::NOREPLACE,
            )
            .map_err(|error| {
                crate::fs_ops::rename_error(from.path(), to.path(), io::Error::from(error))
            })
        } else {
            rename_for_publication(&from, &to)
        }
    }
}

fn held_metadata(descriptor: &OwnedFd) -> io::Result<EntryMetadata> {
    let fd = rustix::io::dup(descriptor).map_err(io::Error::from)?;
    let file = fs::File::from(fd);
    Ok(EntryMetadata::Local(LocalEntryMetadata::from_file(
        &file,
        file.metadata()?,
    )))
}

pub(super) fn retained(error: TransferError, staging: &Location) -> TransferError {
    TransferError::PublicationUnconfirmed(
        PublicationError::Retained {
            staging: staging.clone(),
            notice: RecoveryNotice::InspectStaging,
            cause: error,
        }
        .into(),
    )
}

pub(super) fn open_staging(location: &Location) -> Result<ReadScope, TransferError> {
    let scope = ReadScope::open(location)?;
    if scope.parent.location != *location {
        return Err(stale(location));
    }
    Ok(scope)
}

pub(super) fn stale(location: &Location) -> TransferError {
    MutationError::PlanChanged(location.clone()).into()
}
