use crate::vfs::Location;
use std::{error::Error, fmt, io};

#[cfg(target_os = "macos")]
use {super::RecoveryRecordError, crate::operations::TransferError};

#[derive(Debug)]
pub(crate) enum StagingRecordAction {
    Plan,
    Abandon,
    Register,
}

#[derive(Debug)]
pub(crate) enum StagingError {
    MissingParent(Location),
    NamesExhausted(Location),
    IdentityChanged(Location),
    #[cfg(any(target_os = "macos", windows))]
    IdentityUnavailable(Location),
    Journal {
        staging: Location,
        action: StagingRecordAction,
        source: io::Error,
    },
    #[cfg(target_os = "macos")]
    ResumeReceipt {
        staging: Location,
        source: RecoveryRecordError,
    },
    #[cfg(target_os = "macos")]
    ResumeReceiptCleanup {
        cause: TransferError,
        source: RecoveryRecordError,
    },
}

impl fmt::Display for StagingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingParent(target) => write!(f, "Invalid staging destination {target}"),
            Self::NamesExhausted(parent) => {
                write!(f, "Could not reserve a staging name in {parent}")
            }
            Self::IdentityChanged(staging) => {
                write!(f, "Staging directory identity changed at {staging}")
            }
            #[cfg(any(target_os = "macos", windows))]
            Self::IdentityUnavailable(staging) => {
                write!(f, "Staging directory identity unavailable at {staging}")
            }
            Self::Journal {
                staging,
                action,
                source,
            } => {
                write!(f, "Staging journal {action:?} for {staging}: {source}")
            }
            #[cfg(target_os = "macos")]
            Self::ResumeReceipt { staging, source } => {
                write!(f, "Could not save resume receipt for {staging}: {source}")
            }
            #[cfg(target_os = "macos")]
            Self::ResumeReceiptCleanup { cause, source } => write!(f, "{cause}. {source}"),
        }
    }
}

impl Error for StagingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Journal { source, .. } => Some(source),
            #[cfg(target_os = "macos")]
            Self::ResumeReceipt { source, .. } => Some(source),
            #[cfg(target_os = "macos")]
            Self::ResumeReceiptCleanup { cause, source, .. } => {
                if matches!(cause, TransferError::Cancelled) {
                    Some(source)
                } else {
                    Some(cause)
                }
            }
            _ => None,
        }
    }
}
