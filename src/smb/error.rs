use crate::file_error::{CredentialFailure, SecretFormatError};
use std::{
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
    str::Utf8Error,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathAction {
    CreateStorage,
    ResolveStorage,
    ReadStorage,
    AllocateMount,
    PrepareMount,
    OpenMount,
    InspectMount,
    ProtectMount,
    ReadShare,
    ReadRecord,
    PrepareRecord,
    WriteRecord,
    SyncRecord,
    PublishRecord,
    Unmount,
    Cleanup,
}

impl PathAction {
    pub(super) fn at(self, path: &Path) -> impl FnOnce(io::Error) -> SmbError + '_ {
        move |source| SmbError::Local {
            action: self,
            path: path.to_owned(),
            source,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PasswordAction {
    Read,
    Save,
}

#[derive(Debug)]
pub(crate) enum MountFailure {
    Posix(io::Error),
    Status(i32),
}

#[derive(Debug)]
pub(crate) enum RecordProblem {
    NotRegular,
    TooLarge { limit: usize },
    Json(SecretFormatError),
    Endpoint(Box<SmbError>),
    EndpointMismatch,
}

impl fmt::Display for RecordProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRegular => f.write_str("Expected a regular file"),
            Self::TooLarge { limit } => write!(f, "Record exceeds {limit} bytes"),
            Self::Json(source) => write!(f, "{source}"),
            Self::Endpoint(source) => write!(f, "{source}"),
            Self::EndpointMismatch => {
                f.write_str("Saved share details differ from the normalized address")
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum SmbError {
    InvalidAddress,
    AddressSyntax(url::ParseError),
    AddressEncoding(Utf8Error),
    InvalidUser,
    InvalidPassword,
    DataDirectoryUnavailable,
    AlreadyMounted {
        address: String,
        path: PathBuf,
    },
    Mount {
        address: String,
        path: PathBuf,
        cause: MountFailure,
    },
    MissingMount(PathBuf),
    MountChanged(PathBuf),
    OpenShare {
        address: String,
        cause: Box<Self>,
    },
    Local {
        action: PathAction,
        path: PathBuf,
        source: io::Error,
    },
    EncodeRecord {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidRecord {
        path: PathBuf,
        problem: RecordProblem,
    },
    UnrecordedMount {
        mount: PathBuf,
        record: PathBuf,
    },
    DiscoveryWorkerStart(io::Error),
    Password {
        action: PasswordAction,
        address: String,
        cause: CredentialFailure,
    },
    PasswordEncoding {
        address: String,
        source: Utf8Error,
    },
    SetupCleanup {
        primary: Box<Self>,
        cleanup: Box<Self>,
    },
    DisconnectCleanup {
        address: String,
        cleanup: Box<Self>,
    },
    WorkerDisconnected,
}

impl fmt::Display for SmbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAddress => f.write_str("Invalid SMB share address"),
            Self::AddressSyntax(source) => write!(f, "Invalid SMB URL: {source}"),
            Self::AddressEncoding(source) => write!(f, "Invalid text in SMB address: {source}"),
            Self::InvalidUser => f.write_str("Invalid SMB user name"),
            Self::InvalidPassword => f.write_str("SMB password contains a NUL byte"),
            Self::DataDirectoryUnavailable => f.write_str("SMB data directory unavailable"),
            Self::AlreadyMounted { address, path } => {
                write!(f, "{address} already mounted at {}", path.display())
            }
            Self::Mount {
                address,
                path,
                cause,
            } => {
                write!(f, "Could not mount {address} at {}: ", path.display())?;
                match cause {
                    MountFailure::Posix(source) => write!(f, "{source}"),
                    MountFailure::Status(status) => write!(f, "NetFS status {status}"),
                }
            }
            Self::MissingMount(path) => {
                write!(f, "Expected SMB mount missing at {}", path.display())
            }
            Self::MountChanged(path) => write!(f, "SMB mount changed at {}", path.display()),
            Self::OpenShare { address, cause } => {
                write!(f, "Could not open SMB share {address}: {cause}")
            }
            Self::Local {
                action,
                path,
                source,
            } => write!(f, "SMB {action:?} at {}: {source}", path.display()),
            Self::EncodeRecord { path, source } => write!(
                f,
                "Could not encode SMB record at {}: {source}",
                path.display()
            ),
            Self::InvalidRecord { path, problem } => {
                write!(
                    f,
                    "Invalid SMB connection record at {}: {problem}",
                    path.display()
                )
            }
            Self::UnrecordedMount { mount, record } => write!(
                f,
                "SMB mount at {} is missing the connection record {}",
                mount.display(),
                record.display()
            ),
            Self::DiscoveryWorkerStart(source) => {
                write!(f, "Could not start SMB discovery: {source}")
            }
            Self::Password {
                action,
                address,
                cause,
            } => write!(f, "SMB password {action:?} for {address}: {cause}"),
            Self::PasswordEncoding { address, source } => {
                write!(f, "Invalid saved password encoding for {address}: {source}")
            }
            Self::SetupCleanup { primary, cleanup } => write!(f, "{primary}\n{cleanup}"),
            Self::DisconnectCleanup { address, cleanup } => {
                write!(f, "Disconnected {address}. {cleanup}")
            }
            Self::WorkerDisconnected => f.write_str("The SMB worker exited without a result"),
        }
    }
}

impl Error for SmbError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AddressSyntax(source) => Some(source),
            Self::AddressEncoding(source) | Self::PasswordEncoding { source, .. } => Some(source),
            Self::Local { source, .. }
            | Self::Mount {
                cause: MountFailure::Posix(source),
                ..
            } => Some(source),
            Self::EncodeRecord { source, .. } => Some(source),
            Self::InvalidRecord {
                problem: RecordProblem::Json(source),
                ..
            } => Some(source),
            Self::InvalidRecord {
                problem: RecordProblem::Endpoint(source),
                ..
            } => Some(source.as_ref()),
            Self::DiscoveryWorkerStart(source) => Some(source),
            Self::OpenShare { cause, .. } => Some(cause.as_ref()),
            Self::Password { cause, .. } => Some(cause),
            Self::SetupCleanup { primary, .. } => Some(primary.as_ref()),
            Self::DisconnectCleanup { cleanup, .. } => Some(cleanup.as_ref()),
            _ => None,
        }
    }
}
