use super::AccessCause;
use crate::vfs::Location;
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) enum MetadataError {
    TargetChanged(Location),
    CleanupDirectoryChanged(Location),
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetChanged(target) => {
                write!(f, "Item {target} changed while preserving metadata")
            }
            Self::CleanupDirectoryChanged(target) => {
                write!(f, "Temporary folder {target} changed before cleanup")
            }
        }
    }
}

impl Error for MetadataError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetadataField {
    Permissions,
    AccessTime,
    ModificationTime,
}

#[derive(Debug)]
pub(crate) struct MetadataWarning {
    pub(crate) fields: Vec<MetadataField>,
    pub(crate) causes: Vec<AccessCause>,
}
