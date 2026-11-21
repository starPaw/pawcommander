use super::{ArchiveMemberError, PathError};
use crate::vfs::Location;
use std::{error::Error, fmt, io};

#[derive(Debug)]
pub(crate) enum ArchiveError {
    InvalidName(PathError),
    SelectCurrentFolder,
    PasswordRequiresZip,
    UnsupportedFormat(Location),
    UnsupportedEntry(Location),
    MissingEntrySize(Location),
    EntryEncoding(Location),
    EntryPath {
        entry: Location,
        source: ArchiveMemberError,
    },
    AddEntry {
        archive: Location,
        entry: Location,
        source: io::Error,
    },
    Verify {
        archive: Location,
        source: io::Error,
    },
    ChangedDuringVerification(Location),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(source) => write!(f, "Invalid archive name: {source}"),
            Self::SelectCurrentFolder => {
                f.write_str("Select files or folders from the current folder")
            }
            Self::PasswordRequiresZip => f.write_str("Only ZIP archives support passwords"),
            Self::UnsupportedFormat(location) => {
                write!(f, "Unsupported archive format: {location}")
            }
            Self::UnsupportedEntry(entry) => write!(f, "Unsupported archive entry: {entry}"),
            Self::MissingEntrySize(entry) => write!(f, "Missing SFTP file size for {entry}"),
            Self::EntryEncoding(entry) => write!(f, "Archive entry name must be UTF-8: {entry}"),
            Self::EntryPath { entry, source } => write!(f, "{entry}: {source}"),
            Self::AddEntry {
                archive,
                entry,
                source,
            } => {
                write!(f, "Could not add {entry} to archive {archive}: {source}")
            }
            Self::Verify { archive, source } => write!(f, "Could not verify {archive}: {source}"),
            Self::ChangedDuringVerification(archive) => {
                write!(f, "Archive {archive} changed during verification")
            }
        }
    }
}

impl Error for ArchiveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidName(source) => Some(source),
            Self::EntryPath { source, .. } => Some(source),
            Self::AddEntry { source, .. } | Self::Verify { source, .. } => Some(source),
            _ => None,
        }
    }
}
