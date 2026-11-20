use super::{err, sftp::remote, Location};
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
    file: BufWriter<OutputFile>,
    cancel: Cancellation,
}

impl StagedFile {
    pub(super) fn open(path: &Location, cancel: Cancellation) -> Result<Self, TransferError> {
        cancel.check()?;
        let file = if let Some(path) = path.local() {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            OutputFile::Local(options.open(path)?)
        } else {
            let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE;
            OutputFile::Remote(
                remote(path)?
                    .open_mode(path.path(), flags, 0o600, OpenType::File)
                    .map_err(err)?,
            )
        };
        Ok(Self {
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
            file: BufWriter::with_capacity(64 * 1024, file),
            cancel,
        })
    }

    pub(super) fn close(mut self) -> Result<(), TransferError> {
        self.flush()?;
        match self.file.into_inner().map_err(|error| error.into_error())? {
            OutputFile::Local(file) => file.sync_all()?,
            OutputFile::Remote(mut file) => file.close().map_err(err)?,
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
