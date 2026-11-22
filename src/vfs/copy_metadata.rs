//! Preserve ordinary attributes on transfer payloads and mounted SMB files.
use super::{
    sftp::remote,
    transfer::{
        entry_metadata, EntryMetadata, LocalEntryMetadata, SourceReader, TransferSourceEntry,
        TransferSourceManifest,
    },
    Location,
};
use crate::{
    file_error::{
        AccessAction, AccessCause, AccessError, MetadataError, MetadataField, MetadataWarning,
    },
    fs_ops::local_copy_write_error,
    operations::{Progress, TransferError},
};
use libssh2_sys::{
    LIBSSH2_FX_FAILURE, LIBSSH2_FX_NO_SUCH_FILE, LIBSSH2_FX_OP_UNSUPPORTED,
    LIBSSH2_FX_PERMISSION_DENIED,
};
use ssh2::{ErrorCode, FileStat, Sftp};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Final,
    KeepReadableForMove,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    File(u64),
    Directory,
}

pub(super) struct Attributes {
    shape: Option<Shape>,
    mode: Option<u32>,
    accessed: Option<SystemTime>,
    modified: Option<SystemTime>,
}

impl Attributes {
    pub(super) fn from_entry(entry: &EntryMetadata) -> Self {
        match entry {
            EntryMetadata::Local(local) => Self::from_local(local),
            EntryMetadata::Remote(remote) => Self::from_remote(remote),
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn with_accessed(mut self, accessed: Option<SystemTime>) -> Self {
        self.accessed = accessed;
        self
    }

    fn from_local(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let mode = Some(metadata.permissions().mode());
        #[cfg(not(unix))]
        let mode = None;
        let shape = if metadata.is_file() {
            Some(Shape::File(metadata.len()))
        } else {
            metadata.is_dir().then_some(Shape::Directory)
        };
        Self {
            shape,
            mode,
            accessed: metadata.accessed().ok(),
            modified: metadata.modified().ok(),
        }
    }

    fn from_remote(stat: &FileStat) -> Self {
        let shape = if stat.is_file() {
            stat.size.map(Shape::File)
        } else {
            stat.is_dir().then_some(Shape::Directory)
        };
        Self {
            shape,
            mode: stat.perm,
            accessed: from_seconds(stat.atime),
            modified: from_seconds(stat.mtime),
        }
    }

    fn requested_mode(&self, mode: Mode) -> Option<u32> {
        let directory = self.shape == Some(Shape::Directory);
        let mask = if directory { 0o1777 } else { 0o777 };
        // The copy has its own owner and group. Preserve the sticky bit on
        // directories without transferring set-user-ID or set-group-ID.
        let readable = match (mode, directory) {
            (Mode::KeepReadableForMove, true) => 0o500,
            (Mode::KeepReadableForMove, false) => 0o400,
            _ => 0,
        };
        self.mode.map(|original| (original & mask) | readable)
    }

    fn check_shape(&self, current: &Self, target: &Location) -> Result<(), TransferError> {
        if self.shape.is_some() && self.shape == current.shape {
            return Ok(());
        }
        Err(MetadataError::TargetChanged(target.clone()).into())
    }

    fn warning(
        &self,
        current: &Self,
        remote_target: bool,
        causes: Vec<AccessCause>,
    ) -> Option<MetadataWarning> {
        let exact_time = |expected, actual| {
            let expected = if remote_target {
                from_seconds(wire_time(expected))
            } else {
                expected
            };
            expected.is_some() && expected == actual
        };
        let mut fields = Vec::new();
        if self.mode.is_none()
            || self.mode.map(|mode| mode & 0o7777) != current.mode.map(|mode| mode & 0o7777)
        {
            fields.push(MetadataField::Permissions);
        }
        if !exact_time(self.accessed, current.accessed) {
            fields.push(MetadataField::AccessTime);
        }
        if !exact_time(self.modified, current.modified) {
            fields.push(MetadataField::ModificationTime);
        }
        (!fields.is_empty()).then_some(MetadataWarning { fields, causes })
    }
}

fn from_seconds(value: Option<u64>) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_secs(value?))
}

fn wire_time(value: Option<SystemTime>) -> Option<u64> {
    let seconds = value?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    u32::try_from(seconds).ok().map(u64::from)
}

fn local_attribute(
    result: io::Result<()>,
    target: &Location,
    action: AccessAction,
    causes: &mut Vec<AccessCause>,
) -> Result<(), TransferError> {
    match result {
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::Unsupported | ErrorKind::InvalidInput
            ) =>
        {
            causes.push(error.into());
            Ok(())
        }
        result => result.map_err(|error| local_copy_write_error(target, action, error)),
    }
}

