use super::{
    frame_size, media_error, media_extension, Command, Frame, OpenSource, Reply, Snapshot,
    MEDIA_WORKER_ARG, SFTP_LIMIT,
};
use crate::{
    file_error::{AccessAction, AccessError, PreviewError},
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
        let (source, cached) = prepare(target, cancel)?;
        cancel.check()?;
        target.checked(Ok(()))?;
        let mut process = target.checked(source.launch().map_err(helper_error))?;
        let (mut input, mut output) = process.pipes(TIMEOUT, cancel);
        let opened = source
            .send(&mut input)
            .and_then(|()| read_message(&mut output));
        match target.checked(opened.map_err(helper_error))? {
            Reply::Opened => Ok(Self {
                target,
                process,
                _cached: cached,
            }),
            reply => Err(reply_error(&target.location, reply)),
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
        let (mut input, mut output) = self.process.pipes(TIMEOUT, cancel);
        write_message(&mut input, &command).map_err(helper_error)?;
        let reply = read_message(&mut output).map_err(helper_error)?;
        let Reply::Snapshot {
            state,
            size,
            metadata,
        } = reply
        else {
            return Err(reply_error(&self.target.location, reply));
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
                .ok_or_else(media_error)?;
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
        .ok_or_else(media_error)?;
    let mut cached = tempfile::Builder::new()
        .prefix("pawcommander-media-")
        .suffix(&format!(".{suffix}"))
        .tempfile()
        .map_err(|error| AccessError::new(location, AccessAction::WritePreviewCache, error))?;
    vfs::cache_media_preview(location, cached.as_file_mut(), SFTP_LIMIT, cancel)?;
    Ok((PreparedSource::Path(cached.path().to_owned()), Some(cached)))
}

fn reply_error(location: &Location, reply: Reply) -> TransferError {
    match reply {
        Reply::ReadError(errno) => {
            AccessError::read(location, io::Error::from_raw_os_error(errno)).into()
        }
        Reply::Truncated => PreviewError::ChangedWhileReading(location.clone()).into(),
        _ => media_error().into(),
    }
}

fn helper_error(error: io::Error) -> TransferError {
    TransferError::io(error, |error| match error.kind() {
        io::ErrorKind::TimedOut => crate::i18n::text!("Медиаплеер не ответил вовремя.").into(),
        _ => media_error(),
    })
}
