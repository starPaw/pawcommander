use crate::operations::TransferError;
use crate::vfs::Location;
use std::{error::Error, fmt, io, path::PathBuf, str::Utf8Error};

#[derive(Debug)]
pub(crate) enum Failure {
    Message(String),
    Io { source: io::Error, message: String },
    Path(PathError),
    Listing(Box<ListingError>),
    ConflictClosed,
    Mutation(Box<MutationError>),
    Publication(Box<PublicationError>),
    StagingCleanup(Box<StagingCleanupError>),
}

impl From<MutationError> for Failure {
    fn from(error: MutationError) -> Self {
        Self::Mutation(Box::new(error))
    }
}

impl From<PublicationError> for Failure {
    fn from(error: PublicationError) -> Self {
        Self::Publication(Box::new(error))
    }
}

impl Failure {
    pub(crate) fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Message(_) | Self::ConflictClosed => None,
            Self::Io { source, .. } => Some(source),
            Self::Path(error) => Some(error),
            Self::Listing(error) => Some(error.as_ref()),
            Self::Mutation(error) => Some(error.as_ref()),
            Self::Publication(error) => Some(error.as_ref()),
            Self::StagingCleanup(error) => Some(error.as_ref()),
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
            Self::Mutation(error) => fmt::Display::fmt(error, f),
            Self::Publication(error) => fmt::Display::fmt(error, f),
            Self::StagingCleanup(error) => fmt::Display::fmt(error, f),
        }
    }
}

#[derive(Debug)]
pub(crate) enum RecoveryNotice {
    InspectStaging,
    #[cfg(target_os = "macos")]
    ResumeCopy,
    #[cfg(target_os = "macos")]
    ExternalEditor,
}

#[derive(Debug)]
pub(crate) enum PublicationError {
    DestinationChanged(Location),
    PayloadNotRegular(Location),
    Backup {
        target: Location,
        backup: Location,
        cause: TransferError,
    },
    Rollback {
        target: Location,
        backup: Location,
        cause: TransferError,
        restore: TransferError,
    },
    Retained {
        staging: Location,
        notice: RecoveryNotice,
        cause: TransferError,
    },
}

impl fmt::Display for PublicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DestinationChanged(target) => write!(
                f,
                "Destination {target} changed after the conflict dialog was shown"
            ),
            Self::PayloadNotRegular(payload) => {
                write!(f, "Source item {payload} is no longer a regular file")
            }
            Self::Backup {
                target,
                backup,
                cause,
            } => {
                write!(
                    f,
                    "Could not move the previous item from {target} to {backup}: {cause}"
                )
            }
            Self::Rollback {
                target,
                backup,
                cause,
                restore,
            } => write!(
                f,
                "{cause}. Could not restore the previous item from {backup} to {target}: {restore}"
            ),
            Self::Retained { staging, cause, .. } => {
                write!(
                    f,
                    "{cause}. Temporary folder retained for inspection at {staging}"
                )
            }
        }
    }
}

impl Error for PublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backup { cause, .. } | Self::Retained { cause, .. } => Some(cause),
            Self::Rollback { cause, restore, .. } => {
                if matches!(cause, TransferError::Cancelled) {
                    Some(restore)
                } else {
                    Some(cause)
                }
            }
            Self::DestinationChanged(_) | Self::PayloadNotRegular(_) => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct StagingCleanupError {
    pub(crate) staging: Location,
    pub(crate) target: Location,
    pub(crate) operation: Option<TransferError>,
    pub(crate) cleanup: TransferError,
}

impl StagingCleanupError {
    pub(crate) fn combine(
        result: Result<(), TransferError>,
        cleanup: Result<(), TransferError>,
        staging: &Location,
        target: &Location,
    ) -> Result<(), TransferError> {
        let Err(cleanup) = cleanup else {
            return result;
        };
        let unconfirmed = matches!(result, Err(TransferError::PublicationUnconfirmed(_)));
        let reason = Failure::StagingCleanup(Box::new(Self {
            operation: result.err(),
            cleanup,
            staging: staging.clone(),
            target: target.clone(),
        }));
        Err(if unconfirmed {
            TransferError::PublicationUnconfirmed(reason)
        } else {
            TransferError::Failed(reason)
        })
    }
}

impl fmt::Display for StagingCleanupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(error) = &self.operation {
            write!(f, "{error}. ")?;
        } else {
            write!(f, "Item {} was published. ", self.target)?;
        }
        if matches!(self.cleanup, TransferError::PublicationUnconfirmed(_)) {
            write!(
                f,
                "Cleanup at {} is unconfirmed: {}",
                self.staging, self.cleanup
            )
        } else {
            write!(
                f,
                "Temporary folder remains at {}: {}",
                self.staging, self.cleanup
            )
        }
    }
}

