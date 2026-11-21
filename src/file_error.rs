use crate::operations::TransferError;
use crate::vfs::Location;
use std::net::SocketAddr;
use std::{error::Error, fmt, io, path::PathBuf, str::Utf8Error};

mod archive;
mod checksum;
mod host_key;
mod space;
pub(crate) use archive::ArchiveError;
pub(crate) use checksum::{ChecksumError, ManifestLineError};
pub(crate) use host_key::{HostKeyError, KnownHostEntryError, KnownHostsError, PinError};
pub(crate) use space::{ProbeAction, SftpSpaceFailure, SpaceError};

#[derive(Debug)]
pub(crate) enum Failure {
    Message(String),
    Io {
        source: io::Error,
        message: String,
    },
    Path(PathError),
    Connection(Box<ConnectionError>),
    Access(Box<AccessError>),
    Preview(Box<PreviewError>),
    Search(Box<SearchError>),
    Scan(Box<ScanError>),
    Permissions(Box<PermissionError>),
    Checksum(Box<ChecksumError>),
    Archive(Box<ArchiveError>),
    Space(Box<SpaceError>),
    ComparisonPane {
        right: bool,
        cause: Box<TransferError>,
    },
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
            Self::Connection(error) => Some(error.as_ref()),
            Self::Access(error) => Some(error.as_ref()),
            Self::Preview(error) => Some(error.as_ref()),
            Self::Search(error) => Some(error.as_ref()),
            Self::Scan(error) => Some(error.as_ref()),
            Self::Permissions(error) => Some(error.as_ref()),
            Self::Checksum(error) => Some(error.as_ref()),
            Self::Archive(error) => Some(error.as_ref()),
            Self::Space(error) => Some(error.as_ref()),
            Self::ComparisonPane { cause, .. } => Some(cause.as_ref()),
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
            Self::Connection(error) => fmt::Display::fmt(error, f),
            Self::Access(error) => fmt::Display::fmt(error, f),
            Self::Preview(error) => fmt::Display::fmt(error, f),
            Self::Search(error) => fmt::Display::fmt(error, f),
            Self::Scan(error) => fmt::Display::fmt(error, f),
            Self::Permissions(error) => fmt::Display::fmt(error, f),
            Self::Checksum(error) => fmt::Display::fmt(error, f),
            Self::Archive(error) => fmt::Display::fmt(error, f),
            Self::Space(error) => fmt::Display::fmt(error, f),
            Self::ComparisonPane { right, cause } => {
                let pane = if *right { "Right pane" } else { "Left pane" };
                write!(f, "{pane} · {cause}")
            }
            Self::Listing(error) => fmt::Display::fmt(error, f),
            Self::ConflictClosed => f.write_str("Conflict dialog closed"),
            Self::Mutation(error) => fmt::Display::fmt(error, f),
            Self::Publication(error) => fmt::Display::fmt(error, f),
            Self::StagingCleanup(error) => fmt::Display::fmt(error, f),
        }
    }
}

#[derive(Debug)]
pub(crate) enum SshStage {
    Session,
    Handshake,
    AuthenticationMethods,
    Authentication,
    Agent,
    InteractiveAuthentication,
    Channel,
}

