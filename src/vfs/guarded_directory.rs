//! Directory identity checks and descriptor-relative, no-replacement publication.
use super::{
    read_scope::{changed_during_read, local_subdirectory, ReadScope},
    transfer::{
        canonical, entry_metadata, rename_for_publication, EntryMetadata, LocalEntryMetadata,
    },
    Location,
};
use crate::operations::{Cancellation, TransferError};
use std::{ffi::OsStr, fs, io, os::fd::OwnedFd, path::Path};

pub(super) struct Directory {
    pub(super) location: Location,
    descriptor: Option<OwnedFd>,
    metadata: EntryMetadata,
}

impl Directory {
    pub(super) fn from_scope(scope: &ReadScope) -> Result<Self, TransferError> {
        let location = scope.parent.location.clone();
        let metadata = entry_metadata(&location)?.ok_or_else(changed_during_read)?;
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
            .ok_or_else(changed_during_read)?;
        let descriptor = scope
            .descriptor
            .as_ref()
            .map(|root| {
                local_subdirectory(root, relative, cancel)?
                    .map_or_else(|| rustix::io::dup(root).map_err(io::Error::from), Ok)
            })
            .transpose()?;
        let anchor = Self {
            location,
            descriptor,
            metadata: expected.clone(),
        };
        anchor.verify()?;
        if let Some(fd) = &anchor.descriptor {
            let file = fs::File::from(rustix::io::dup(fd).map_err(io::Error::from)?);
            let captured =
                EntryMetadata::Local(LocalEntryMetadata::from_file(&file, file.metadata()?));
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
        self.verify()?;
        let path = self.location.join(name);
        if entry_metadata(&path)?.is_some() {
            return Err(stale(&path));
        }
        Ok(())
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
            .map_err(Into::into)
        } else {
            rename_for_publication(&from, &to)
        }
    }
}

pub(super) fn retained(error: TransferError, staging: &Location) -> TransferError {
    TransferError::PublicationUnconfirmed(crate::tr_format!(
        "{error}. Временная папка сохранена для проверки: {staging}.",
        "{error}. Temporary folder retained for inspection: {staging}."
    ))
}

pub(super) fn open_staging(location: &Location) -> Result<ReadScope, TransferError> {
    let scope = ReadScope::open(location)?;
    if scope.parent.location != *location {
        return Err(stale(location));
    }
    Ok(scope)
}

pub(super) fn stale(location: &Location) -> TransferError {
    crate::tr_format!(
        "После построения плана объект изменился: {location}. Постройте план заново.",
        "An item changed after planning: {location}. Build the plan again."
    )
    .into()
}
