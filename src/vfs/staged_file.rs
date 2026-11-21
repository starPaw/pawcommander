use super::{sftp::remote, Location};
use crate::file_error::{AccessAction, AccessError};
use crate::operations::{Cancellation, TransferError};
use ssh2::{OpenFlags, OpenType};
use std::{
    fs,
    io::{self, BufWriter, Write},
};

enum OutputFile {
    Local(fs::File),
    Remote(ssh2::File),
}

impl Write for OutputFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Local(file) => file.write(bytes),
            Self::Remote(file) => file.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Local(file) => file.flush(),
            Self::Remote(file) => file.flush(),
        }
    }
}

pub(super) struct StagedFile {
    path: Location,
    file: BufWriter<OutputFile>,
    cancel: Cancellation,
}

impl StagedFile {
    pub(super) fn open(path: &Location, cancel: Cancellation) -> Result<Self, TransferError> {
        cancel.check()?;
        let action = AccessAction::OpenFile;
        let file = if let Some(local) = path.local() {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            OutputFile::Local(
                options
                    .open(local)
                    .map_err(|error| AccessError::new(path, action, error))?,
            )
        } else {
            let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE;
            OutputFile::Remote(
                remote(path)
                    .map_err(|error| AccessError::new(path, action, error))?
                    .open_mode(path.path(), flags, 0o600, OpenType::File)
                    .map_err(|error| AccessError::new(path, action, error))?,
            )
        };
        Ok(Self {
            path: path.clone(),
            file: BufWriter::with_capacity(64 * 1024, file),
            cancel,
        })
    }

    #[cfg(target_os = "macos")]
    pub(super) fn open_at(
        path: &Location,
        offset: u64,
        expected: &super::transfer::EntryMetadata,
        cancel: Cancellation,
    ) -> Result<Self, TransferError> {
        use super::err;
        use super::transfer::{EntryMetadata, LocalEntryMetadata};
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        cancel.check()?;
        let file = if let Some(path) = path.local() {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)?;
            let metadata = file.metadata()?;
            if metadata.nlink() != 1
                || !expected.matches_file(&EntryMetadata::Local(LocalEntryMetadata::from_file(
                    &file, metadata,
                )))
            {
                return Err(super::resume_copy::changed_copy());
            }
            file.seek(SeekFrom::Start(offset))?;
            OutputFile::Local(file)
        } else {
            let mut file = remote(path)?
                .open_mode(path.path(), OpenFlags::WRITE, 0, OpenType::File)
                .map_err(err)?;
            if !expected.matches_file(&EntryMetadata::Remote(file.stat().map_err(err)?)) {
                return Err(super::resume_copy::changed_copy());
            }
            file.seek(SeekFrom::Start(offset))?;
            OutputFile::Remote(file)
        };
        Ok(Self {
            path: path.clone(),
            file: BufWriter::with_capacity(64 * 1024, file),
            cancel,
        })
    }

    pub(super) fn close(mut self) -> Result<(), TransferError> {
        self.flush()
            .map_err(|error| AccessError::write(&self.path, error))?;
        let output = self
            .file
            .into_inner()
            .map_err(|error| AccessError::write(&self.path, error.into_error()))?;
        match output {
            OutputFile::Local(file) => file
                .sync_all()
                .map_err(|error| AccessError::write(&self.path, error))?,
            OutputFile::Remote(mut file) => file
                .close()
                .map_err(|error| AccessError::new(&self.path, AccessAction::WriteFile, error))?,
        }
        Ok(())
    }
}

impl Write for StagedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.cancel.check()?;
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.cancel.check()?;
        self.file.flush()
    }
}