#[derive(Debug)]
pub(crate) enum ConnectionError {
    InvalidEndpoint,
    Resolve {
        host: String,
        source: io::Error,
    },
    Connect {
        endpoint: String,
        last: Option<(SocketAddr, io::Error)>,
    },
    Transport(io::Error),
    Ssh {
        stage: SshStage,
        source: ssh2::Error,
    },
    HostKey(HostKeyError),
    #[cfg(target_os = "macos")]
    Forward {
        target: String,
        source: ssh2::Error,
    },
    Route {
        jump: bool,
        address: String,
        cause: TransferError,
    },
    Automatic(TransferError),
    AuthenticationUnconfirmed {
        interactive: bool,
    },
    UnsupportedAuthentication,
    InteractiveTimeout,
    HostKeyTimeout,
    Finalized,
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint => f.write_str("Invalid SSH endpoint"),
            Self::Resolve { host, source } => write!(f, "Could not resolve {host}: {source}"),
            Self::Connect { endpoint, last } => {
                write!(f, "Could not connect to {endpoint}")?;
                if let Some((address, source)) = last {
                    write!(f, " ({address}): {source}")?;
                }
                Ok(())
            }
            Self::Transport(source) => write!(f, "Could not configure the SSH transport: {source}"),
            Self::Ssh { stage, source } => write!(f, "SSH {stage:?}: {source}"),
            Self::HostKey(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::Forward { target, source } => {
                write!(f, "Could not forward TCP to {target}: {source}")
            }
            Self::Route {
                jump,
                address,
                cause,
            } => {
                let role = if *jump { "Jump host" } else { "Target host" };
                write!(f, "{role} {address} · {cause}")
            }
            Self::Automatic(cause) => write!(f, "Automatic SFTP connection: {cause}"),
            Self::AuthenticationUnconfirmed { .. } => f.write_str("Authentication unconfirmed"),
            Self::UnsupportedAuthentication => {
                f.write_str("No supported SSH authentication method")
            }
            Self::InteractiveTimeout => f.write_str("Interactive SSH authentication timed out"),
            Self::HostKeyTimeout => f.write_str("SSH host key verification timed out"),
            Self::Finalized => f.write_str("SSH connection already finalized"),
        }
    }
}

impl Error for ConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Resolve { source, .. } | Self::Transport(source) => Some(source),
            Self::Connect { last, .. } => last.as_ref().map(|(_, error)| error as &dyn Error),
            Self::Ssh { source, .. } => Some(source),
            Self::HostKey(error) => Some(error),
            #[cfg(target_os = "macos")]
            Self::Forward { source, .. } => Some(source),
            Self::Route { cause, .. } | Self::Automatic(cause) => Some(cause),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessAction {
    ResolvePath,
    ReadMetadata,
    OpenFile,
    CreateFile,
    ReadOpenMetadata,
    ReadChildren,
    ReadFile,
    ReadLink,
    WriteFile,
    FlushFile,
    SyncFile,
    CloseFile,
    SeekFile,
    WriteMetadata,
    CheckSearchResult,
    #[cfg(target_os = "macos")]
    WritePreviewCache,
}

#[derive(Debug)]
pub(crate) enum AccessCause {
    Local(io::Error),
    Sftp(ssh2::Error),
    SftpStream(io::Error),
    #[cfg(target_os = "macos")]
    Path(PathError),
    Connection(Box<TransferError>),
}

impl From<io::Error> for AccessCause {
    fn from(error: io::Error) -> Self {
        Self::Local(error)
    }
}

impl From<ssh2::Error> for AccessCause {
    fn from(error: ssh2::Error) -> Self {
        Self::Sftp(error)
    }
}

