use super::Failure;
use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt, io};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CopyVerificationError {
    SourceSize {
        source: Location,
        expected: u64,
        actual: u64,
    },
    CopySize {
        target: Location,
        expected: u64,
        actual: u64,
    },
    ExtraData {
        target: Location,
        expected: u64,
    },
    DigestMismatch(Location),
    StagedNotRegular {
        staging: Location,
        target: Location,
    },
}

impl fmt::Display for CopyVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceSize {
                source,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "Read {actual} bytes while copying {source}. Expected {expected}"
                )
            }
            Self::CopySize {
                target,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "Read {actual} bytes while verifying {target}. Expected {expected}"
                )
            }
            Self::ExtraData { target, expected } => {
                write!(
                    f,
                    "Verification found data beyond {expected} bytes at {target}"
                )
            }
            Self::DigestMismatch(target) => write!(f, "SHA-256 mismatch at {target}"),
            Self::StagedNotRegular { staging, target } => {
                write!(
                    f,
                    "Expected a regular temporary file at {staging} for {target}"
                )
            }
        }
    }
}

impl Error for CopyVerificationError {}

impl From<CopyVerificationError> for Failure {
    fn from(error: CopyVerificationError) -> Self {
        Self::CopyVerification(Box::new(error))
    }
}

impl From<CopyVerificationError> for TransferError {
    fn from(error: CopyVerificationError) -> Self {
        Self::Failed(error.into())
    }
}

impl From<CopyVerificationError> for io::Error {
    fn from(error: CopyVerificationError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, Failure::from(error))
    }
}
