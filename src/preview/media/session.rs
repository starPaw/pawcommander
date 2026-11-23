use super::{
    frame_size, media_extension, Command, Frame, OpenSource, Reply, Snapshot, MEDIA_WORKER_ARG,
    SFTP_LIMIT,
};
use crate::{
    file_error::{AccessAction, AccessError, MediaAction, MediaError, PreviewError},
    operations::{Cancellation, TransferError},
    preview::{
        helper::{read_message, write_blob, write_message, Process},
        source::Target,
    },
    vfs::{self, Location},
};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    path::PathBuf,
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(6);

pub(super) struct Session<'a> {
    target: &'a Target,
    // The process exits before the staged SFTP file is removed.
    process: Process,
    _cached: Option<tempfile::NamedTempFile>,
}

impl<'a> Session<'a> {
    pub(super) fn open(target: &'a Target, cancel: &Cancellation) -> Result<Self, TransferError> {
        let location = &target.location;
        let (source, cached) = prepare(target, cancel)?;
        cancel.check()?;
        target.checked(Ok(()))?;
        let mut process = target.checked(
            source
                .launch()
                .map_err(media_io(location, MediaAction::LaunchHelper)),
        )?;
        let (mut input, mut output) = process.pipes(TIMEOUT, cancel);
        let opened = source
            .send(&mut input)
            .map_err(media_io(location, MediaAction::SendSource))
            .and_then(|()| {
                read_message(&mut output).map_err(media_io(location, MediaAction::ReadOpenReply))
            });
        match target.checked(opened)? {
            Reply::Opened => Ok(Self {
                target,
                process,
                _cached: cached,
            }),
            reply => Err(reply_error(location, MediaAction::ReadOpenReply, reply)),
        }
    }

    pub(super) fn update(
        &mut self,
        command: Command,
        cancel: &Cancellation,
    ) -> Result<Snapshot, TransferError> {
        cancel.check()?;
        self.target.checked(Ok(()))?;
        let result = self.exchange(command, cancel);
        self.target.checked(result)
    }