impl Error for StagingCleanupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self.operation.as_ref() {
            Some(error) if !matches!(error, TransferError::Cancelled) => Some(error),
            _ => Some(&self.cleanup),
        }
    }
}

#[derive(Debug)]
pub(crate) enum LocalPathAction {
    OpenDirectory,
    ResolveParent,
    ResolveEntry,
    ReadMetadata,
    CheckDestination,
    CreateDirectory,
}

#[derive(Debug)]
pub(crate) enum SftpRemovalAction {
    Inspect,
    RemoveFile,
    RemoveDirectory,
}

#[derive(Debug)]
pub(crate) enum MutationError {
    LocalIo {
        path: PathBuf,
        action: LocalPathAction,
        source: io::Error,
    },
    CurrentDirectory {
        input: PathBuf,
        source: io::Error,
    },
    NotDirectory(PathBuf),
    ModifyRoot(PathBuf),
    RenameRoot(Location),
    UndoUnavailable(Location),
    DestinationExists(PathBuf),
    ReadOnlyArchive(Location),
    RenameLocal {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },
    OpenSftpRename {
        from: Location,
        to: Location,
        reason: String,
    },
    RenameSftp {
        from: Location,
        to: Location,
        source: ssh2::Error,
    },
    CreateSftp {
        destination: Location,
        source: ssh2::Error,
    },
    Trash {
        path: PathBuf,
        source: trash::Error,
    },
    InvalidSftpRemoval(Location),
    SftpRemovalDepth(Location),
    OpenSftpRemoval {
        location: Location,
        reason: String,
    },
    RemoveSftp {
        location: Location,
        action: SftpRemovalAction,
        source: ssh2::Error,
    },
}

impl MutationError {
    pub(crate) fn local(
        path: impl Into<PathBuf>,
        action: LocalPathAction,
        source: io::Error,
    ) -> Self {
        Self::LocalIo {
            path: path.into(),
            action,
            source,
        }
    }
}

impl fmt::Display for MutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalIo {
                path,
                action,
                source,
            } => write!(f, "{action:?} at {path:?}: {source}"),
            Self::CurrentDirectory { input, source } => {
                write!(
                    f,
                    "Could not determine the current folder for {input:?}: {source}"
                )
            }
            Self::NotDirectory(path) => write!(f, "{path:?} is not a folder"),
            Self::ModifyRoot(path) => write!(f, "Cannot modify the filesystem root {path:?}"),
            Self::RenameRoot(path) => write!(f, "Cannot rename the root {}", path.path().display()),
            Self::UndoUnavailable(path) => {
                write!(f, "Undo unavailable for {}", path.path().display())
            }
            Self::DestinationExists(path) => write!(f, "Destination {path:?} already exists"),
            Self::ReadOnlyArchive(path) => write!(f, "Archive location {path} is read-only"),
            Self::RenameLocal { from, to, source } => {
                write!(
                    f,
                    "Could not rename {from:?} to {to:?} without replacement: {source}"
                )
            }
            Self::OpenSftpRename { from, to, reason } => {
                write!(f, "Could not open SFTP to rename {from} to {to}: {reason}")
            }
            Self::RenameSftp { from, to, source } => {
                write!(f, "SFTP rename from {from} to {to}: {source}")
            }
            Self::CreateSftp {
                destination,
                source,
            } => {
                write!(
                    f,
                    "Could not create the SFTP folder {}: {source}",
                    destination.path().display()
                )
            }
            Self::Trash { path, source } => {
                write!(f, "Could not move {path:?} to Trash: {source}")
            }
            Self::InvalidSftpRemoval(location) => {
                write!(f, "Invalid SFTP deletion path {location}")
            }
            Self::SftpRemovalDepth(location) => {
                write!(f, "Folder nesting is too deep at {location}")
            }
            Self::OpenSftpRemoval { location, reason } => {
                write!(
                    f,
                    "Could not open SFTP for deletion at {location}: {reason}"
                )
            }
            Self::RemoveSftp {
                location,
                action,
                source,
            } => write!(f, "{action:?} at {location}: {source}"),
        }
    }
}

impl Error for MutationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LocalIo { source, .. }
            | Self::CurrentDirectory { source, .. }
            | Self::RenameLocal { source, .. } => Some(source),
            Self::CreateSftp { source, .. }
            | Self::RemoveSftp { source, .. }
            | Self::RenameSftp { source, .. } => Some(source),
            Self::Trash { source, .. } => Some(source),
            _ => None,
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
    InvalidLocalName(String),
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
            Self::InvalidName(name) | Self::InvalidLocalName(name) => {
                write!(f, "Invalid file name {name:?}")
            }
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
