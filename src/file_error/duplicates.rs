use super::{Failure, ScanLimit, StagingCleanupError};
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) enum DuplicateRole {
    Folder,
    Protected,
    Selected,
}

#[derive(Debug)]
pub(crate) struct MacTrashError {
    pub domain: String,
    pub code: isize,
    pub description: String,
}

impl fmt::Display for MacTrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} {})", self.description, self.domain, self.code)
    }
}

impl Error for MacTrashError {}

#[derive(Debug)]
pub(crate) enum DuplicateError {
    SearchRequiresLocal(Location),
    SearchInStaging(Location),
    SearchLimit(ScanLimit),
    Changed {
        location: Location,
        cause: Option<TransferError>,
    },
    Verify {
        location: Location,
        role: DuplicateRole,
        cause: TransferError,
    },
    DifferentContents {
        selected: Location,
        protected: Location,
    },
    Trash {
        location: Location,
        source: MacTrashError,
    },
    Restored {
        location: Location,
        cause: TransferError,
    },
    Restore {
        original: Location,
        held: Location,
        cause: TransferError,
        restore: TransferError,
    },
    CleanupAfterTrash(StagingCleanupError),
    Batch {
        completed: usize,
        cleanup_warnings: usize,
        cause: TransferError,
    },
}

impl DuplicateError {
    pub(crate) fn verification(
        location: &Location,
        role: DuplicateRole,
        cause: TransferError,
    ) -> TransferError {
        if matches!(cause, TransferError::Cancelled) {
            return cause;
        }
        Self::Verify {
            location: location.clone(),
            role,
            cause,
        }
        .into()
    }
}

impl From<DuplicateError> for TransferError {
    fn from(error: DuplicateError) -> Self {
        let unconfirmed = matches!(
            &error,
            DuplicateError::Restore { .. }
                | DuplicateError::Batch {
                    cause: Self::PublicationUnconfirmed(_),
                    ..
                }
        );
        let reason = Failure::Duplicate(Box::new(error));
        if unconfirmed {
            Self::PublicationUnconfirmed(reason)
        } else {
            Self::Failed(reason)
        }
    }
}

impl fmt::Display for DuplicateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SearchRequiresLocal(location) => {
                write!(f, "Duplicate search requires a local folder at {location}")
            }
            Self::SearchInStaging(location) => {
                write!(
                    f,
                    "Duplicate search cannot scan the temporary folder at {location}"
                )
            }
            Self::SearchLimit(limit) => write!(f, "Duplicate search {limit}"),
            Self::Changed { location, cause } => {
                write!(f, "Item changed after duplicate search at {location}")?;
                if let Some(cause) = cause {
                    write!(f, ": {cause}")?;
                }
                Ok(())
            }
            Self::Verify {
                location,
                role,
                cause,
            } => {
                write!(
                    f,
                    "Could not verify duplicate search item ({role:?}) at {location}: {cause}"
                )
            }
            Self::DifferentContents {
                selected,
                protected,
            } => {
                write!(
                    f,
                    "Selected file {selected} differs from the protected file {protected}"
                )
            }
            Self::Trash { location, source } => {
                write!(f, "Could not move {location} to Trash: {source}")
            }
            Self::Restored { location, cause } => {
                write!(f, "{cause}. File restored to {location}")
            }
            Self::Restore {
                original,
                held,
                cause,
                restore,
            } => {
                write!(
                    f,
                    "{cause}. Could not confirm restoration from {held} to {original}: {restore}"
                )
            }
            Self::CleanupAfterTrash(error) => write!(
                f,
                "Moved {} to Trash. Could not clean up {}: {}",
                error.target, error.staging, error.cleanup
            ),
            Self::Batch {
                completed,
                cleanup_warnings,
                cause,
            } => write!(
                f,
                "{cause}. Moved to Trash: {completed}. Cleanup warnings: {cleanup_warnings}"
            ),
        }
    }
}

impl Error for DuplicateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Changed { cause, .. } => cause.as_ref().map(|cause| cause as &dyn Error),
            Self::Verify { cause, .. }
            | Self::Restored { cause, .. }
            | Self::Batch { cause, .. } => Some(cause),
            Self::Restore {
                cause: TransferError::Cancelled,
                restore,
                ..
            } => Some(restore),
            Self::Restore { cause, .. } => Some(cause),
            Self::CleanupAfterTrash(error) => Some(&error.cleanup),
            Self::Trash { source, .. } => Some(source),
            Self::SearchRequiresLocal(_)
            | Self::SearchInStaging(_)
            | Self::SearchLimit(_)
            | Self::DifferentContents { .. } => None,
        }
    }
}