fn local_apply(
    file: &File,
    wanted: &Attributes,
    mode: Mode,
    target: &Location,
    existing: Option<&TransferSourceEntry>,
) -> Result<Option<MetadataWarning>, TransferError> {
    let read_error = |error| AccessError::new(target, AccessAction::ReadOpenMetadata, error);
    let original = file.metadata().map_err(read_error)?;
    if let Some(existing) = existing {
        existing.verify_metadata(
            target,
            &EntryMetadata::Local(LocalEntryMetadata::from_file(file, original.clone())),
        )?;
    }
    let initial = Attributes::from_local(&original);
    wanted.check_shape(&initial, target)?;
    let mut causes = Vec::new();
    if let Some(accessed) = wanted
        .accessed
        .filter(|&time| Some(time) != initial.accessed)
    {
        local_attribute(
            file.set_times(FileTimes::new().set_accessed(accessed)),
            target,
            AccessAction::WriteMetadata,
            &mut causes,
        )?;
    }
    if let Some(modified) = wanted
        .modified
        .filter(|&time| Some(time) != initial.modified)
    {
        local_attribute(
            file.set_times(FileTimes::new().set_modified(modified)),
            target,
            AccessAction::WriteMetadata,
            &mut causes,
        )?;
    }
    #[cfg(unix)]
    if let Some(permissions) = wanted.requested_mode(mode)
        && initial.mode.map(|mode| mode & 0o7777) != Some(permissions)
    {
        local_attribute(
            file.set_permissions(fs::Permissions::from_mode(permissions)),
            target,
            AccessAction::WritePermissions,
            &mut causes,
        )?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    let observed = file.metadata().map_err(read_error)?;
    let after = Attributes::from_local(&observed);
    wanted.check_shape(&after, target)?;
    if existing.is_some() {
        let opened = EntryMetadata::Local(LocalEntryMetadata::from_file(file, observed));
        let actual =
            entry_metadata(target)?.ok_or_else(|| MetadataError::TargetChanged(target.clone()))?;
        TransferSourceEntry {
            metadata: opened,
            children: None,
        }
        .verify_metadata(target, &actual)?;
    }
    Ok(wanted.warning(&after, false, causes))
}

enum RemoteObject<'a> {
    Path(&'a Sftp, &'a Path),
    Handle(&'a mut ssh2::File),
}

impl RemoteObject<'_> {
    fn stat(&mut self, target: &Location) -> Result<FileStat, TransferError> {
        let (result, action) = match self {
            Self::Path(channel, path) => (channel.lstat(path), AccessAction::ReadMetadata),
            Self::Handle(handle) => (handle.stat(), AccessAction::ReadOpenMetadata),
        };
        result.map_err(|error| AccessError::new(target, action, error).into())
    }

    fn set(
        &mut self,
        stat: FileStat,
        target: &Location,
        causes: &mut Vec<AccessCause>,
    ) -> Result<(), TransferError> {
        let result = match self {
            Self::Path(channel, path) => channel.setstat(path, stat),
            Self::Handle(handle) => handle.setstat(stat),
        };
        match result {
            Err(error)
                if matches!(
                    error.code(),
                    ErrorCode::SFTP(
                        LIBSSH2_FX_PERMISSION_DENIED
                            | LIBSSH2_FX_OP_UNSUPPORTED
                            | LIBSSH2_FX_FAILURE
                    )
                ) =>
            {
                causes.push(error.into());
                Ok(())
            }
            result => result.map_err(|error| {
                AccessError::new(target, AccessAction::WriteMetadata, error).into()
            }),
        }
    }

    fn apply(
        &mut self,
        wanted: &Attributes,
        mode: Mode,
        target: &Location,
        existing: Option<&TransferSourceEntry>,
    ) -> Result<Option<MetadataWarning>, TransferError> {
        let initial = self.stat(target)?;
        if let Some(existing) = existing {
            existing.verify_metadata(target, &EntryMetadata::Remote(initial.clone()))?;
        }
        wanted.check_shape(&Attributes::from_remote(&initial), target)?;
        let accessed = wire_time(wanted.accessed).or(initial.atime);
        let modified = wire_time(wanted.modified).or(initial.mtime);
        let mut attempted = false;
        let mut causes = Vec::new();
        // SFTP v3 encodes both times together. Retain the target's other
        // value if one source date is absent or outside the wire range.
        if let (Some(atime), Some(mtime)) = (accessed, modified)
            && (accessed != initial.atime || modified != initial.mtime)
        {
            self.set(
                FileStat {
                    atime: Some(atime),
                    mtime: Some(mtime),
                    ..empty_stat()
                },
                target,
                &mut causes,
            )?;
            attempted = true;
        }
        if let Some(permissions) = wanted.requested_mode(mode)
            && initial.perm.map(|mode| mode & 0o7777) != Some(permissions)
        {
            self.set(
                FileStat {
                    perm: Some(permissions),
                    ..empty_stat()
                },
                target,
                &mut causes,
            )?;
            attempted = true;
        }
        let stat = if attempted {
            self.stat(target)?
        } else {
            initial
        };
        let after = Attributes::from_remote(&stat);
        wanted.check_shape(&after, target)?;
        Ok(wanted.warning(&after, true, causes))
    }
}

