use super::{Failure, PathError};
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt, io};

#[derive(Debug)]
pub(crate) enum BatchRenameError {
    SelectionCount,
    SelectionFolder,
    RuleLength,
    InvalidTemplate,
    InvalidRegex(regex::Error),
    NameEncoding,
    NameLength,
    InvalidName(PathError),
    DuplicateName,
    PreviewRequired,
    Changed(Location),
    Occupied {
        target: Location,
        source: Option<io::Error>,
    },
    UndoLimit(Location),
    Unverified {
        from: Location,
        to: Location,
        cause: TransferError,
    },
    Incomplete {
        completed: usize,
        total: usize,
        cause: TransferError,
    },
}

impl From<BatchRenameError> for TransferError {
    fn from(error: BatchRenameError) -> Self {
        Self::Failed(Failure::BatchRename(Box::new(error)))
    }
}

impl fmt::Display for BatchRenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelectionCount => f.write_str("Batch rename requires 2 to 100,000 items"),
            Self::SelectionFolder => f.write_str("Batch rename requires items in a single folder"),
            Self::RuleLength => {
                f.write_str("The rename template or regular expression is too long")
            }
            Self::InvalidTemplate => f.write_str("Invalid rename template"),
            Self::InvalidRegex(error) => write!(f, "Invalid rename expression: {error}"),
            Self::NameEncoding => f.write_str("Batch rename requires UTF-8 names"),
            Self::NameLength => f.write_str("The proposed name is too long"),
            Self::InvalidName(error) => fmt::Display::fmt(error, f),
            Self::DuplicateName => f.write_str("More than one item gets a conflicting name"),
            Self::PreviewRequired => f.write_str("Batch rename requires a valid preview"),
            Self::Changed(location) => write!(f, "Item {location} changed after selection"),
            Self::Occupied { target, source } => {
                write!(f, "Name already exists: {target}")?;
                if let Some(source) = source {
                    write!(f, " ({source})")?;
                }
                Ok(())
            }
            Self::UndoLimit(path) => write!(f, "Undo limit reached while checking {path}"),
            Self::Unverified { from, to, cause } => {
                write!(
                    f,
                    "Rename from {from} to {to} could not be verified: {cause}"
                )
            }
            Self::Incomplete {
                completed,
                total,
                cause,
            } => write!(f, "Renamed {completed} of {total}: {cause}"),
        }
    }
}

impl Error for BatchRenameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidRegex(source) => Some(source),
            Self::InvalidName(source) => Some(source),
            Self::Occupied { source, .. } => source.as_ref().map(|error| error as &dyn Error),
            Self::Unverified { cause, .. } | Self::Incomplete { cause, .. } => Some(cause),
            _ => None,
        }
    }
}
