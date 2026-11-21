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
    report_at: Location,
    file: BufWriter<OutputFile>,
    cancel: Cancellation,
}

impl StagedFile {
    pub(super) fn open(path: &Location, cancel: Cancellation) -> Result<Self, TransferError> {
        Self::open_for(path, path, cancel)
    }

    pub(super) fn open_for(
        path: &Location,
        report_at: &Location,
        cancel: Cancellation,
    ) -> Result<Self, TransferError> {
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
                    .map_err(|error| AccessError::new(report_at, action, error))?,
            )
        } else {
            let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE;
            OutputFile::Remote(
                remote(path)
                    .map_err(|error| AccessError::new(report_at, action, error))?
                    .open_mode(path.path(), flags, 0o600, OpenType::File)
                    .map_err(|error| AccessError::new(report_at, action, error))?,
            )
        };
        Ok(Self {
            report_at: report_at.clone(),
            file: BufWriter::with_capacity(64 * 1024, file),
            cancel,
        })
    }

    #[cfg(target_os = "macos")]
    pub(super) fn open_at(
        path: &Location,
        report_at: &Location,
        offset: u64,
        expected: &super::transfer::EntryMetadata,
        cancel: Cancellation,
    ) -> Result<Self, TransferError> {
        use super::transfer::{EntryMetadata, LocalEntryMetadata};
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        cancel.check()?;
        let open = AccessAction::OpenFile;
        let metadata = AccessAction::ReadOpenMetadata;
        let seek_error = |error| AccessError::stream(report_at, AccessAction::SeekFile, error);
        let file = if let Some(path) = path.local() {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
                .map_err(|error| AccessError::new(report_at, open, error))?;
            let actual = file
                .metadata()
                .map_err(|error| AccessError::new(report_at, metadata, error))?;
            if actual.nlink() != 1
                || !expected.matches_file(&EntryMetadata::Local(LocalEntryMetadata::from_file(
                    &file, actual,
                )))
            {
                return Err(super::resume_copy::changed_copy());
            }
            file.seek(SeekFrom::Start(offset)).map_err(seek_error)?;
            OutputFile::Local(file)
        } else {
            let mut file = remote(path)
                .map_err(|error| AccessError::new(report_at, open, error))?
                .open_mode(path.path(), OpenFlags::WRITE, 0, OpenType::File)
                .map_err(|error| AccessError::new(report_at, open, error))?;
            let actual = file
                .stat()
                .map_err(|error| AccessError::new(report_at, metadata, error))?;
            if !expected.matches_file(&EntryMetadata::Remote(actual)) {
                return Err(super::resume_copy::changed_copy());
            }
            file.seek(SeekFrom::Start(offset)).map_err(seek_error)?;
            OutputFile::Remote(file)
        };
        Ok(Self {
            report_at: report_at.clone(),
            file: BufWriter::with_capacity(64 * 1024, file),
            cancel,
        })
    }

    pub(super) fn close(self) -> Result<(), TransferError> {
        self.cancel.check()?;
        let mut output = self
            .file
            .into_inner()
            .map_err(|error| AccessError::write(&self.report_at, error.into_error()))?;
        output.flush().map_err(|error| {
            AccessError::stream(&self.report_at, AccessAction::FlushFile, error)
        })?;
        match output {
            OutputFile::Local(file) => file.sync_all().map_err(|error| {
                AccessError::new(&self.report_at, AccessAction::SyncFile, error)
            })?,
            OutputFile::Remote(mut file) => file.close().map_err(|error| {
                AccessError::new(&self.report_at, AccessAction::CloseFile, error)
            })?,
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
