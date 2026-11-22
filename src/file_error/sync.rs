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
pub(crate) enum SyncError {
    Folder {
        role: SyncFolderRole,
        folder: Location,
        cause: Box<TransferError>,
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

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            Self::Folder { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }
}