fn empty_stat() -> FileStat {
    FileStat {
        size: None,
        uid: None,
        gid: None,
        perm: None,
        atime: None,
        mtime: None,
    }
}

fn apply_path(
    location: &Location,
    report_at: &Location,
    wanted: &Attributes,
    channel: Option<&Sftp>,
    mode: Mode,
    existing: Option<&TransferSourceEntry>,
) -> Result<Option<MetadataWarning>, TransferError> {
    if let Some(channel) = channel {
        return RemoteObject::Path(channel, location.path())
            .apply(wanted, mode, report_at, existing);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    // A directory handle may be unavailable on a local backend. Treat its
    // attribute operation as unsupported before publishing the private copy.
    let file = match options.open(location.path()) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::Unsupported
            ) =>
        {
            if let Some(existing) = existing {
                let actual = entry_metadata(location)?
                    .ok_or_else(|| MetadataError::TargetChanged(report_at.clone()))?;
                existing.verify_metadata(report_at, &actual)?;
            }
            let metadata = fs::symlink_metadata(location.path())
                .map_err(|error| AccessError::new(report_at, AccessAction::ReadMetadata, error))?;
            let current = Attributes::from_local(&metadata);
            wanted.check_shape(&current, report_at)?;
            return Ok(wanted.warning(&current, false, vec![error.into()]));
        }
        Err(error) => return Err(AccessError::new(report_at, AccessAction::OpenFile, error).into()),
    };
    local_apply(&file, wanted, mode, report_at, existing)
}

fn mapped(source: &Location, target: &Location, entry: &Location) -> Location {
    let relative = entry
        .path()
        .strip_prefix(source.path())
        .expect("copy manifest root");
    if relative.as_os_str().is_empty() {
        target.clone()
    } else {
        target.join(relative)
    }
}

