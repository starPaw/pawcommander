use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt, path::PathBuf};

#[derive(Debug)]
pub(crate) enum RemoteEditError {
    SftpRequired(Location),
    WorkingCopyMissing(PathBuf),
    RevisionChanged(Location),
    MissingAccessMetadata(Location),
    UnsupportedMode(Location),
    AccessMismatch(Location),
}

impl From<RemoteEditError> for TransferError {
    fn from(error: RemoteEditError) -> Self {
        Self::Failed(Failure::RemoteEdit(Box::new(error)))
    }
}

impl fmt::Display for RemoteEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SftpRequired(path) => write!(f, "Expected an SFTP file at {path}"),
            Self::WorkingCopyMissing(path) => {
                write!(f, "Working copy is missing at {}", path.display())
            }
            Self::RevisionChanged(path) => {
                write!(f, "Server file {path} changed or is unavailable")
            }
            Self::MissingAccessMetadata(path) => {
                write!(f, "Owner or permissions are unavailable for {path}")
            }
            Self::UnsupportedMode(path) => write!(f, "Unsupported special permissions at {path}"),
            Self::AccessMismatch(path) => {
                write!(
                    f,
                    "Owner or permissions could not be verified on the staged file {path}"
                )
            }
        }
    }
}

impl Error for RemoteEditError {}
