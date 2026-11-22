use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt, path::PathBuf};

use super::AccessError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CollectionAction {
    Import,
    Export,
}

#[derive(Debug)]
pub(crate) enum CollectionUnavailable {
    Missing,
    InTrash,
    Access(Box<AccessError>),
}

impl fmt::Display for CollectionUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("The item is missing or the volume is unavailable"),
            Self::InTrash => f.write_str("The item is in the Trash"),
            Self::Access(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl Error for CollectionUnavailable {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Access(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum CollectionError {
    Document {
        action: CollectionAction,
        file: Location,
        cause: Box<TransferError>,
    },
    Entry {
        row: usize,
        path: Option<PathBuf>,
        cause: Box<TransferError>,
    },
    Json(serde_json::Error),
    ByteLimit(usize),
    EntryLimit(usize),
    Version(u32),
    ExportSuffix,
    ImportSelection,
    InvalidPath,
    OutsideFolder {
        resolved: PathBuf,
        folder: PathBuf,
    },
    WrongKind {
        folder: bool,
    },
    DuplicateEntry,
    LocalOnly(Location),
    RegularItem(Location),
    Trashed(Location),
    Changed(Location),
    Unavailable(CollectionUnavailable),
    AvailableAgain,
    InvalidName,
    NameInUse,
    CollectionLimit(usize),
    CatalogItemLimit(usize),
    InvalidTag,
    TagLimit,
    RelinkKind,
    AlreadyMember,
}

impl CollectionError {
    pub(crate) fn document(
        action: CollectionAction,
        file: &Location,
        cause: TransferError,
    ) -> TransferError {
        if matches!(cause, TransferError::Cancelled) {
            return cause;
        }
        let unconfirmed = matches!(cause, TransferError::PublicationUnconfirmed(_));
        let error: TransferError = Self::Document {
            action,
            file: file.clone(),
            cause: Box::new(cause),
        }
        .into();
        if unconfirmed {
            error.with_unconfirmed_publication()
        } else {
            error
        }
    }
}

impl fmt::Display for CollectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Document {
                action,
                file,
                cause,
            } => {
                write!(f, "{action:?} collection at {file}: {cause}")
            }
            Self::Entry { row, path, cause } => {
                write!(f, "Collection entry {row}")?;
                if let Some(path) = path {
                    write!(f, " at {}", path.display())?;
                }
                write!(f, ": {cause}")
            }
            Self::Json(error) => write!(f, "Collection JSON: {error}"),
            Self::ByteLimit(limit) => write!(f, "Collection file exceeds {limit} bytes"),
            Self::EntryLimit(limit) => write!(f, "Collection file has more than {limit} entries"),
            Self::Version(version) => write!(f, "Unsupported collection file version {version}"),
            Self::ExportSuffix => f.write_str("Expected a filename ending in .pawcollection.json"),
            Self::ImportSelection => {
                f.write_str("Expected a .pawcollection.json file in the active folder")
            }
            Self::InvalidPath => {
                f.write_str("Expected a relative path within the collection folder")
            }
            Self::OutsideFolder { resolved, folder } => write!(
                f,
                "{} is outside the collection folder {}",
                resolved.display(),
                folder.display()
            ),
            Self::WrongKind { folder: true } => f.write_str("Expected a folder"),
            Self::WrongKind { folder: false } => f.write_str("Expected a regular file"),
            Self::DuplicateEntry => f.write_str("The collection file contains a duplicate item"),
            Self::LocalOnly(location) => write!(f, "Expected a local file or folder at {location}"),
            Self::RegularItem(location) => {
                write!(f, "Expected a regular file or folder at {location}")
            }
            Self::Trashed(location) => write!(f, "{location} is in the Trash"),
            Self::Changed(location) => write!(f, "The collection item changed at {location}"),
            Self::Unavailable(error) => fmt::Display::fmt(error, f),
            Self::AvailableAgain => f.write_str("The item is available again"),
            Self::InvalidName => f.write_str("Invalid collection name"),
            Self::NameInUse => f.write_str("A collection with that name already exists"),
            Self::CollectionLimit(limit) => {
                write!(f, "The catalog supports at most {limit} collections")
            }
            Self::CatalogItemLimit(limit) => {
                write!(f, "The catalog supports at most {limit} items in total")
            }
            Self::InvalidTag => f.write_str("Invalid collection tag"),
            Self::TagLimit => f.write_str("Too many tags on the collection item"),
            Self::RelinkKind => f.write_str("Select the same type of item"),
            Self::AlreadyMember => f.write_str("This item already belongs to the collection"),
        }
    }
}

impl Error for CollectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Document { cause, .. } | Self::Entry { cause, .. } => Some(cause.as_ref()),
            Self::Json(error) => Some(error),
            Self::Unavailable(error) => Some(error),
            _ => None,
        }
    }
}
