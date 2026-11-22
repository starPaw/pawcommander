use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt, io};

#[derive(Debug)]
pub(crate) enum UndoError {
    OriginalOccupied {
        target: Location,
        source: Option<io::Error>,
    },
    FolderChanged(Location),
    ItemMissing(Location),
    ItemChanged(Location),
    RestoredItemMismatch(Location),
    Unconfirmed {
        from: Location,
        to: Location,
        cause: TransferError,
    },
    Incomplete {
        restored: usize,
        remaining: usize,
        cause: TransferError,
    },
}

impl From<UndoError> for TransferError {
    fn from(error: UndoError) -> Self {
        Self::Failed(Failure::Undo(Box::new(error)))
    }
}

impl fmt::Display for UndoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OriginalOccupied { target, source } => {
                write!(f, "The original name {target} is occupied")?;
                if let Some(source) = source {
                    write!(f, " ({source})")?;
                }
                Ok(())
            }
            Self::FolderChanged(path) => write!(f, "A folder required for Undo changed at {path}"),
            Self::ItemMissing(path) => write!(f, "The item to restore is missing at {path}"),
            Self::ItemChanged(path) => write!(f, "The item to restore changed at {path}"),
            Self::RestoredItemMismatch(path) => {
                write!(f, "The restored item does not match at {path}")
            }
            Self::Unconfirmed { from, to, cause } => {
                write!(
                    f,
                    "Could not confirm restoration from {from} to {to}: {cause}"
                )
            }
            Self::Incomplete {
                restored,
                remaining,
                cause,
            } => write!(
                f,
                "Restored {restored}, {remaining} available to retry: {cause}"
            ),
        }
    }
}

impl Error for UndoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OriginalOccupied { source, .. } => {
                source.as_ref().map(|error| error as &dyn Error)
            }
            Self::Unconfirmed { cause, .. } | Self::Incomplete { cause, .. } => Some(cause),
            _ => None,
        }
    }
}
