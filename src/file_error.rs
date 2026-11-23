use crate::operations::TransferError;
use crate::vfs::Location;
use std::net::SocketAddr;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::{error::Error, fmt, io, path::PathBuf, str::Utf8Error};

mod archive;
mod batch_rename;
mod checksum;
#[cfg(target_os = "macos")]
mod collection;
#[cfg(target_os = "macos")]
mod compatible_plan;
mod conflict;
mod copy_verification;
mod credential;
mod cross_move;
#[cfg(target_os = "macos")]
mod duplicates;
#[cfg(target_os = "macos")]
mod external_editor;
#[cfg(target_os = "macos")]
mod file_promise;
#[cfg(target_os = "macos")]
mod file_template;
mod host_key;
mod image_preview;
#[cfg(target_os = "macos")]
mod link_create;
#[cfg(target_os = "macos")]
mod media;
mod metadata;
mod move_history;
#[cfg(target_os = "macos")]
mod pdf;
mod recovery_record;
#[cfg(target_os = "macos")]
mod resume_check;
mod retry;
mod space;
mod staging;
#[cfg(target_os = "macos")]
mod sync;
mod text_preview;
mod transfer_plan;
mod undo;
pub(crate) use archive::{ArchiveError, ArchiveIoError, ArchiveIssue};
pub(crate) use batch_rename::BatchRenameError;
pub(crate) use checksum::{ChecksumError, ManifestLineError};
#[cfg(target_os = "macos")]
pub(crate) use collection::{CollectionAction, CollectionError, CollectionUnavailable};
#[cfg(target_os = "macos")]
pub(crate) use compatible_plan::CompatiblePlanError;
pub(crate) use conflict::{ConflictError, ReplacementRefusal};
pub(crate) use copy_verification::CopyVerificationError;
pub(crate) use credential::{
    CredentialAction, CredentialError, CredentialFailure, SecretFormatError,
};
pub(crate) use cross_move::CrossMoveError;
#[cfg(target_os = "macos")]
pub(crate) use duplicates::{DuplicateError, DuplicateRole, MacTrashError};
#[cfg(target_os = "macos")]
pub(crate) use external_editor::RemoteEditError;
#[cfg(target_os = "macos")]
pub(crate) use file_promise::FilePromiseError;
#[cfg(target_os = "macos")]
pub(crate) use file_template::FileTemplateError;
pub(crate) use host_key::{HostKeyError, KnownHostEntryError, KnownHostsError, PinError};
pub(crate) use image_preview::ImagePreviewError;
#[cfg(target_os = "macos")]
pub(crate) use link_create::{LinkCreateError, LinkIoAction};
#[cfg(target_os = "macos")]
pub(crate) use media::{MediaAction, MediaError, MediaNativeError};
pub(crate) use metadata::{MetadataError, MetadataField, MetadataWarning};
pub(crate) use move_history::{DescendantProblem, MoveHistoryError};
#[cfg(target_os = "macos")]
pub(crate) use pdf::{PdfAction, PdfError, PdfProblem};
#[cfg(target_os = "macos")]
pub(crate) use recovery_record::ResumeRecordEvent;
pub(crate) use recovery_record::{
    RecoveryRecordAction, RecoveryRecordError, RecoveryRecordProblem, RecoveryRecordSection,
};
#[cfg(target_os = "macos")]
pub(crate) use resume_check::{ResumeCheckError, ResumeCheckProblem, ResumeRole};
pub(crate) use retry::{RetryError, RetryProblem};
pub(crate) use space::{ProbeAction, SftpSpaceFailure, SpaceError};
pub(crate) use staging::{StagingError, StagingRecordAction};
#[cfg(target_os = "macos")]
pub(crate) use sync::{
    SyncDifference, SyncError, SyncExclusionProblem, SyncFolderRole, SyncStateAction,
    SyncStateKind, SyncStateProblem, SyncVerification,
};
pub(crate) use text_preview::TextDecodeError;
#[cfg(target_os = "macos")]
pub(crate) use transfer_plan::SmbTransferRole;
pub(crate) use transfer_plan::TransferPlanError;
pub(crate) use undo::UndoError;

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
    #[cfg(target_os = "macos")]
    Duplicate(Box<DuplicateError>),
    Permissions(Box<PermissionError>),
    Checksum(Box<ChecksumError>),
    CopyVerification(Box<CopyVerificationError>),
    Archive(Box<ArchiveError>),
    Metadata(Box<MetadataError>),
    Space(Box<SpaceError>),
    #[cfg(target_os = "macos")]
    Sync(Box<SyncError>),
    ComparisonPane {
        right: bool,
        cause: Box<TransferError>,
    },
    Listing(Box<ListingError>),
    ConflictClosed,
    Conflict(Box<ConflictError>),
    BatchRename(Box<BatchRenameError>),
    CrossMove(Box<CrossMoveError>),
    MoveHistory(Box<MoveHistoryError>),
    #[cfg(target_os = "macos")]
    RemoteEdit(Box<RemoteEditError>),
    Undo(Box<UndoError>),
    #[cfg(target_os = "macos")]
    FilePromise(Box<FilePromiseError>),
    #[cfg(target_os = "macos")]
    FileTemplate(Box<FileTemplateError>),
    #[cfg(target_os = "macos")]
    LinkCreate(Box<LinkCreateError>),
    #[cfg(target_os = "macos")]
    Collection(Box<CollectionError>),
    #[cfg(target_os = "macos")]
    CompatiblePlan(Box<CompatiblePlanError>),
    Mutation(Box<MutationError>),
    Publication(Box<PublicationError>),
    Staging(Box<StagingError>),
    #[cfg(target_os = "macos")]
    ResumeCheck(Box<ResumeCheckError>),
    Retry(Box<RetryError>),
    TransferPlan(Box<TransferPlanError>),
    RecoveryRecord(Box<RecoveryRecordError>),
    StagingCleanup(Box<StagingCleanupError>),
}

