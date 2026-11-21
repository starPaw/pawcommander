use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryProblem {
    InspectionRequired,
    RequestChanged,
    SourceChanged,
    DestinationChanged,
    TargetChanged,
    BackupRetained,
    OriginalRetained,
    PayloadMissing,
    PayloadPathMissing,
}

#[derive(Debug)]
pub(crate) struct RetryError {
    pub(crate) location: Location,
    pub(crate) problem: RetryProblem,
}

impl RetryProblem {
    pub(crate) fn at(self, location: &Location) -> TransferError {
        TransferError::Failed(Failure::Retry(Box::new(RetryError {
            location: location.clone(),
            problem: self,
        })))
    }
}

impl fmt::Display for RetryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Retry check for {}: {:?}", self.location, self.problem)
    }
}

impl Error for RetryError {}
