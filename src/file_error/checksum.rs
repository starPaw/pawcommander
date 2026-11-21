use super::PathError;
use crate::vfs::Location;
use std::{error::Error, fmt, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManifestLineError {
    Format,
    Digest,
    Escape,
    Path,
}

impl fmt::Display for ManifestLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Format => "Expected a SHA-256 checksum and a filename",
            Self::Digest => "Expected 64 hexadecimal digits",
            Self::Escape => "Invalid filename escape",
            Self::Path => "Invalid relative file path",
        })
    }
}

impl Error for ManifestLineError {}

#[derive(Debug)]
pub(crate) enum ChecksumError {
    InvalidName(PathError),
    InvalidSelection(Location),
    UnsupportedFilename(Location),
    SourceMissing(Location),
    RequiresManifest(Location),
    ManifestMissing(Location),
    InvalidLine {
        manifest: Location,
        row: usize,
        source: ManifestLineError,
    },
    DuplicatePath {
        manifest: Location,
        row: usize,
        path: PathBuf,
    },
    EmptyManifest(Location),
    TooLarge(Location),
    DirectoryChanged(Location),
}

impl fmt::Display for ChecksumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(source) => write!(f, "Invalid manifest name: {source}"),
            Self::InvalidSelection(folder) => write!(f, "Expected files from {folder}"),
            Self::UnsupportedFilename(file) => write!(f, "Cannot use {file} in a manifest"),
            Self::SourceMissing(file) => write!(f, "File is missing: {file}"),
            Self::RequiresManifest(file) => write!(f, "Expected a SHA-256 manifest at {file}"),
            Self::ManifestMissing(file) => write!(f, "Manifest is missing: {file}"),
            Self::InvalidLine {
                manifest,
                row,
                source,
            } => {
                write!(
                    f,
                    "Invalid SHA-256 manifest {manifest}, line {row}: {source}"
                )
            }
            Self::DuplicatePath {
                manifest,
                row,
                path,
            } => {
                write!(
                    f,
                    "Duplicate path {path:?} in manifest {manifest}, line {row}"
                )
            }
            Self::EmptyManifest(file) => write!(f, "SHA-256 manifest is empty: {file}"),
            Self::TooLarge(file) => write!(
                f,
                "SHA-256 manifest exceeds its size or entry limit: {file}"
            ),
            Self::DirectoryChanged(folder) => write!(f, "Manifest folder changed: {folder}"),
        }
    }
}

impl Error for ChecksumError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidName(source) => Some(source),
            Self::InvalidLine { source, .. } => Some(source),
            _ => None,
        }
    }
}