#[cfg(target_os = "macos")]
impl From<PathError> for AccessCause {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl From<TransferError> for AccessCause {
    fn from(error: TransferError) -> Self {
        Self::Connection(Box::new(error))
    }
}

impl fmt::Display for AccessCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(error) | Self::SftpStream(error) => fmt::Display::fmt(error, f),
            Self::Sftp(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::Path(error) => fmt::Display::fmt(error, f),
            Self::Connection(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl AccessCause {
    pub(crate) fn is_absent(&self) -> bool {
        match self {
            Self::Local(error) | Self::SftpStream(error) => error.kind() == io::ErrorKind::NotFound,
            Self::Sftp(error) => {
                error.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_NO_SUCH_FILE)
            }
            #[cfg(target_os = "macos")]
            Self::Path(_) => false,
            Self::Connection(_) => false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct AccessError {
    pub(crate) location: Location,
    pub(crate) action: AccessAction,
    pub(crate) cause: AccessCause,
}

impl AccessError {
    pub(crate) fn new(
        location: &Location,
        action: AccessAction,
        cause: impl Into<AccessCause>,
    ) -> Self {
        Self {
            location: location.clone(),
            action,
            cause: cause.into(),
        }
    }

    pub(crate) fn read(location: &Location, source: io::Error) -> Self {
        Self::stream(location, AccessAction::ReadFile, source)
    }

    pub(crate) fn write(location: &Location, source: io::Error) -> Self {
        Self::stream(location, AccessAction::WriteFile, source)
    }

    pub(crate) fn stream(location: &Location, action: AccessAction, source: io::Error) -> Self {
        let cause = if location.remote() {
            AccessCause::SftpStream(source)
        } else {
            AccessCause::Local(source)
        };
        Self::new(location, action, cause)
    }
}

impl fmt::Display for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} at {}: {}", self.action, self.location, self.cause)
    }
}

impl Error for AccessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.cause {
            AccessCause::Local(error) | AccessCause::SftpStream(error) => Some(error),
            AccessCause::Sftp(error) => Some(error),
            #[cfg(target_os = "macos")]
            AccessCause::Path(error) => Some(error),
            AccessCause::Connection(error) => Some(error.as_ref()),
        }
    }
}

#[derive(Debug)]
pub(crate) enum PreviewError {
    UnsupportedFile(Location),
    ChangedBeforeRead(Location),
    ChangedWhileReading(Location),
    #[cfg(target_os = "macos")]
    MediaLimit {
        location: Location,
        limit: u64,
    },
    WorkerUnavailable,
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFile(location) => write!(f, "Unsupported preview file: {location}"),
            Self::ChangedBeforeRead(location) => {
                write!(f, "File changed before reading: {location}")
            }
            Self::ChangedWhileReading(location) => {
                write!(f, "File changed while reading: {location}")
            }
            #[cfg(target_os = "macos")]
            Self::MediaLimit { location, limit } => {
                write!(f, "Media preview exceeds {limit} bytes: {location}")
            }
            Self::WorkerUnavailable => f.write_str("Preview worker is unavailable"),
        }
    }
}

impl Error for PreviewError {}

#[derive(Debug)]
pub(crate) enum SearchError {
    RequiresSftpConnection(Location),
    DifferentSftpConnection(Location),
    DirectoryChanged {
        location: Location,
        resolved: PathBuf,
    },
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiresSftpConnection(location) => {
                write!(f, "Search requires an SFTP connection for {location}")
            }
            Self::DifferentSftpConnection(location) => write!(
                f,
                "Search result {location} belongs to a different SFTP connection"
            ),
            Self::DirectoryChanged { location, resolved } => write!(
                f,
                "Folder {location} now resolves to {}",
                resolved.display()
            ),
        }
    }
}

impl Error for SearchError {}

