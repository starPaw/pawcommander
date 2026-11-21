use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt, path::PathBuf};

#[derive(Debug)]
pub(crate) enum FilePromiseError {
    RemoteSourceRequired(Location),
    SourceMissing(Location),
    SourceTypeChanged(Location),
    InvalidReceiverUrl,
    InvalidDestination(PathBuf),
    QueueUnavailable,
    QueueDisconnected,
}

impl From<FilePromiseError> for TransferError {
    fn from(error: FilePromiseError) -> Self {
        Self::Failed(Failure::FilePromise(Box::new(error)))
    }
}

impl fmt::Display for FilePromiseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemoteSourceRequired(path) => write!(f, "Expected an SFTP source at {path}"),
            Self::SourceMissing(path) => write!(f, "The dragged item is missing at {path}"),
            Self::SourceTypeChanged(path) => write!(f, "The dragged item changed type at {path}"),
            Self::InvalidReceiverUrl => f.write_str("The receiver did not supply a file path"),
            Self::InvalidDestination(path) => {
                write!(f, "Invalid receiver destination {}", path.display())
            }
            Self::QueueUnavailable => f.write_str("The file operation queue is unavailable"),
            Self::QueueDisconnected => f.write_str("The queue closed before confirming delivery"),
        }
    }
}

impl Error for FilePromiseError {}
