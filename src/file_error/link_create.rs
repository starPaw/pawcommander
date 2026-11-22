use super::{Failure, PathError};
use crate::operations::TransferError;
use std::{
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum LinkIoAction {
    InspectDirectory,
    InspectTarget,
    Create,
}

#[derive(Debug)]
pub(crate) enum LinkCreateError {
    InvalidName(PathError),
    MissingTarget,
    ExpectedFile(PathBuf),
    DifferentVolume(PathBuf),
    TargetChanged(PathBuf),
    DirectoryChanged(PathBuf),
    Io {
        path: PathBuf,
        action: LinkIoAction,
        source: io::Error,
    },
}

impl LinkCreateError {
    pub(crate) fn io(path: &Path, action: LinkIoAction, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            action,
            source,
        }
    }
}

impl From<LinkCreateError> for TransferError {
    fn from(error: LinkCreateError) -> Self {
        Self::Failed(Failure::LinkCreate(Box::new(error)))
    }
}

impl fmt::Display for LinkCreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(error) => fmt::Display::fmt(error, f),
            Self::MissingTarget => f.write_str("Enter the link target path"),
            Self::ExpectedFile(path) => write!(
                f,
                "A hard link requires a regular file at {}",
                path.display()
            ),
            Self::DifferentVolume(path) => write!(
                f,
                "The hard link target {} is on a different volume",
                path.display()
            ),
            Self::TargetChanged(path) => {
                write!(f, "The hard link target {} changed", path.display())
            }
            Self::DirectoryChanged(path) => {
                write!(f, "The link destination folder {} changed", path.display())
            }
            Self::Io {
                path,
                action,
                source,
            } => write!(f, "{action:?} at {}: {source}", path.display()),
        }
    }
}

impl Error for LinkCreateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidName(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