#[derive(Debug)]
pub(crate) enum ScanError {
    PropertiesStart(io::Error),
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    DirectoryChanged(Location),
    InvalidSftpEntry {
        directory: Location,
        name: PathBuf,
    },
    DepthLimit(Location),
    FolderUnavailable(Location),
    DuplicateEntry {
        directory: Location,
        name: std::ffi::OsString,
    },
    UnsupportedComparisonType(Location),
    MissingComparisonMetadata(Location),
    ComparisonLimit,
    FileChanged(Location),
    ExpectedRegularFile(Location),
    MissingFileVersion(Location),
    #[cfg(target_os = "macos")]
    MissingLocalFileSize,
    #[cfg(target_os = "macos")]
    MissingSftpFileSize(Location),
    MissingSftpFileType(Location),
    #[cfg(target_os = "macos")]
    DiskUsageRequiresLocalFolder,
    #[cfg(target_os = "macos")]
    DiskUsageLimit,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PropertiesStart(source) => write!(f, "Calculation not started: {source}"),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::DirectoryChanged(location) => {
                write!(f, "Folder {location} changed during calculation")
            }
            Self::InvalidSftpEntry { directory, name } => {
                write!(f, "Invalid entry {name:?} in {directory}")
            }
            Self::DepthLimit(location) => write!(f, "Folder nesting limit reached at {location}"),
            Self::FolderUnavailable(location) => {
                write!(f, "Folder {location} changed or is unavailable")
            }
            Self::DuplicateEntry { directory, name } => {
                write!(f, "Duplicate entry {name:?} in {directory}")
            }
            Self::UnsupportedComparisonType(location) => {
                write!(f, "Cannot compare this item type at {location}")
            }
            Self::MissingComparisonMetadata(location) => {
                write!(f, "Missing file size or modified time for {location}")
            }
            Self::ComparisonLimit => {
                f.write_str("Folder comparison exceeded its entry or path limit")
            }
            Self::FileChanged(location) => write!(f, "File {location} changed while reading"),
            Self::ExpectedRegularFile(location) => {
                write!(f, "Expected a regular file at {location}")
            }
            Self::MissingFileVersion(location) => write!(
                f,
                "The server omitted the size or modification time for {location}"
            ),
            #[cfg(target_os = "macos")]
            Self::MissingLocalFileSize => f.write_str("File size is unavailable"),
            #[cfg(target_os = "macos")]
            Self::MissingSftpFileSize(location) => {
                write!(f, "Missing SFTP file size for {location}")
            }
            Self::MissingSftpFileType(location) => {
                write!(f, "Missing SFTP file type for {location}")
            }
            #[cfg(target_os = "macos")]
            Self::DiskUsageRequiresLocalFolder => {
                f.write_str("Disk usage analysis needs a local folder")
            }
            #[cfg(target_os = "macos")]
            Self::DiskUsageLimit => f.write_str("Disk usage analysis reached its scan limit"),
        }
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PropertiesStart(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum PermissionError {
    InvalidRule,
    InvalidSelection,
    FolderChanged(Location),
    ItemChanged(Location),
    MissingMode(Location),
    UnsupportedType(Location),
    SetMode {
        location: Location,
        source: ssh2::Error,
    },
    ModeMismatch {
        location: Location,
        expected: u32,
        actual: u32,
    },
    VerifyMode(TransferError),
}

impl fmt::Display for PermissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRule => f.write_str("Invalid permission rule"),
            Self::InvalidSelection => f.write_str("Selection must be within one SFTP folder"),
            Self::FolderChanged(location) => write!(f, "SFTP folder {location} changed"),
            Self::ItemChanged(location) => write!(f, "SFTP item {location} changed"),
            Self::MissingMode(location) => write!(f, "Missing permissions for {location}"),
            Self::UnsupportedType(location) => write!(f, "Unsupported item type at {location}"),
            Self::SetMode { location, source } => {
                write!(f, "Could not change permissions on {location}: {source}")
            }
            Self::ModeMismatch {
                location,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "The server set {actual:04o} instead of {expected:04o} on {location}"
                )
            }
            Self::VerifyMode(cause) => write!(f, "Could not verify permissions: {cause}"),
        }
    }
}

impl Error for PermissionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SetMode { source, .. } => Some(source),
            Self::VerifyMode(cause) => Some(cause),
            _ => None,
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
    DestinationExists(Location),
    SourceChanged(Location),
    ReadOnlyArchive(Location),
    RenameLocal {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },
    OpenSftpRename {
        from: Location,
        to: Location,
        reason: TransferError,
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
        reason: TransferError,
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
            Self::DestinationExists(location) => write!(f, "Destination {location} already exists"),
            Self::SourceChanged(location) => write!(f, "Source changed during copying: {location}"),
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
            Self::OpenSftpRename { reason, .. } | Self::OpenSftpRemoval { reason, .. } => {
                Some(reason)
            }
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