    fn exchange(
        &mut self,
        command: Command,
        cancel: &Cancellation,
    ) -> Result<Snapshot, TransferError> {
        let location = &self.target.location;
        let (mut input, mut output) = self.process.pipes(TIMEOUT, cancel);
        write_message(&mut input, &command)
            .map_err(media_io(location, MediaAction::SendCommand))?;
        let reply =
            read_message(&mut output).map_err(media_io(location, MediaAction::ReadState))?;
        let Reply::Snapshot {
            state,
            size,
            metadata,
        } = reply
        else {
            return Err(reply_error(location, MediaAction::ReadState, reply));
        };
        if !state.valid() {
            return Err(MediaError::InvalidResponse(MediaAction::ReadState)
                .at(location)
                .into());
        }
        let frame = if let Some(size) = size {
            let length = frame_size(size)
                .ok_or_else(|| MediaError::InvalidResponse(MediaAction::ReadFrame).at(location))?;
            let mut rgba = vec![0; length];
            output
                .read_exact(&mut rgba)
                .map_err(media_io(location, MediaAction::ReadFrame))?;
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

enum PreparedSource {
    Path(PathBuf),
    Descriptor(OwnedFd, &'static str),
}

impl PreparedSource {
    fn launch(&self) -> io::Result<Process> {
        let Self::Descriptor(file, _) = self else {
            return Process::launch(MEDIA_WORKER_ARG);
        };
        let mut command = std::process::Command::new(std::env::current_exe()?);
        let fd = file.as_raw_fd();
        // Only the child inherits this duplicate. Between fork and exec the
        // hook only calls fcntl and retrieves errno on failure.
        unsafe {
            command.arg(MEDIA_WORKER_ARG).pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Process::spawn(&mut command)
    }

    fn send(self, input: &mut impl Write) -> io::Result<()> {
        match self {
            Self::Path(path) => {
                write_message(input, &OpenSource::Path)?;
                write_blob(input, path.as_os_str().as_bytes())
            }
            Self::Descriptor(file, extension) => write_message(
                input,
                &OpenSource::Descriptor {
                    fd: file.as_raw_fd(),
                    extension: extension.into(),
                },
            ),
        }
    }
}

fn prepare(
    target: &Target,
    cancel: &Cancellation,
) -> Result<(PreparedSource, Option<tempfile::NamedTempFile>), TransferError> {
    cancel.check()?;
    let location = &target.location;
    if let Some(path) = location.local() {
        if target.capture_mount()?.is_some() {
            let extension = path
                .extension()
                .and_then(media_extension)
                .ok_or_else(|| MediaError::UnsupportedFormat.at(location))?;
            let file = target.open_local(path)?;
            // Keep clear of the helper's stdin, stdout and stderr redirects.
            let fd =
                rustix::io::fcntl_dupfd_cloexec(&file, libc::STDERR_FILENO + 1).map_err(|error| {
                    AccessError::new(location, AccessAction::OpenFile, io::Error::from(error))
                });
            return Ok((
                PreparedSource::Descriptor(target.checked(fd.map_err(Into::into))?, extension),
                None,
            ));
        }
        let path = fs::canonicalize(path)
            .map_err(|error| AccessError::new(location, AccessAction::ResolvePath, error))?;
        if !path
            .metadata()
            .map_err(|error| AccessError::new(location, AccessAction::ReadMetadata, error))?
            .is_file()
        {
            return Err(PreviewError::UnsupportedFile(location.clone()).into());
        }
        return Ok((PreparedSource::Path(path), None));
    }
    let suffix = location
        .path()
        .extension()
        .and_then(|ext| ext.to_str())
        .ok_or_else(|| MediaError::UnsupportedFormat.at(location))?;
    let mut cached = tempfile::Builder::new()
        .prefix("pawcommander-media-")
        .suffix(&format!(".{suffix}"))
        .tempfile()
        .map_err(|error| AccessError::new(location, AccessAction::WritePreviewCache, error))?;
    vfs::cache_media_preview(location, cached.as_file_mut(), SFTP_LIMIT, cancel)?;
    Ok((PreparedSource::Path(cached.path().to_owned()), Some(cached)))
}

fn reply_error(location: &Location, action: MediaAction, reply: Reply) -> TransferError {
    match reply {
        Reply::ReadError(errno) => {
            AccessError::read(location, io::Error::from_raw_os_error(errno)).into()
        }
        Reply::Truncated => PreviewError::ChangedWhileReading(location.clone()).into(),
        Reply::Failed(errors) => MediaError::Playback(errors).at(location).into(),
        _ => MediaError::InvalidResponse(action).at(location).into(),
    }
}

fn media_io(
    location: &Location,
    action: MediaAction,
) -> impl FnOnce(io::Error) -> TransferError + '_ {
    move |source| MediaError::Io { action, source }.at(location).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::{Failure, MediaNativeError};
    use std::error::Error;

    #[test]
    fn media_transport_preserves_the_stage_path_and_os_error_or_cancellation() {
        let location = Location::Local("/fixtures/video.mov".into());
        let error =
            media_io(&location, MediaAction::ReadFrame)(io::Error::from_raw_os_error(libc::EIO));
        let TransferError::Failed(Failure::Preview(preview)) = &error else {
            panic!("expected media preview failure");
        };
        assert!(
            matches!(preview.as_ref(), PreviewError::Media { location: target, cause:
            MediaError::Io { action: MediaAction::ReadFrame, .. } } if target == &location)
        );
        let original = preview
            .source()
            .unwrap()
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(original.raw_os_error(), Some(libc::EIO));
        let message = crate::i18n::file_error(&error);
        assert!(message.contains(&location.display()) && message.contains(&original.to_string()));

        let cancel = Cancellation::default();
        cancel.request();
        let error = media_io(&location, MediaAction::ReadState)(cancel.check().unwrap_err());
        assert!(matches!(error, TransferError::Cancelled));
    }

    #[test]
    fn media_reply_preserves_backend_codes_and_the_selected_path() {
        let location = Location::Local("/fixtures/broken.mp4".into());
        let errors = vec![
            MediaNativeError {
                domain: "AVFoundationErrorDomain".into(),
                code: -11829,
            },
            MediaNativeError {
                domain: "NSOSStatusErrorDomain".into(),
                code: -12848,
            },
        ];
        let error = reply_error(&location, MediaAction::ReadState, Reply::Failed(errors));
        let TransferError::Failed(Failure::Preview(preview)) = &error else {
            panic!("expected the typed playback failure");
        };
        assert!(
            matches!(preview.as_ref(), PreviewError::Media { location: target, cause:
            MediaError::Playback(errors) } if target == &location && errors.len() == 2)
        );
        let message = crate::i18n::file_error(&error);
        for detail in [
            location.display().as_str(),
            "AVFoundationErrorDomain (-11829)",
            "NSOSStatusErrorDomain (-12848)",
        ] {
            assert!(message.contains(detail), "{message}");
        }
    }
}
