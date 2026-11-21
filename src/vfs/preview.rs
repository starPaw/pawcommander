use super::location::Location;
use super::sftp::remote;
use crate::file_error::{AccessAction, AccessError, PreviewError};
use crate::operations::{Cancellation, TransferError};
#[cfg(target_os = "macos")]
use std::io::{ErrorKind, Read};

pub(crate) fn preview_data(
    location: &Location,
    limit: usize,
    cancel: &Cancellation,
) -> Result<(ssh2::FileStat, Vec<u8>), TransferError> {
    cancel.check()?;
    let (stat, input) = preview_input(location)?;
    cancel.check()?;
    let bytes = if let Some(mut input) = input {
        cancel
            .read_up_to(&mut input, limit.saturating_add(1))
            .map_err(|error| AccessError::read(location, error))?
    } else {
        Vec::new()
    };
    Ok((stat, bytes))
}

pub(crate) fn open_preview_stream(location: &Location) -> Result<ssh2::File, TransferError> {
    preview_input(location)?
        .1
        .ok_or_else(|| PreviewError::ChangedBeforeRead(location.clone()).into())
}

fn preview_input(
    location: &Location,
) -> Result<(ssh2::FileStat, Option<ssh2::File>), TransferError> {
    let sftp = remote(location)
        .map_err(|error| AccessError::new(location, AccessAction::OpenFile, error))?;
    let stat = sftp
        .lstat(location.path())
        .map_err(|error| AccessError::new(location, AccessAction::ReadMetadata, error))?;
    if stat.is_dir() {
        return Ok((stat, None));
    }
    if !stat.is_file() {
        return Err(PreviewError::UnsupportedFile(location.clone()).into());
    }
    let mut input = sftp
        .open(location.path())
        .map_err(|error| AccessError::new(location, AccessAction::OpenFile, error))?;
    let opened = input
        .stat()
        .map_err(|error| AccessError::new(location, AccessAction::ReadOpenMetadata, error))?;
    if !opened.is_file() {
        return Err(PreviewError::ChangedBeforeRead(location.clone()).into());
    }
    Ok((opened, Some(input)))
}

#[cfg(target_os = "macos")]
pub(crate) fn cache_media_preview(
    location: &Location,
    target: &mut impl std::io::Write,
    limit: u64,
    cancel: &Cancellation,
) -> Result<(), TransferError> {
    let too_large = || PreviewError::MediaLimit {
        location: location.clone(),
        limit,
    };
    let changed = || PreviewError::ChangedWhileReading(location.clone());
    let cache_error = |error| AccessError::new(location, AccessAction::WritePreviewCache, error);
    cancel.check()?;
    let (stat, input) = preview_input(location)?;
    if stat.size.is_some_and(|size| size > limit) {
        return Err(too_large().into());
    }
    let mut input = input
        .ok_or_else(|| PreviewError::ChangedBeforeRead(location.clone()))?
        .take(limit.saturating_add(1));
    let mut copied = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        cancel.check()?;
        let count = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(AccessError::read(location, error).into()),
        };
        copied += count as u64;
        if copied > limit {
            return Err(too_large().into());
        }
        cancel.check()?;
        target.write_all(&buffer[..count]).map_err(cache_error)?;
    }
    if stat.size.is_some_and(|size| size != copied) {
        return Err(changed().into());
    }
    cancel.check()?;
    let current = input
        .get_mut()
        .stat()
        .map_err(|error| AccessError::new(location, AccessAction::ReadOpenMetadata, error))?;
    if current.size != stat.size || current.mtime != stat.mtime || !current.is_file() {
        return Err(changed().into());
    }
    cancel.check()?;
    target.flush().map_err(|error| cache_error(error).into())
}
