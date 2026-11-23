use super::{ArchiveMemberError, PathError};
use crate::vfs::Location;
use std::{error::Error, fmt, io};

/// A diagnostic produced by archive readers and writers, before presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveIssue {
    FileDirectoryCollision,
    InvalidEntry,
    ZipHeaderMismatch,
    ZipHeaderSizeOrCrcMismatch,
    InvalidPath,
    MetadataLimit,
    NotRegularFile,
    InvalidZipStructure,
    RebuiltEntryListMismatch,
    UnsupportedRebuildFormat,
    UnsupportedZipEditMethod,
    UnsafeZipRebuild,
    EntrySizeMismatch,
    UnsupportedSparseTarRebuild,
    TarHardLinkToRemovedEntry,
    InvalidTarExtendedHeader,
    UnsafeTarGlobalMetadata,
    InvalidZipPasswordOrData,
    UnsupportedZipEncryptionOrCompression,
    UnsupportedZipCompression,
    UnsupportedExtractionEntry,
    CorruptEntryData,
    ZipCrcMismatch,
    OverlappingZipEntries,
    ZipCheckPasswordRequired,
    InvalidTarLength,
    UnverifiableSparseTar,
    MissingTarTrailer,
    UnexpectedTarTrailerData,
    ZipExtractionPasswordRequired,
    InvalidZipPassword,
    PasswordRequiresZip,
}

impl ArchiveIssue {
    fn io_kind(self) -> io::ErrorKind {
        match self {
            Self::UnsupportedZipEncryptionOrCompression
            | Self::UnsupportedZipCompression
            | Self::UnsupportedExtractionEntry
            | Self::ZipCheckPasswordRequired
            | Self::UnverifiableSparseTar => io::ErrorKind::Unsupported,
            Self::ZipExtractionPasswordRequired | Self::InvalidZipPassword => {
                io::ErrorKind::InvalidInput
            }
            _ => io::ErrorKind::InvalidData,
        }
    }

    pub(crate) fn io_error(self) -> io::Error {
        io::Error::new(
            self.io_kind(),
            ArchiveIoError {
                issue: self,
                source: None,
            },
        )
    }

    pub(crate) fn with_source(self, source: impl Error + Send + Sync + 'static) -> io::Error {
        io::Error::new(
            self.io_kind(),
            ArchiveIoError {
                issue: self,
                source: Some(Box::new(source)),
            },
        )
    }
}

impl fmt::Display for ArchiveIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FileDirectoryCollision => "The same archive path is used for a folder and a file",
            Self::InvalidEntry => "Invalid archive entry",
            Self::ZipHeaderMismatch => "ZIP local header disagrees with the directory",
            Self::ZipHeaderSizeOrCrcMismatch => "ZIP headers disagree on size or CRC",
            Self::InvalidPath => "Invalid path inside the archive",
            Self::MetadataLimit => "Archive contains too much metadata to browse",
            Self::NotRegularFile => "Archive must be a regular file",
            Self::InvalidZipStructure => "Invalid ZIP structure",
            Self::RebuiltEntryListMismatch => "The rebuilt archive has an unexpected entry list",
            Self::UnsupportedRebuildFormat => "Unsupported archive rebuild format",
            Self::UnsupportedZipEditMethod => "Editing this ZIP method is unsupported",
            Self::UnsafeZipRebuild => "This ZIP structure cannot be rebuilt safely",
            Self::EntrySizeMismatch => "Archive entry size does not match",
            Self::UnsupportedSparseTarRebuild => "Rebuilding a sparse TAR is unsupported",
            Self::TarHardLinkToRemovedEntry => {
                "A TAR hard link refers to an entry selected for removal"
            }
            Self::InvalidTarExtendedHeader => "Damaged TAR extension header",
            Self::UnsafeTarGlobalMetadata => "TAR global paths or sizes cannot be rebuilt safely",
            Self::InvalidZipPasswordOrData => "Incorrect password or damaged ZIP data",
            Self::UnsupportedZipEncryptionOrCompression => {
                "ZIP encryption or compression method is unsupported"
            }
            Self::UnsupportedZipCompression => "ZIP compression method is unsupported",
            Self::UnsupportedExtractionEntry => "Only regular files and folders can be extracted",
            Self::CorruptEntryData => "Archive entry data is corrupt",
            Self::ZipCrcMismatch => "ZIP CRC does not match",
            Self::OverlappingZipEntries => "ZIP entry data overlaps",
            Self::ZipCheckPasswordRequired => "Enter a password to check this ZIP",
            Self::InvalidTarLength => "Invalid TAR length",
            Self::UnverifiableSparseTar => "This sparse TAR entry requires a different verifier",
            Self::MissingTarTrailer => "TAR end blocks are missing",
            Self::UnexpectedTarTrailerData => "Unexpected data after the TAR end blocks",
            Self::ZipExtractionPasswordRequired => "Enter a password to extract this ZIP",
            Self::InvalidZipPassword => "Incorrect ZIP password",
            Self::PasswordRequiresZip => "Passwords are supported for ZIP archives only",
        })
    }
}

#[derive(Debug)]
pub(crate) struct ArchiveIoError {
    pub(crate) issue: ArchiveIssue,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ArchiveIoError {
    pub(crate) fn from_io(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }
}

impl fmt::Display for ArchiveIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.issue.fmt(f)?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl Error for ArchiveIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|error| error as _)
    }
}

