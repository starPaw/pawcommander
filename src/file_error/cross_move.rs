use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) enum CrossMoveError {
    Changed(Location),
    UnsupportedType(Location),
    #[cfg(target_os = "macos")]
    Smb(Location),
    #[cfg(target_os = "macos")]
    AccessTimes {
        source: Location,
        expected: usize,
        actual: usize,
    },
    Hold {
        original: Location,
        held: Location,
        cause: TransferError,
    },
    #[cfg(target_os = "macos")]
    Restored {
        original: Location,
        cause: Option<TransferError>,
    },
    Restore {
        original: Location,
        held: Location,
        cause: TransferError,
        restore: TransferError,
    },
}

impl From<CrossMoveError> for TransferError {
    fn from(error: CrossMoveError) -> Self {
        Self::Failed(Failure::CrossMove(Box::new(error)))
    }
}

impl fmt::Display for CrossMoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Changed(path) => write!(f, "Move stopped: item {path} changed or is inaccessible"),
            Self::UnsupportedType(path) => {
                write!(f, "Safe cross-volume move is unavailable for this object type: {path}")
            }
            #[cfg(target_os = "macos")]
            Self::Smb(path) => write!(f, "Use Copy to transfer files through SMB at {path}"),
            #[cfg(target_os = "macos")]
            Self::AccessTimes { source, expected, actual } => write!(
                f, "Invalid saved access times for {source}: expected {expected} entries, found {actual}"
            ),
            Self::Hold { original, held, cause } => {
                write!(f, "Check the original source {original} and temporary path {held}: {cause}")
            }
            #[cfg(target_os = "macos")]
            Self::Restored { original, cause } => {
                if let Some(cause) = cause {
                    writeln!(f, "{cause}")?;
                }
                write!(f, "The remaining source was restored to {original}")
            }
            Self::Restore { original, held, cause, restore } => {
                write!(f, "{cause}. Check restoration from {held} to {original}: {restore}")
            }
        }
    }
}

impl Error for CrossMoveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Restore {
                cause: TransferError::Cancelled,
                restore,
                ..
            } => Some(restore),
            Self::Hold { cause, .. } | Self::Restore { cause, .. } => Some(cause),
            #[cfg(target_os = "macos")]
            Self::Restored {
                cause: Some(cause), ..
            } => Some(cause),
            _ => None,
        }
    }
}
