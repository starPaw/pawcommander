use crate::vfs::Location;
use std::{error::Error, fmt, io, path::PathBuf, str::Utf8Error};

#[derive(Debug)]
pub(crate) enum Failure {
    Message(String),
    Io { source: io::Error, message: String },
    Path(PathError),
    Listing(Box<ListingError>),
    ConflictClosed,
}

impl Failure {
    pub(crate) fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Message(_) | Self::ConflictClosed => None,
            Self::Io { source, .. } => Some(source),
            Self::Path(error) => Some(error),
            Self::Listing(error) => Some(error.as_ref()),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) | Self::Io { message, .. } => f.write_str(message),
            Self::Path(error) => fmt::Display::fmt(error, f),
            Self::Listing(error) => fmt::Display::fmt(error, f),
            Self::ConflictClosed => f.write_str("Conflict dialog closed"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ListingErrorKind {
    OpenLocal(io::Error),
    ReadLocalEntry(io::Error),
    LocalMetadata(io::Error),
    ArchiveFormat,
    ReadArchive(io::Error),
    MissingArchiveFolder,
    OpenSftp(ssh2::Error),
    ReadSftp(ssh2::Error),
    SftpNameEncoding(PathBuf),
    InvalidSftpName(PathError),
    RequiresSftpConnection,
}

#[derive(Debug)]
pub(crate) struct ListingError {
    pub(crate) location: Location,
    pub(crate) kind: ListingErrorKind,
}

impl ListingError {
    pub(crate) fn new(location: impl Into<Location>, kind: ListingErrorKind) -> Self {
        Self {
            location: location.into(),
            kind,
        }
    }
}

impl fmt::Display for ListingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ListingErrorKind::*;
        write!(f, "{:?}: ", self.location.path())?;
        match &self.kind {
            OpenLocal(source) => write!(f, "Could not open folder: {source}"),
            ReadLocalEntry(source) => write!(f, "Could not read folder entry: {source}"),
            LocalMetadata(source) => write!(f, "Could not read entry metadata: {source}"),
            ArchiveFormat => f.write_str("Unsupported archive format"),
            ReadArchive(source) => write!(f, "Could not read archive: {source}"),
            MissingArchiveFolder => f.write_str("Folder missing from archive"),
            OpenSftp(source) => write!(f, "Could not open SFTP folder: {source}"),
            ReadSftp(source) => write!(f, "Could not read SFTP folder entry: {source}"),
            SftpNameEncoding(name) => write!(f, "SFTP entry name {name:?} is not UTF-8"),
            InvalidSftpName(source) => fmt::Display::fmt(source, f),
            RequiresSftpConnection => f.write_str("Folder listing requires an open SFTP session"),
        }
    }
}

impl Error for ListingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        use ListingErrorKind::*;
        match &self.kind {
            OpenLocal(source)
            | ReadLocalEntry(source)
            | LocalMetadata(source)
            | ReadArchive(source) => Some(source),
            OpenSftp(source) | ReadSftp(source) => Some(source),
            InvalidSftpName(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveMemberError {
    InvalidPath,
    LimitExceeded,
}

impl fmt::Display for ArchiveMemberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPath => "Invalid path inside archive",
            Self::LimitExceeded => "Archive path exceeds the browsing limit",
        })
    }
}

impl Error for ArchiveMemberError {}

#[derive(Debug)]
pub(crate) enum PathError {
    ArchiveAddress,
    ArchiveFile(PathBuf),
    ArchiveMember {
        archive: PathBuf,
        source: ArchiveMemberError,
    },
    SftpUrl(url::ParseError),
    SftpUrlFields,
    MissingSftpHost,
    SftpUserEncoding(Utf8Error),
    MissingSftpUser,
    SftpPathEncoding(Utf8Error),
    SftpPathContainsNul(PathBuf),
    SftpHomeShorthand,
    RemotePathEncoding(PathBuf),
    RemotePathSyntax(PathBuf),
    InvalidName(String),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArchiveAddress => f.write_str("Invalid archive address"),
            Self::ArchiveFile(path) => write!(f, "Invalid archive file {path:?}"),
            Self::ArchiveMember { archive, source } => write!(f, "{archive:?}: {source}"),
            Self::SftpUrl(source) => write!(f, "Invalid SFTP address: {source}"),
            Self::SftpUrlFields => {
                f.write_str("Unexpected password, query or fragment in SFTP address")
            }
            Self::MissingSftpHost => f.write_str("Missing SFTP host"),
            Self::SftpUserEncoding(source) => write!(f, "Invalid SFTP user encoding: {source}"),
            Self::MissingSftpUser => f.write_str("Missing SFTP user"),
            Self::SftpPathEncoding(source) => write!(f, "Invalid SFTP path encoding: {source}"),
            Self::SftpPathContainsNul(path) => write!(f, "NUL in SFTP path {path:?}"),
            Self::SftpHomeShorthand => {
                f.write_str("SFTP address does not support home-directory shorthand")
            }
            Self::RemotePathEncoding(path) => write!(f, "SFTP path {path:?} is not UTF-8"),
            Self::RemotePathSyntax(path) => {
                write!(f, "Expected an absolute POSIX SFTP path, got {path:?}")
            }
            Self::InvalidName(name) => write!(f, "Invalid file name {name:?}"),
        }
    }
}

impl Error for PathError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ArchiveMember { source, .. } => Some(source),
            Self::SftpUrl(source) => Some(source),
            Self::SftpUserEncoding(source) | Self::SftpPathEncoding(source) => Some(source),
            _ => None,
        }
    }
}
