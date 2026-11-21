use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplacementRefusal {
    SourceNotRegular,
    TargetNotRegular,
    SameFile,
    WouldOverwriteArchive,
    #[cfg(target_os = "macos")]
    Smb,
}

#[derive(Debug)]
pub(crate) enum ConflictError {
    ReplacementUnavailable {
        source: Location,
        target: Location,
        reason: ReplacementRefusal,
    },
    SourceChanged(Location),
}

impl From<ConflictError> for TransferError {
    fn from(error: ConflictError) -> Self {
        Self::Failed(Failure::Conflict(Box::new(error)))
    }
}

impl fmt::Display for ConflictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReplacementUnavailable {
                source,
                target,
                reason,
            } => {
                write!(f, "Cannot replace {target} with {source} ({reason:?})")
            }
            Self::SourceChanged(source) => {
                write!(
                    f,
                    "Source {source} changed after the conflict dialog was shown"
                )
            }
        }
    }
}

impl Error for ConflictError {}
