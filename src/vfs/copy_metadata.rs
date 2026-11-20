//! Preserve ordinary attributes on private SFTP transfer payloads.
use super::{
    err,
    sftp::remote,
    transfer::{
        entry_metadata, EntryMetadata, LocalEntryMetadata, SourceReader, TransferSourceEntry,
        TransferSourceManifest,
    },
    Location,
};
use crate::{
    i18n::text,
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

struct Attributes {
    shape: Option<Shape>,
    mode: Option<u32>,
    accessed: Option<SystemTime>,
    modified: Option<SystemTime>,
}

impl Attributes {
    fn from_entry(entry: &EntryMetadata) -> Self {
        match entry {
            EntryMetadata::Local(local) => Self::from_local(local),
            EntryMetadata::Remote(remote) => Self::from_remote(remote),
        }
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
        Err(crate::tr_format!(
            "Объект {target} изменился во время сохранения метаданных.",
            "Item {target} changed while preserving metadata."
        )
        .into())
    }

    fn warning(&self, current: &Self, remote_target: bool) -> Option<String> {
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
            fields.push(text("права доступа"));
        }
        if !exact_time(self.accessed, current.accessed) {
            fields.push(text("время доступа"));
        }
        if !exact_time(self.modified, current.modified) {
            fields.push(text("время изменения"));
        }
        if fields.is_empty() {
            return None;
        }
        let fields = fields.join(", ");
        Some(crate::tr_format!(
            "Не удалось полностью сохранить {fields}.",
            "Could not fully preserve {fields}."
        ))
    }
}

fn from_seconds(value: Option<u64>) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_secs(value?))
}

fn wire_time(value: Option<SystemTime>) -> Option<u64> {
    let seconds = value?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    u32::try_from(seconds).ok().map(u64::from)
}

fn local_attribute(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::Unsupported | ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        result => result,
    }
}

