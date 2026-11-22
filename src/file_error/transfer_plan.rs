use super::{Failure, PathError};
use crate::{operations::TransferError, vfs::Location};
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
use std::{error::Error, fmt};

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
pub(crate) enum SmbTransferRole {
    Source,
    Destination,
}

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
    #[cfg(target_os = "macos")]
    SmbMount {
        path: PathBuf,
        role: SmbTransferRole,
        cause: Box<crate::smb::SmbError>,
    },
}

#[cfg(target_os = "macos")]
impl TransferPlanError {
    pub(crate) fn smb_mount(
        path: &Path,
        role: SmbTransferRole,
        cause: crate::smb::SmbError,
    ) -> Self {
        Self::SmbMount {
            path: path.to_owned(),
            role,
            cause: Box::new(cause),
        }
    }
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
            #[cfg(target_os = "macos")]
            Self::SmbMount { path, role, cause } => write!(
                f,
                "Cannot use SMB transfer {role:?} {}: {cause}",
                path.display()
            ),
        }
    }
}

impl Error for TransferPlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSourceName { cause, .. } => Some(cause),
            #[cfg(target_os = "macos")]
            Self::SmbMount { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }
}