pub(super) fn stage(
    source: &Location,
    payload: &Location,
    target: &Location,
    manifest: &TransferSourceManifest,
    mode: Mode,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let channel = payload.remote().then(|| remote(payload)).transpose()?;
    for (original, entry) in manifest.entries.iter().rev() {
        progress.checkpoint()?;
        let copy = mapped(source, payload, original);
        let result = apply_path(
            &copy,
            &mapped(source, target, original),
            &Attributes::from_entry(&entry.metadata),
            channel.as_ref(),
            mode,
            None,
        );
        record_staged_metadata(original, progress, result)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn access_time(metadata: &EntryMetadata) -> Option<SystemTime> {
    Attributes::from_entry(metadata).accessed
}

#[cfg(target_os = "macos")]
pub(super) fn stage_resumed_entry(
    source: &Location,
    payload: &Location,
    target: &Location,
    metadata: &EntryMetadata,
    accessed: Option<SystemTime>,
    mode: Mode,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let mut attributes = Attributes::from_entry(metadata);
    attributes.accessed = accessed;
    let channel = payload.remote().then(|| remote(payload)).transpose()?;
    let result = apply_path(payload, target, &attributes, channel.as_ref(), mode, None);
    record_staged_metadata(source, progress, result)
}

fn record_staged_metadata(
    source: &Location,
    progress: &mut Progress,
    result: Result<Option<MetadataWarning>, TransferError>,
) -> Result<(), TransferError> {
    match result {
        Ok(warning) => {
            progress.results.note_metadata(source, warning);
            Ok(())
        }
        Err(error) => {
            progress.results.staged_with_error(source, &error);
            Err(error)
        }
    }
}

pub(super) fn finish_file(
    target: &Location,
    wanted: &Attributes,
    opened: &mut SourceReader,
) -> Result<Option<MetadataWarning>, TransferError> {
    match opened {
        SourceReader::Local(file) => finish_local_file(target, wanted, file),
        SourceReader::Remote(handle) => {
            RemoteObject::Handle(handle).apply(wanted, Mode::Final, target, None)
        }
    }
}

pub(super) fn finish_local_file(
    target: &Location,
    wanted: &Attributes,
    file: &fs::File,
) -> Result<Option<MetadataWarning>, TransferError> {
    local_apply(file, wanted, Mode::Final, target, None)
}

pub(super) fn finish_directory(
    target: &Location,
    wanted: &Attributes,
    existing: &TransferSourceEntry,
) -> Result<Option<MetadataWarning>, TransferError> {
    let channel = target.remote().then(|| remote(target)).transpose()?;
    apply_path(
        target,
        target,
        wanted,
        channel.as_ref(),
        Mode::Final,
        Some(existing),
    )
}

/// Re-enable traversal in the private payload so a definite failure can
/// clean it up. This never visits the previous destination or held source.
pub(super) fn reopen_for_cleanup(
    source: &Location,
    payload: &Location,
    manifest: &TransferSourceManifest,
) -> Result<(), TransferError> {
    let channel = payload.remote().then(|| remote(payload)).transpose()?;
    for (original, entry) in &manifest.entries {
        if entry.children.is_none() {
            continue;
        }
        let dir = mapped(source, payload, original);
        if let Some(channel) = &channel {
            match channel.lstat(dir.path()) {
                Ok(stat) if stat.is_dir() => {
                    if let Some(mode) = stat.perm.filter(|mode| mode & 0o700 != 0o700) {
                        channel
                            .setstat(
                                dir.path(),
                                FileStat {
                                    perm: Some((mode & 0o1777) | 0o700),
                                    ..empty_stat()
                                },
                            )
                            .map_err(|error| {
                                AccessError::new(&dir, AccessAction::WriteMetadata, error)
                            })?;
                    }
                }
                Err(error) if error.code() == ErrorCode::SFTP(LIBSSH2_FX_NO_SUCH_FILE) => {}
                Err(error) => {
                    return Err(AccessError::new(&dir, AccessAction::ReadMetadata, error).into())
                }
                Ok(_) => return Err(MetadataError::CleanupDirectoryChanged(dir).into()),
            }
        } else {
            match fs::symlink_metadata(dir.path()) {
                Ok(metadata) if metadata.is_dir() => {
                    #[cfg(unix)]
                    if metadata.permissions().mode() & 0o700 != 0o700 {
                        fs::set_permissions(
                            dir.path(),
                            fs::Permissions::from_mode(
                                (metadata.permissions().mode() & 0o1777) | 0o700,
                            ),
                        )
                        .map_err(|error| {
                            AccessError::new(&dir, AccessAction::WriteMetadata, error)
                        })?;
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(AccessError::new(&dir, AccessAction::ReadMetadata, error).into())
                }
                Ok(_) => return Err(MetadataError::CleanupDirectoryChanged(dir).into()),
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::file_error::Failure;

    #[test]
    fn local_metadata_write_retains_tolerated_and_fatal_errors() {
        let target = Location::Local("/fixture/copy".into());
        let mut causes = Vec::new();
        local_attribute(
            Err(io::Error::from_raw_os_error(libc::EACCES)),
            &target,
            AccessAction::WritePermissions,
            &mut causes,
        )
        .unwrap();
        let wanted = Attributes {
            shape: Some(Shape::File(5)),
            mode: Some(0o640),
            accessed: from_seconds(Some(42)),
            modified: from_seconds(Some(43)),
        };
        let observed = Attributes {
            mode: Some(0o600),
            ..wanted
        };
        let warning = wanted.warning(&observed, false, causes).unwrap();
        assert_eq!(warning.fields, [MetadataField::Permissions]);
        assert!(
            matches!(warning.causes.as_slice(), [AccessCause::Local(source)]
            if source.raw_os_error() == Some(libc::EACCES))
        );

        let error = local_attribute(
            Err(io::Error::from_raw_os_error(libc::EIO)),
            &target,
            AccessAction::WritePermissions,
            &mut Vec::new(),
        )
        .unwrap_err();
        let TransferError::Failed(Failure::Access(error)) = error else {
            panic!("expected a typed metadata write failure");
        };
        assert_eq!(error.location, target);
        assert_eq!(error.action, AccessAction::WritePermissions);
        assert!(matches!(error.cause, AccessCause::Local(source)
            if source.raw_os_error() == Some(libc::EIO)));
    }
}