#[derive(Debug)]
pub(crate) enum ArchiveError {
    InvalidName(PathError),
    SelectCurrentFolder,
    SelectArchiveEntries,
    SelectOtherPane,
    PasswordRequiresZip,
    UnsupportedFormat(Location),
    UnsupportedEntry(Location),
    MissingEntrySize(Location),
    EntryEncoding(Location),
    MemberMissing(Location),
    MemberOccupied(Location),
    MemberNameUnchanged(Location),
    CannotAddArchiveToItself(Location),
    AdditionsRequirePassword(Location),
    ExtractionLimit(Location),
    ExtractionDestinationChanged(Location),
    ExtractionReplaceUnavailable(Location),
    #[cfg(target_os = "macos")]
    SmbEditingUnavailable(Location),
    #[cfg(target_os = "macos")]
    SmbExtractionFolder {
        entry: Location,
        destination: Location,
    },
    #[cfg(target_os = "macos")]
    StageOutput {
        target: Location,
        source: io::Error,
    },
    EntryPath {
        entry: Location,
        source: ArchiveMemberError,
    },
    AddEntry {
        archive: Location,
        entry: Location,
        source: io::Error,
    },
    Read {
        archive: Location,
        source: io::Error,
    },
    ExtractEntry {
        entry: Location,
        destination: Location,
        source: io::Error,
    },
    Rebuild {
        location: Location,
        source: io::Error,
    },
    Verify {
        archive: Location,
        source: io::Error,
    },
    ChangedDuringVerification(Location),
    SourceChanged(Location),
}

impl ArchiveError {
    pub(crate) fn io_cause(&self) -> Option<&io::Error> {
        match self {
            #[cfg(target_os = "macos")]
            Self::StageOutput { source, .. } => Some(source),
            Self::AddEntry { source, .. }
            | Self::Read { source, .. }
            | Self::ExtractEntry { source, .. }
            | Self::Rebuild { source, .. }
            | Self::Verify { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(source) => write!(f, "Invalid archive name: {source}"),
            Self::SelectCurrentFolder => {
                f.write_str("Select files or folders from the current folder")
            }
            Self::SelectArchiveEntries => f.write_str("Select entries from one archive folder"),
            Self::SelectOtherPane => f.write_str("Select files or folders in the other pane"),
            Self::PasswordRequiresZip => f.write_str("Only ZIP archives support passwords"),
            Self::UnsupportedFormat(location) => {
                write!(f, "Unsupported archive format: {location}")
            }
            Self::UnsupportedEntry(entry) => write!(f, "Unsupported archive entry: {entry}"),
            Self::MissingEntrySize(entry) => write!(f, "Missing SFTP file size for {entry}"),
            Self::EntryEncoding(entry) => write!(f, "Archive entry name must be UTF-8: {entry}"),
            Self::MemberMissing(entry) => write!(f, "Archive entry no longer exists: {entry}"),
            Self::MemberOccupied(entry) => {
                write!(f, "Path is already used in the archive: {entry}")
            }
            Self::MemberNameUnchanged(entry) => {
                write!(f, "Archive entry name is unchanged: {entry}")
            }
            Self::CannotAddArchiveToItself(archive) => {
                write!(f, "Cannot add archive to itself: {archive}")
            }
            Self::AdditionsRequirePassword(archive) => {
                write!(f, "A password is required for added ZIP files: {archive}")
            }
            Self::ExtractionLimit(entry) => {
                write!(f, "Archive extraction exceeds 32 GiB at {entry}")
            }
            Self::ExtractionDestinationChanged(location) => {
                write!(f, "Extraction folder changed: {location}")
            }
            Self::ExtractionReplaceUnavailable(location) => {
                write!(f, "Cannot replace {location} during extraction")
            }
            #[cfg(target_os = "macos")]
            Self::SmbEditingUnavailable(location) => {
                write!(
                    f,
                    "Safe archive replacement is unavailable on SMB at {location}"
                )
            }
            #[cfg(target_os = "macos")]
            Self::SmbExtractionFolder { entry, destination } => {
                write!(
                    f,
                    "Cannot extract folder {entry} to SMB destination {destination}"
                )
            }
            #[cfg(target_os = "macos")]
            Self::StageOutput { target, source } => {
                write!(f, "Could not prepare temporary data for {target}: {source}")
            }
            Self::EntryPath { entry, source } => write!(f, "{entry}: {source}"),
            Self::AddEntry {
                archive,
                entry,
                source,
            } => {
                write!(f, "Could not add {entry} to archive {archive}: {source}")
            }
            Self::Read { archive, source } => {
                write!(f, "Could not read archive {archive}: {source}")
            }
            Self::ExtractEntry {
                entry,
                destination,
                source,
            } => {
                write!(f, "Could not extract {entry} to {destination}: {source}")
            }
            Self::Rebuild { location, source } => {
                write!(f, "Could not rebuild archive at {location}: {source}")
            }
            Self::Verify { archive, source } => write!(f, "Could not verify {archive}: {source}"),
            Self::ChangedDuringVerification(archive) => {
                write!(f, "Archive {archive} changed during verification")
            }
            Self::SourceChanged(archive) => {
                write!(f, "The archive changed during the operation: {archive}")
            }
        }
    }
}

impl Error for ArchiveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidName(source) => Some(source),
            Self::EntryPath { source, .. } => Some(source),
            _ => self.io_cause().map(|source| source as _),
        }
    }
}
