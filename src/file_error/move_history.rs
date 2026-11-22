use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) enum DescendantProblem {
    Missing,
    Changed,
    Unmapped,
    Read(TransferError),
}

#[derive(Debug)]
pub(crate) enum MoveHistoryError {
    Coverage {
        target: Location,
        cause: TransferError,
    },
    Descendants {
        target: Location,
        unconfirmed: usize,
        first: TransferError,
    },
    Descendant {
        source: Location,
        target: Location,
        problem: DescendantProblem,
    },
}

impl From<MoveHistoryError> for TransferError {
    fn from(error: MoveHistoryError) -> Self {
        Self::Failed(Failure::MoveHistory(Box::new(error)))
    }
}

impl fmt::Display for MoveHistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coverage { target, cause } => {
                write!(
                    f,
                    "Moved to {target}, but the item history is incomplete: {cause}"
                )
            }
            Self::Descendants {
                target,
                unconfirmed,
                first,
            } => {
                write!(f, "Moved to {target}, but {unconfirmed} descendants could not be confirmed: {first}")
            }
            Self::Descendant {
                source,
                target,
                problem,
            } => {
                write!(f, "Could not confirm the move of {source} to {target}: ")?;
                match problem {
                    DescendantProblem::Missing => f.write_str("the destination is missing"),
                    DescendantProblem::Changed => f.write_str("the destination item changed"),
                    DescendantProblem::Unmapped => {
                        f.write_str("the descendant path could not be mapped")
                    }
                    DescendantProblem::Read(cause) => fmt::Display::fmt(cause, f),
                }
            }
        }
    }
}

impl Error for MoveHistoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Coverage { cause, .. }
            | Self::Descendants { first: cause, .. }
            | Self::Descendant {
                problem: DescendantProblem::Read(cause),
                ..
            } => Some(cause),
            _ => None,
        }
    }
}
