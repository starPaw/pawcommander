use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) enum CompatiblePlanError {
    EmptySelection,
    NonLocal(Location),
    SmbVolume(Location),
    ReadOnlyDestination(Location),
    ReadOnlySource(Location),
    FatSource(Location),
    NestedVolume(Location),
    UnsupportedItem(Location),
    StagingPathLimit { destination: Location, limit: usize },
    NameLimit { target: Location, available: usize },
    EntryLimit { source: Location, limit: usize },
    PathBudget { source: Location, limit: usize },
}

impl From<CompatiblePlanError> for TransferError {
    fn from(error: CompatiblePlanError) -> Self {
        Self::Failed(Failure::CompatiblePlan(Box::new(error)))
    }
}

impl fmt::Display for CompatiblePlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySelection => f.write_str("Compatible transfer requires a selection"),
            Self::NonLocal(path) => {
                write!(f, "Compatible transfer requires a local path at {path}")
            }
            Self::SmbVolume(path) => {
                write!(f, "Compatible transfer is unavailable on SMB at {path}")
            }
            Self::ReadOnlyDestination(path) => write!(f, "Read-only destination volume at {path}"),
            Self::ReadOnlySource(path) => write!(
                f,
                "Moving is unavailable on the read-only source volume at {path}"
            ),
            Self::FatSource(path) => write!(
                f,
                "Moving is unavailable on the FAT or exFAT source volume at {path}"
            ),
            Self::NestedVolume(path) => write!(f, "The selected tree crosses a mount at {path}"),
            Self::UnsupportedItem(path) => {
                write!(f, "Unsupported item for compatible transfer at {path}")
            }
            Self::StagingPathLimit { destination, limit } => {
                write!(
                    f,
                    "A temporary copy at {destination} would exceed the {limit}-byte path limit"
                )
            }
            Self::NameLimit { target, available } => {
                write!(
                    f,
                    "No compatible name fits at {target} with {available} name bytes available"
                )
            }
            Self::EntryLimit { source, limit } => {
                write!(f, "Compatible plan exceeded {limit} entries at {source}")
            }
            Self::PathBudget { source, limit } => {
                write!(
                    f,
                    "Compatible plan uses more than {limit} bytes for paths at {source}"
                )
            }
        }
    }
}

impl Error for CompatiblePlanError {}
