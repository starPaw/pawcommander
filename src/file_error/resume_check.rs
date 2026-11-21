use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeRole {
    Source,
    Destination,
    Staged,
    ParkedDestination,
    HeldOriginal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeCheckProblem {
    Missing,
    Unexpected,
    Changed,
    ContentsMismatch,
    SizeMismatch { expected: u64, actual: u64 },
    WrongType,
    IncompleteMetadata,
    InvalidPath,
    MissingProof,
}

#[derive(Debug)]
pub(crate) struct ResumeCheckError {
    pub(crate) role: ResumeRole,
    pub(crate) location: Location,
    pub(crate) problem: ResumeCheckProblem,
}

impl ResumeRole {
    pub(crate) fn error(self, location: &Location, problem: ResumeCheckProblem) -> TransferError {
        TransferError::Failed(Failure::ResumeCheck(Box::new(ResumeCheckError {
            role: self,
            location: location.clone(),
            problem,
        })))
    }
}

impl fmt::Display for ResumeCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Recovery check for {:?} at {}: {:?}",
            self.role, self.location, self.problem
        )
    }
}

impl Error for ResumeCheckError {}
