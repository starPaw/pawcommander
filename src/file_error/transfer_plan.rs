use super::{Failure, PathError};
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) enum TransferPlanError {
    MissingName(Location),
    MissingParent(Location),
    NonUtf8Name(Location),
    InvalidSourceName {
        location: Location,
        cause: PathError,
    },
    DuplicateTarget {
        first: Location,
        second: Location,
        target: Location,
    },
    SameFolder {
        source: Location,
        destination: Location,
    },
    IntoSource {
        source: Location,
        destination: Location,
    },
    NoUnusedName(Location),
}

impl From<TransferPlanError> for TransferError {
    fn from(error: TransferPlanError) -> Self {
        Self::Failed(Failure::TransferPlan(Box::new(error)))
    }
}

impl fmt::Display for TransferPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingName(location) => write!(f, "Path {location} has no file name"),
            Self::MissingParent(location) => write!(f, "Path {location} has no parent folder"),
            Self::NonUtf8Name(location) => write!(f, "File name at {location} is not UTF-8"),
            Self::InvalidSourceName { location, cause } => {
                write!(f, "Invalid transfer source name at {location} ({cause})")
            }
            Self::DuplicateTarget {
                first,
                second,
                target,
            } => {
                write!(f, "Both {first} and {second} map to {target}")
            }
            Self::SameFolder {
                source,
                destination,
            } => {
                write!(f, "Source {source} is already in {destination}")
            }
            Self::IntoSource {
                source,
                destination,
            } => {
                write!(f, "Cannot copy or move {source} into {destination}")
            }
            Self::NoUnusedName(location) => write!(f, "No unused name found for {location}"),
        }
    }
}

impl Error for TransferPlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSourceName { cause, .. } => Some(cause),
            _ => None,
        }
    }
}
