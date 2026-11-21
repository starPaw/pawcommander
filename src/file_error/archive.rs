use super::{ArchiveMemberError, PathError};
use crate::vfs::Location;
use std::{error::Error, fmt, io};

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
