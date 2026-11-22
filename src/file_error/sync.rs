use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncFolderRole {
    Source,
    Destination,
    Left,
    Right,
}

#[derive(Debug)]
pub(crate) enum SyncVerification {
    StagedCopy,
    HeldOriginal,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SyncDifference {
    Missing,
    Unexpected,
    Changed,
}

#[derive(Debug)]
pub(crate) enum SyncError {
    Folder {
        role: SyncFolderRole,
        folder: Location,
        cause: Box<TransferError>,
    },
    Apply {
        completed: usize,
        cause: Box<TransferError>,
    },
    VerificationMismatch {
        check: SyncVerification,
        difference: SyncDifference,
        original: Location,
        inspected: Location,
    },
    EntryTypeChanged(Location),
    LinkTargetChanged(Location),
    PlanEntryAdded(Location),
    PlanEntryMissing(Location),
    NameCollision(Location),
    RetainedStage(Location),
    Overlap {
        source: Location,
        destination: Location,
    },
    PlanLimit,
    ExcludedPath {
        target: Location,
        excluded: PathBuf,
    },
    BaselineChanged,
    UnresolvedConflicts,
    ResultMismatch(Location),
}

impl SyncError {
    pub(crate) fn applying(completed: usize, cause: TransferError) -> TransferError {
        if matches!(cause, TransferError::Cancelled) {
            return cause;
        }
        let unconfirmed = matches!(cause, TransferError::PublicationUnconfirmed(_));
        let error: TransferError = Self::Apply {
            completed,
            cause: Box::new(cause),
        }
        .into();
        if unconfirmed {
            error.with_unconfirmed_publication()
        } else {
            error
        }
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Apply { completed, cause } => {
                write!(
                    f,
                    "Synchronization stopped after {completed} completed changes: {cause}"
                )
            }
            Self::VerificationMismatch {
                check,
                difference,
                original,
                inspected,
            } => {
                write!(
                    f,
                    "{check:?} does not match {original}: {difference:?} item at {inspected}"
                )
            }
            Self::Folder {
                role,
                folder,
                cause,
            } => {
                write!(f, "{role:?} folder {folder}: {cause}")
            }
            Self::EntryTypeChanged(location) => {
                write!(f, "Item type changed while scanning {location}")
            }
            Self::LinkTargetChanged(location) => {
                write!(f, "Symbolic link target changed while scanning {location}")
            }
            Self::PlanEntryAdded(location) => write!(f, "Item added after planning at {location}"),
            Self::PlanEntryMissing(location) => {
                write!(f, "Item disappeared after planning at {location}")
            }
            Self::NameCollision(location) => write!(f, "Names collide at {location}"),
            Self::RetainedStage(location) => write!(f, "Found a sync staging folder at {location}"),
            Self::Overlap {
                source,
                destination,
            } => {
                write!(f, "Sync folders overlap: {source} and {destination}")
            }
            Self::PlanLimit => f.write_str("Synchronization plan exceeded its entry or path limit"),
            Self::ExcludedPath { target, excluded } => write!(
                f,
                "Replacing or deleting {target} would affect excluded path {}",
                excluded.display()
            ),
            Self::BaselineChanged => f.write_str("The shared synchronization state changed"),
            Self::UnresolvedConflicts => {
                f.write_str("The synchronization plan has unresolved conflicts")
            }
            Self::ResultMismatch(folder) => {
                write!(f, "Folder {folder} differs from the applied plan")
            }
        }
    }
}

impl Error for SyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Folder { cause, .. } | Self::Apply { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }
}
