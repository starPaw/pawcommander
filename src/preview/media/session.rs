use super::{
    frame_size, media_error, Command, Frame, Reply, Snapshot, MEDIA_WORKER_ARG, SFTP_LIMIT,
};
use crate::{
    file_error::{AccessAction, AccessError, PreviewError},
    operations::{Cancellation, TransferError},
    preview::helper::{read_message, write_blob, write_message, Process},
    vfs::{self, Location},
};
use std::{
    fs,
    io::{self, Read},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(6);

pub(super) struct Session {
    // The process exits before the staged SFTP file is removed.
    process: Process,
    _cached: Option<tempfile::NamedTempFile>,
}

impl Session {
    pub(super) fn open(location: &Location, cancel: &Cancellation) -> Result<Self, TransferError> {
        let (path, cached) = prepare(location, cancel)?;
        cancel.check()?;
        let mut process = Process::launch(MEDIA_WORKER_ARG).map_err(helper_error)?;
        let (mut input, mut output) = process.pipes(TIMEOUT, cancel);
        write_blob(&mut input, path.as_os_str().as_bytes()).map_err(helper_error)?;
        if !matches!(
            read_message(&mut output).map_err(helper_error)?,
            Reply::Opened
        ) {
            return Err(media_error().into());
        }
        Ok(Self {
            process,
            _cached: cached,
        })
    }

    pub(super) fn update(
        &mut self,
        command: Command,
        cancel: &Cancellation,
    ) -> Result<Snapshot, TransferError> {
        let (mut input, mut output) = self.process.pipes(TIMEOUT, cancel);
        write_message(&mut input, &command).map_err(helper_error)?;
        let Reply::Snapshot {
            state,
            size,
            metadata,
        } = read_message(&mut output).map_err(helper_error)?
        else {
            return Err(media_error().into());
        };
        if !state.valid() {
            return Err(media_error().into());
        }
        let frame = if let Some(size) = size {
            let mut rgba = vec![0; frame_size(size).ok_or_else(media_error)?];
            output.read_exact(&mut rgba).map_err(helper_error)?;
            Some(Frame { size, rgba })
        } else {
            None
        };
        Ok(Snapshot {
            state,
            frame,
            metadata,
        })
    }
}

fn prepare(
    location: &Location,
    cancel: &Cancellation,
) -> Result<(PathBuf, Option<tempfile::NamedTempFile>), TransferError> {
    cancel.check()?;
    if let Some(path) = location.local() {
        let path = fs::canonicalize(path)
            .map_err(|error| AccessError::new(location, AccessAction::ResolvePath, error))?;
        if !path
            .metadata()
            .map_err(|error| AccessError::new(location, AccessAction::ReadMetadata, error))?
            .is_file()
        {
            return Err(PreviewError::UnsupportedFile(location.clone()).into());
        }
        return Ok((path, None));
    }
    let suffix = location
        .path()
        .extension()
        .and_then(|ext| ext.to_str())
        .ok_or_else(media_error)?;
    let mut cached = tempfile::Builder::new()
        .prefix("pawcommander-media-")
        .suffix(&format!(".{suffix}"))
        .tempfile()
        .map_err(|error| AccessError::new(location, AccessAction::WritePreviewCache, error))?;
    vfs::cache_media_preview(location, cached.as_file_mut(), SFTP_LIMIT, cancel)?;
    Ok((cached.path().to_owned(), Some(cached)))
}

fn helper_error(error: io::Error) -> TransferError {
    TransferError::io(error, |error| match error.kind() {
        io::ErrorKind::TimedOut => crate::i18n::text!("Медиаплеер не ответил вовремя.").into(),
        _ => media_error(),
    })
}