fn local_apply(
    file: &File,
    wanted: &Attributes,
    mode: Mode,
    target: &Location,
    existing: Option<&TransferSourceEntry>,
) -> Result<Option<String>, TransferError> {
    let original = file.metadata()?;
    if let Some(existing) = existing {
        existing.verify_metadata(
            target,
            &EntryMetadata::Local(LocalEntryMetadata::from_file(file, original.clone())),
        )?;
    }
    let initial = Attributes::from_local(&original);
    wanted.check_shape(&initial, target)?;
    if let Some(accessed) = wanted
        .accessed
        .filter(|&time| Some(time) != initial.accessed)
    {
        local_attribute(file.set_times(FileTimes::new().set_accessed(accessed)))?;
    }
    if let Some(modified) = wanted
        .modified
        .filter(|&time| Some(time) != initial.modified)
    {
        local_attribute(file.set_times(FileTimes::new().set_modified(modified)))?;
    }
    #[cfg(unix)]
    if let Some(permissions) = wanted.requested_mode(mode)
        && initial.mode.map(|mode| mode & 0o7777) != Some(permissions)
    {
        local_attribute(file.set_permissions(fs::Permissions::from_mode(permissions)))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    let observed = file.metadata()?;
    let after = Attributes::from_local(&observed);
    wanted.check_shape(&after, target)?;
    if existing.is_some() {
        let opened = EntryMetadata::Local(LocalEntryMetadata::from_file(file, observed));
        let actual =
            entry_metadata(target)?.ok_or_else(|| super::transfer::changed_source(target))?;
        TransferSourceEntry {
            metadata: opened,
            children: None,
        }
        .verify_metadata(target, &actual)?;
    }
    Ok(wanted.warning(&after, false))
}

enum RemoteObject<'a> {
    Path(&'a Sftp, &'a Path),
    Handle(&'a mut ssh2::File),
}

impl RemoteObject<'_> {
    fn stat(&mut self) -> Result<FileStat, TransferError> {
        match self {
            Self::Path(channel, path) => channel.lstat(path),
            Self::Handle(handle) => handle.stat(),
        }
        .map_err(|error| TransferError::Failed(err(error)))
    }

    fn set(&mut self, stat: FileStat) -> Result<(), TransferError> {
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
                Ok(())
            }
            result => result.map_err(|error| TransferError::Failed(err(error))),
        }
    }

    fn apply(
        &mut self,
        wanted: &Attributes,
        mode: Mode,
        target: &Location,
        existing: Option<&TransferSourceEntry>,
    ) -> Result<Option<String>, TransferError> {
        let initial = self.stat()?;
        if let Some(existing) = existing {
            existing.verify_metadata(target, &EntryMetadata::Remote(initial.clone()))?;
        }
        wanted.check_shape(&Attributes::from_remote(&initial), target)?;
        let accessed = wire_time(wanted.accessed).or(initial.atime);
        let modified = wire_time(wanted.modified).or(initial.mtime);
        let mut attempted = false;
        // SFTP v3 encodes both times together. Retain the target's other
        // value if one source date is absent or outside the wire range.
        if let (Some(atime), Some(mtime)) = (accessed, modified)
            && (accessed != initial.atime || modified != initial.mtime)
        {
            self.set(FileStat {
                atime: Some(atime),
                mtime: Some(mtime),
                ..empty_stat()
            })?;
            attempted = true;
        }
        if let Some(permissions) = wanted.requested_mode(mode)
            && initial.perm.map(|mode| mode & 0o7777) != Some(permissions)
        {
            self.set(FileStat {
                perm: Some(permissions),
                ..empty_stat()
            })?;
            attempted = true;
        }
        let stat = if attempted { self.stat()? } else { initial };
        let after = Attributes::from_remote(&stat);
        wanted.check_shape(&after, target)?;
        Ok(wanted.warning(&after, true))
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
    target: &Location,
    wanted: &Attributes,
    channel: Option<&Sftp>,
    mode: Mode,
    existing: Option<&TransferSourceEntry>,
) -> Result<Option<String>, TransferError> {
    if let Some(channel) = channel {
        return RemoteObject::Path(channel, target.path()).apply(wanted, mode, target, existing);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    // A directory handle may be unavailable on a local backend. Treat its
    // attribute operation as unsupported before publishing the private copy.
    let file = match options.open(target.path()) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::Unsupported
            ) =>
        {
            if let Some(existing) = existing {
                let actual = entry_metadata(target)?
                    .ok_or_else(|| super::transfer::changed_source(target))?;
                existing.verify_metadata(target, &actual)?;
            }
            let current = Attributes::from_local(&fs::symlink_metadata(target.path())?);
            wanted.check_shape(&current, target)?;
            return Ok(wanted.warning(&current, false));
        }
        Err(error) => return Err(error.into()),
    };
    local_apply(&file, wanted, mode, target, existing)
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
    manifest: &TransferSourceManifest,
    mode: Mode,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let channel = payload.remote().then(|| remote(payload)).transpose()?;
    for (original, entry) in manifest.entries.iter().rev() {
        progress.checkpoint()?;
        let copy = mapped(source, payload, original);
        let warning = apply_path(
            &copy,
            &Attributes::from_entry(&entry.metadata),
            channel.as_ref(),
            mode,
            None,
        )?;
        progress.results.note_metadata(original, warning.as_deref());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn access_time(metadata: &EntryMetadata) -> Option<SystemTime> {
    Attributes::from_entry(metadata).accessed
}

#[cfg(target_os = "macos")]
pub(super) fn stage_resumed_file(
    source: &Location,
    payload: &Location,
    metadata: &EntryMetadata,
    accessed: Option<SystemTime>,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let mut attributes = Attributes::from_entry(metadata);
    attributes.accessed = accessed;
    let channel = payload.remote().then(|| remote(payload)).transpose()?;
    let warning = apply_path(payload, &attributes, channel.as_ref(), Mode::Final, None)?;
    progress.results.note_metadata(source, warning.as_deref());
    Ok(())
}

pub(super) fn finish_file(
    target: &Location,
    source: &EntryMetadata,
    opened: &mut SourceReader,
) -> Result<Option<String>, TransferError> {
    let wanted = Attributes::from_entry(source);
    match opened {
        SourceReader::Local(file) => local_apply(file, &wanted, Mode::Final, target, None),
        SourceReader::Remote(handle) => {
            RemoteObject::Handle(handle).apply(&wanted, Mode::Final, target, None)
        }
    }
}

pub(super) fn finish_directory(
    target: &Location,
    source: &EntryMetadata,
    existing: &TransferSourceEntry,
) -> Result<Option<String>, TransferError> {
    let channel = target.remote().then(|| remote(target)).transpose()?;
    apply_path(
        target,
        &Attributes::from_entry(source),
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
                            .map_err(err)?;
                    }
                }
                Err(error) if error.code() == ErrorCode::SFTP(LIBSSH2_FX_NO_SUCH_FILE) => {}
                Err(error) => return Err(err(error).into()),
                Ok(_) => return Err(text("Временная папка изменилась перед очисткой").into()),
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
                        )?;
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => return Err(text("Временная папка изменилась перед очисткой").into()),
            }
        }
    }
    Ok(())
}