impl From<MutationError> for Failure {
    fn from(error: MutationError) -> Self {
        Self::Mutation(Box::new(error))
    }
}

impl From<ScanError> for Failure {
    fn from(error: ScanError) -> Self {
        Self::Scan(Box::new(error))
    }
}

impl From<PublicationError> for Failure {
    fn from(error: PublicationError) -> Self {
        Self::Publication(Box::new(error))
    }
}

impl Error for Failure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Message(_) | Self::ConflictClosed => None,
            Self::Io { source, .. } => Some(source),
            Self::Path(error) => Some(error),
            Self::Connection(error) => Some(error.as_ref()),
            Self::Access(error) => Some(error.as_ref()),
            Self::Preview(error) => Some(error.as_ref()),
            Self::Search(error) => Some(error.as_ref()),
            Self::Scan(error) => Some(error.as_ref()),
            #[cfg(target_os = "macos")]
            Self::Duplicate(error) => Some(error.as_ref()),
            Self::Permissions(error) => Some(error.as_ref()),
            Self::Checksum(error) => Some(error.as_ref()),
            Self::CopyVerification(error) => Some(error.as_ref()),
            Self::Archive(error) => Some(error.as_ref()),
            Self::Metadata(error) => Some(error.as_ref()),
            Self::Space(error) => Some(error.as_ref()),
            #[cfg(target_os = "macos")]
            Self::Sync(error) => Some(error.as_ref()),
            Self::ComparisonPane { cause, .. } => Some(cause.as_ref()),
            Self::Listing(error) => Some(error.as_ref()),
            Self::Conflict(error) => Some(error.as_ref()),
            Self::BatchRename(error) => Some(error.as_ref()),
            Self::CrossMove(error) => Some(error.as_ref()),
            Self::MoveHistory(error) => Some(error.as_ref()),
            #[cfg(target_os = "macos")]
            Self::RemoteEdit(error) => Some(error.as_ref()),
            Self::Undo(error) => Some(error.as_ref()),
            #[cfg(target_os = "macos")]
            Self::FilePromise(error) => Some(error.as_ref()),
            #[cfg(target_os = "macos")]
            Self::FileTemplate(error) => Some(error.as_ref()),
            #[cfg(target_os = "macos")]
            Self::LinkCreate(error) => Some(error.as_ref()),
            #[cfg(target_os = "macos")]
            Self::Collection(error) => Some(error.as_ref()),
            #[cfg(target_os = "macos")]
            Self::CompatiblePlan(error) => Some(error.as_ref()),
            Self::Mutation(error) => Some(error.as_ref()),
            Self::Publication(error) => Some(error.as_ref()),
            Self::Staging(error) => Some(error.as_ref()),
            #[cfg(target_os = "macos")]
            Self::ResumeCheck(error) => Some(error.as_ref()),
            Self::Retry(error) => Some(error.as_ref()),
            Self::TransferPlan(error) => Some(error.as_ref()),
            Self::RecoveryRecord(error) => Some(error.as_ref()),
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
            #[cfg(target_os = "macos")]
            Self::Duplicate(error) => fmt::Display::fmt(error, f),
            Self::Permissions(error) => fmt::Display::fmt(error, f),
            Self::Checksum(error) => fmt::Display::fmt(error, f),
            Self::CopyVerification(error) => fmt::Display::fmt(error, f),
            Self::Archive(error) => fmt::Display::fmt(error, f),
            Self::Metadata(error) => fmt::Display::fmt(error, f),
            Self::Space(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::Sync(error) => fmt::Display::fmt(error, f),
            Self::ComparisonPane { right, cause } => {
                let pane = if *right { "Right pane" } else { "Left pane" };
                write!(f, "{pane} · {cause}")
            }
            Self::Listing(error) => fmt::Display::fmt(error, f),
            Self::ConflictClosed => f.write_str("Conflict dialog closed"),
            Self::Conflict(error) => fmt::Display::fmt(error, f),
            Self::BatchRename(error) => fmt::Display::fmt(error, f),
            Self::CrossMove(error) => fmt::Display::fmt(error, f),
            Self::MoveHistory(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::RemoteEdit(error) => fmt::Display::fmt(error, f),
            Self::Undo(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::FilePromise(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::FileTemplate(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::LinkCreate(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::Collection(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::CompatiblePlan(error) => fmt::Display::fmt(error, f),
            Self::Mutation(error) => fmt::Display::fmt(error, f),
            Self::Publication(error) => fmt::Display::fmt(error, f),
            Self::Staging(error) => fmt::Display::fmt(error, f),
            #[cfg(target_os = "macos")]
            Self::ResumeCheck(error) => fmt::Display::fmt(error, f),
            Self::Retry(error) => fmt::Display::fmt(error, f),
            Self::TransferPlan(error) => fmt::Display::fmt(error, f),
            Self::RecoveryRecord(error) => fmt::Display::fmt(error, f),
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

#[derive(Clone, Copy, Debug)]
pub(crate) enum SftpStateLock {
    Sessions,
    OpeningRegistry,
    OpeningSession,
    DirectoryPoll,
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
    ExpectedSftp(Location),
    SftpStatePoisoned {
        endpoint: String,
        lock: SftpStateLock,
    },
    CachedChannel {
        endpoint: String,
        source: ssh2::Error,
    },
    Automatic {
        endpoint: String,
        previous_channel: Option<ssh2::Error>,
        cause: TransferError,
    },
    KeyPasswordConflict,
    KeyFileRequired,
    ImportedKeyNotFile(PathBuf),
    ImportedKeyTooLarge {
        path: PathBuf,
        limit: usize,
    },
    ImportedKeyEncoding {
        path: PathBuf,
        source: Utf8Error,
    },
    Credential(Box<CredentialError>),
    InitialPathNotDirectory(Location),
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
            Self::ExpectedSftp(location) => write!(f, "Expected an SFTP address at {location}"),
            Self::SftpStatePoisoned { endpoint, lock } => {
                write!(f, "SFTP {lock:?} lock poisoned for {endpoint}")
            }
            Self::CachedChannel { endpoint, source } => {
                write!(
                    f,
                    "Could not open an SFTP channel on the session for {endpoint}: {source}"
                )
            }
            Self::Automatic {
                endpoint,
                previous_channel,
                cause,
            } => {
                write!(
                    f,
                    "Could not open an SFTP connection for {endpoint}: {cause}"
                )?;
                if let Some(source) = previous_channel {
                    write!(f, " (previous channel: {source})")?;
                }
                Ok(())
            }
            Self::KeyPasswordConflict => f.write_str("Clear the account password to save the key"),
            Self::KeyFileRequired => f.write_str("Choose a private key file"),
            Self::ImportedKeyNotFile(path) => {
                write!(
                    f,
                    "The private key path {} must refer to a regular file",
                    path.display()
                )
            }
            Self::ImportedKeyTooLarge { path, limit } => {
                write!(
                    f,
                    "The private key file {} exceeds the {limit}-byte import limit",
                    path.display()
                )
            }
            Self::ImportedKeyEncoding { path, source } => {
                write!(
                    f,
                    "Invalid text encoding in the private key file {}: {source}",
                    path.display()
                )
            }
            Self::Credential(error) => fmt::Display::fmt(error, f),
            Self::InitialPathNotDirectory(location) => {
                write!(f, "The initial SFTP path {location} is not a folder")
            }
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
            Self::Ssh { source, .. } | Self::CachedChannel { source, .. } => Some(source),
            Self::HostKey(error) => Some(error),
            #[cfg(target_os = "macos")]
            Self::Forward { source, .. } => Some(source),
            Self::Route { cause, .. } | Self::Automatic { cause, .. } => Some(cause),
            Self::ImportedKeyEncoding { source, .. } => Some(source),
            Self::Credential(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessAction {
    ResolvePath,
    CopyEntry,
    ReadMetadata,
    OpenFile,
    CreateFile,
    CreateDirectory,
    CreateStagingDirectory,
    RemoveStagingDirectory,
    #[cfg(target_os = "macos")]
    RemoveStagingItem,
    ReadOpenMetadata,
    ReadChildren,
    ReadFile,
    ReadLink,
    CreateLink,
    WriteFile,
    FlushFile,
    SyncFile,
    CloseFile,
    SeekFile,
    WriteMetadata,
    WritePermissions,
    #[cfg(target_os = "macos")]
    CheckSmbVolume,
    #[cfg(target_os = "macos")]
    ReadVolumeLimits,
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
    Image {
        location: Location,
        cause: ImagePreviewError,
    },
    Text {
        location: Location,
        encoding: &'static str,
        record: Option<u64>,
        cause: TextDecodeError,
    },
    #[cfg(target_os = "macos")]
    MediaLimit {
        location: Location,
        limit: u64,
    },
    #[cfg(target_os = "macos")]
    Media {
        location: Location,
        cause: MediaError,
    },
    #[cfg(target_os = "macos")]
    Pdf {
        location: Location,
        cause: PdfError,
    },
    WorkerUnavailable,
}

impl PreviewError {
    pub(crate) fn io_cause(&self) -> Option<&io::Error> {
        match self {
            Self::Image { cause, .. } => cause.io_cause(),
            #[cfg(target_os = "macos")]
            Self::Media {
                cause: MediaError::Io { source, .. },
                ..
            }
            | Self::Pdf {
                cause: PdfError::Io { source, .. },
                ..
            } => Some(source),
            _ => None,
        }
    }
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
            Self::Image { location, cause } => write!(f, "Image preview at {location}: {cause}"),
            Self::Text {
                location,
                encoding,
                record,
                cause,
            } => {
                write!(f, "Text preview in {encoding} at {location}")?;
                if let Some(record) = record {
                    write!(f, ", record {record}")?;
                }
                write!(f, ": {cause}")
            }
            #[cfg(target_os = "macos")]
            Self::MediaLimit { location, limit } => {
                write!(f, "Media preview exceeds {limit} bytes: {location}")
            }
            #[cfg(target_os = "macos")]
            Self::Media { location, cause } => write!(f, "Media preview at {location}: {cause}"),
            #[cfg(target_os = "macos")]
            Self::Pdf { location, cause } => write!(f, "PDF preview at {location}: {cause}"),
            Self::WorkerUnavailable => f.write_str("Preview worker is unavailable"),
        }
    }
}

impl Error for PreviewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Image { cause, .. } => Some(cause),
            Self::Text { cause, .. } => Some(cause),
            #[cfg(target_os = "macos")]
            Self::Media { cause, .. } => Some(cause),
            #[cfg(target_os = "macos")]
            Self::Pdf { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

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

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanLimitKind {
    Entries,
    PathBytes,
    Depth,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) struct ScanLimit {
    pub(crate) location: Location,
    pub(crate) kind: ScanLimitKind,
    pub(crate) maximum: usize,
}

#[cfg(target_os = "macos")]
impl fmt::Display for ScanLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "exceeded the {:?} limit of {} at {}",
            self.kind, self.maximum, self.location
        )
    }
}

#[derive(Debug)]
pub(crate) enum ScanError {
    PropertiesStart(io::Error),
    #[cfg(target_os = "macos")]
    PropertiesItemChanged(Location),
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    DirectoryChanged(Location),
    InvalidSftpEntry {
        directory: Location,
        name: PathBuf,
    },
    DepthLimit(Location),
    TransferDepthLimit {
        location: Location,
        limit: usize,
    },
    MoveEntryLimit {
        location: Location,
        limit: usize,
    },
    #[cfg(target_os = "macos")]
    FolderFingerprint {
        location: Location,
        source: serde_json::Error,
    },
    TransferSizeOverflow(Location),
    FolderUnavailable(Location),
    DuplicateEntry {
        directory: Location,
        name: std::ffi::OsString,
    },
    UnsupportedComparisonType(Location),
    MissingComparisonMetadata(Location),
    ComparisonLimit,
    ComparisonFolder {
        file: Location,
        cause: Box<TransferError>,
    },
    ShortComparisonRead {
        file: Location,
        expected: u64,
        actual: u64,
    },
    FileMissing(Location),
    FileChanged(Location),
    ExpectedRegularFile(Location),
    #[cfg(target_os = "macos")]
    MissingLocalFileSize,
    MissingSftpFileSize(Location),
    MissingSftpModificationTime(Location),
    MissingSftpFileVersion(Location),
    MissingSftpFileType(Location),
    SftpCopySymlink(Location),
    SftpCopySpecialFile(Location),
    LocalCopySpecialFile(Location),
    #[cfg(target_os = "macos")]
    DiskUsageRequiresLocalFolder(Location),
    #[cfg(target_os = "macos")]
    OtherVolume(Location),
    #[cfg(target_os = "macos")]
    DiskUsageLimit(ScanLimit),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PropertiesStart(source) => write!(f, "Calculation not started: {source}"),
            #[cfg(target_os = "macos")]
            Self::PropertiesItemChanged(location) => {
                write!(f, "Item changed while reading properties at {location}")
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            Self::DirectoryChanged(location) => {
                write!(f, "Folder {location} changed during calculation")
            }
            Self::InvalidSftpEntry { directory, name } => {
                write!(f, "Invalid entry {name:?} in {directory}")
            }
            Self::DepthLimit(location) => write!(f, "Folder nesting limit reached at {location}"),
            Self::TransferDepthLimit { location, limit } => {
                write!(
                    f,
                    "Folders are nested more than {limit} levels at {location}"
                )
            }
            Self::MoveEntryLimit { location, limit } => {
                write!(
                    f,
                    "The per-item move journal exceeds {limit} entries at {location}"
                )
            }
            #[cfg(target_os = "macos")]
            Self::FolderFingerprint { location, source } => {
                write!(
                    f,
                    "Could not encode folder fingerprint metadata for {location}: {source}"
                )
            }
            Self::TransferSizeOverflow(location) => {
                write!(f, "The copy grew beyond its size limit at {location}")
            }
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
            Self::ComparisonFolder { file, cause } => {
                write!(f, "Could not verify the folder containing {file}: {cause}")
            }
            Self::ShortComparisonRead {
                file,
                expected,
                actual,
            } => {
                write!(f, "Expected {expected} bytes from {file} but read {actual}")
            }
            Self::FileMissing(location) => write!(f, "File missing at {location}"),
            Self::FileChanged(location) => write!(f, "File {location} changed while reading"),
            Self::ExpectedRegularFile(location) => {
                write!(f, "Expected a regular file at {location}")
            }
            #[cfg(target_os = "macos")]
            Self::MissingLocalFileSize => f.write_str("File size is unavailable"),
            Self::MissingSftpFileSize(location) => {
                write!(f, "Missing SFTP file size for {location}")
            }
            Self::MissingSftpModificationTime(location) => {
                write!(f, "Missing SFTP modification time for {location}")
            }
            Self::MissingSftpFileVersion(location) => {
                write!(
                    f,
                    "Missing SFTP file size and modification time for {location}"
                )
            }
            Self::MissingSftpFileType(location) => {
                write!(f, "Missing SFTP file type for {location}")
            }
            Self::SftpCopySymlink(location) => {
                write!(f, "Cannot copy a symbolic link over SFTP: {location}")
            }
            Self::SftpCopySpecialFile(location) => {
                write!(f, "Cannot copy a special file over SFTP: {location}")
            }
            Self::LocalCopySpecialFile(location) => {
                write!(f, "Cannot copy a special file at {location}")
            }
            #[cfg(target_os = "macos")]
            Self::DiskUsageRequiresLocalFolder(location) => {
                write!(f, "Disk usage analysis needs a local folder at {location}")
            }
            #[cfg(target_os = "macos")]
            Self::OtherVolume(location) => {
                write!(f, "Skipped {location} on another volume")
            }
            #[cfg(target_os = "macos")]
            Self::DiskUsageLimit(limit) => write!(f, "Disk usage analysis {limit}"),
        }
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PropertiesStart(source) => Some(source),
            Self::ComparisonFolder { cause, .. } => Some(cause.as_ref()),
            #[cfg(target_os = "macos")]
            Self::FolderFingerprint { source, .. } => Some(source),
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
    StagingCreation {
        staging: Location,
        create: ssh2::Error,
        inspect: Option<ssh2::Error>,
    },
    #[cfg(target_os = "macos")]
    SmbUpload {
        target: Location,
        cause: TransferError,
    },
    #[cfg(target_os = "macos")]
    SmbTargetChanged(Location),
    #[cfg(target_os = "macos")]
    SmbTargetSize {
        target: Location,
        expected: u64,
        actual: u64,
    },
    #[cfg(target_os = "macos")]
    CompatibleOutputChanged(Location),
    #[cfg(target_os = "macos")]
    CompatibleCopyUnconfirmed {
        target: Location,
        cause: TransferError,
    },
    #[cfg(target_os = "macos")]
    DirectoryUnverified {
        target: Location,
        cause: TransferError,
    },
    #[cfg(target_os = "macos")]
    RecoveryFinalization {
        target: Location,
        moving: bool,
        cause: RecoveryRecordError,
    },
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
            Self::StagingCreation {
                staging,
                create,
                inspect,
            } => {
                write!(
                    f,
                    "Temporary folder creation at {staging} is unconfirmed: {create}"
                )?;
                if let Some(inspect) = inspect {
                    write!(f, ". Inspection failed: {inspect}")?;
                }
                Ok(())
            }
            #[cfg(target_os = "macos")]
            Self::SmbUpload { target, cause } => {
                write!(f, "SMB copy to {target} is unconfirmed: {cause}")
            }
            #[cfg(target_os = "macos")]
            Self::SmbTargetChanged(target) => {
                write!(f, "SMB destination {target} changed during copying")
            }
            #[cfg(target_os = "macos")]
            Self::SmbTargetSize {
                target,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "SMB destination {target} has {actual} bytes, expected {expected}"
                )
            }
            #[cfg(target_os = "macos")]
            Self::CompatibleOutputChanged(target) => {
                write!(f, "Destination differs from the verified copy: {target}")
            }
            #[cfg(target_os = "macos")]
            Self::CompatibleCopyUnconfirmed { target, cause } => {
                write!(f, "Compatible copy at {target} is unconfirmed: {cause}")
            }
            #[cfg(target_os = "macos")]
            Self::DirectoryUnverified { target, cause } => {
                write!(f, "The new folder {target} could not be verified: {cause}")
            }
            #[cfg(target_os = "macos")]
            Self::RecoveryFinalization {
                target,
                moving,
                cause,
            } => {
                let operation = if *moving { "Move" } else { "Copy" };
                write!(f, "{operation} completed at {target}. {cause}")
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
            Self::StagingCreation { create, .. } => Some(create),
            #[cfg(target_os = "macos")]
            Self::SmbUpload { cause, .. } => Some(cause),
            #[cfg(target_os = "macos")]
            Self::SmbTargetChanged(_) | Self::SmbTargetSize { .. } => None,
            #[cfg(target_os = "macos")]
            Self::CompatibleOutputChanged(_) => None,
            #[cfg(target_os = "macos")]
            Self::CompatibleCopyUnconfirmed { cause, .. }
            | Self::DirectoryUnverified { cause, .. } => Some(cause),
            #[cfg(target_os = "macos")]
            Self::RecoveryFinalization { cause, .. } => Some(cause),
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
    pub(crate) completed_move: bool,
    pub(crate) cleanup: TransferError,
}

impl StagingCleanupError {
    pub(crate) fn combine(
        result: Result<(), TransferError>,
        cleanup: Result<(), TransferError>,
        staging: &Location,
        target: &Location,
    ) -> Result<(), TransferError> {
        match cleanup {
            Ok(()) => result,
            Err(cleanup) => Err(Self::failed_cleanup(result.err(), cleanup, staging, target)),
        }
    }

    pub(crate) fn failed_cleanup(
        operation: Option<TransferError>,
        cleanup: TransferError,
        staging: &Location,
        target: &Location,
    ) -> TransferError {
        // A private cleanup with a lost reply does not affect publication of the target.
        let unconfirmed = matches!(operation, Some(TransferError::PublicationUnconfirmed(_)));
        let reason = Failure::StagingCleanup(Box::new(Self {
            operation,
            completed_move: false,
            cleanup,
            staging: staging.clone(),
            target: target.clone(),
        }));
        if unconfirmed {
            TransferError::PublicationUnconfirmed(reason)
        } else {
            TransferError::Failed(reason)
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn after_move(
        cleanup: TransferError,
        staging: &Location,
        target: &Location,
    ) -> TransferError {
        TransferError::Failed(Failure::StagingCleanup(Box::new(Self {
            staging: staging.clone(),
            target: target.clone(),
            operation: None,
            completed_move: true,
            cleanup,
        })))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn stage_may_be_absent(&self) -> bool {
        match &self.operation {
            None | Some(TransferError::Cancelled) => true,
            Some(TransferError::Failed(Failure::CrossMove(error))) => {
                matches!(
                    error.as_ref(),
                    CrossMoveError::Prepare { .. } | CrossMoveError::Restored { .. }
                )
            }
            Some(TransferError::Failed(Failure::Staging(error))) => matches!(
                error.as_ref(),
                StagingError::Journal {
                    action: StagingRecordAction::Register,
                    ..
                } | StagingError::ResumeReceipt { .. }
                    | StagingError::ResumeReceiptCleanup { .. }
            ),
            _ => false,
        }
    }
}

impl fmt::Display for StagingCleanupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(error) = &self.operation {
            write!(f, "{error}. ")?;
        } else if self.completed_move {
            write!(f, "Move completed at {}. ", self.target)?;
        } else {
            write!(f, "Item {} was published. ", self.target)?;
        }
        if matches!(self.cleanup, TransferError::PublicationUnconfirmed(_)) {
            return write!(
                f,
                "Cleanup at {} is unconfirmed: {}",
                self.staging, self.cleanup
            );
        }
        #[cfg(target_os = "macos")]
        if self.stage_may_be_absent() {
            return write!(
                f,
                "Could not clean up the temporary folder at {}. {}",
                self.staging, self.cleanup
            );
        }
        write!(
            f,
            "Temporary folder remains at {}: {}",
            self.staging, self.cleanup
        )
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
    RemoveFile,
    RemoveDirectory,
}

#[derive(Debug)]
pub(crate) enum SftpRemovalAction {
    Inspect,
    RemoveFile,
    RemoveDirectory,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
pub(crate) enum SmbMutationAction {
    CreateFile,
    CreateFolder,
    Rename,
    Trash,
    BatchRename,
    CreateLink,
    InspectLinkTarget,
    Undo,
    ReadArchive,
    CreateArchive,
    ExtractArchive,
    EditArchive,
    CreateChecksumManifest,
    VerifyChecksumManifest,
    ImportCollection,
    ExportCollection,
    CompareFolders,
    CompareFiles,
    CheckSyncFolder,
    FindDuplicates,
    AnalyzeDiskUsage,
    ReadProperties,
    ReadPreview,
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
    #[cfg(target_os = "macos")]
    CompatibleNameOccupied(Location),
    SourceChanged(Location),
    MoveIncomplete {
        target: Location,
        cause: TransferError,
    },
    #[cfg(target_os = "macos")]
    RetainedMoveSource {
        held: Location,
        cause: TransferError,
    },
    #[cfg(target_os = "macos")]
    PlanChanged(Location),
    #[cfg(target_os = "macos")]
    SmbSourceNotRegular(Location),
    #[cfg(target_os = "macos")]
    SmbMount {
        path: PathBuf,
        action: SmbMutationAction,
        cause: Box<crate::smb::SmbError>,
    },
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

    #[cfg(target_os = "macos")]
    pub(crate) fn smb_mount(
        path: &Path,
        action: SmbMutationAction,
        cause: crate::smb::SmbError,
    ) -> Self {
        Self::SmbMount {
            path: path.to_owned(),
            action,
            cause: Box::new(cause),
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
            #[cfg(target_os = "macos")]
            Self::SmbMount {
                path,
                action,
                cause,
            } => write!(f, "SMB {action:?} at {}: {cause}", path.display()),
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
            #[cfg(target_os = "macos")]
            Self::CompatibleNameOccupied(target) => {
                write!(
                    f,
                    "Destination name became occupied after preview: {target}"
                )
            }
            Self::SourceChanged(location) => write!(f, "Source changed during copying: {location}"),
            Self::MoveIncomplete { target, cause } => {
                write!(
                    f,
                    "A copy was created at {target}, but the move is incomplete: {cause}"
                )
            }
            #[cfg(target_os = "macos")]
            Self::RetainedMoveSource { held, cause } => {
                write!(f, "Retained source at {held}: {cause}")
            }
            #[cfg(target_os = "macos")]
            Self::PlanChanged(location) => write!(f, "An item changed after planning: {location}"),
            #[cfg(target_os = "macos")]
            Self::SmbSourceNotRegular(source) => {
                write!(f, "SMB uploads require a regular file: {source}")
            }
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
            Self::MoveIncomplete { cause, .. } => Some(cause),
            #[cfg(target_os = "macos")]
            Self::RetainedMoveSource { cause, .. } => Some(cause),
            Self::Trash { source, .. } => Some(source),
            #[cfg(target_os = "macos")]
            Self::SmbMount { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ListingErrorKind {
    OpenLocal(io::Error),
    #[cfg(target_os = "macos")]
    SmbMount(Box<crate::smb::SmbError>),
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
            #[cfg(target_os = "macos")]
            SmbMount(source) => write!(f, "Could not open folder: {source}"),
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
            #[cfg(target_os = "macos")]
            SmbMount(source) => Some(source.as_ref()),
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
