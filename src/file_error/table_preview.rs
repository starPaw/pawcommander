use super::PreviewError;
use crate::vfs::Location;
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) enum TablePreviewError {
    RecordBytes { record: u64, limit: usize },
    ColumnCount { record: u64, limit: usize },
    Filter(regex::Error),
}

impl TablePreviewError {
    pub(crate) fn at(self, location: &Location) -> PreviewError {
        PreviewError::Table {
            location: location.clone(),
            cause: self,
        }
    }
}

impl fmt::Display for TablePreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecordBytes { record, limit } => {
                write!(f, "Record {record} exceeds {limit} bytes of decoded text")
            }
            Self::ColumnCount { record, limit } => {
                write!(f, "Record {record} has more than {limit} columns")
            }
            Self::Filter(source) => write!(f, "Could not prepare the table filter: {source}"),
        }
    }
}

impl Error for TablePreviewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Filter(source) => Some(source),
            _ => None,
        }
    }
}
