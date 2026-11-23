use super::PreviewError;
use crate::vfs::Location;
use std::{error::Error, fmt, io};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewWorkerKind {
    File,
    Text,
    Hex,
    Table,
}

impl fmt::Display for PreviewWorkerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::File => "File preview",
            Self::Text => "Text preview",
            Self::Hex => "Hex preview",
            Self::Table => "Table preview",
        })
    }
}

#[derive(Debug)]
pub(crate) enum PreviewWorkerError {
    Start(io::Error),
    Disconnected,
}

impl PreviewWorkerError {
    pub(crate) fn at(self, location: &Location, kind: PreviewWorkerKind) -> PreviewError {
        PreviewError::Worker {
            location: location.clone(),
            kind,
            cause: self,
        }
    }
}

impl fmt::Display for PreviewWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(source) => write!(f, "Could not start the preview service: {source}"),
            Self::Disconnected => f.write_str("The preview service stopped"),
        }
    }
}

impl Error for PreviewWorkerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Start(source) => Some(source),
            Self::Disconnected => None,
        }
    }
}
