use crate::{operations::TransferError, vfs::Location};
use std::{error::Error, fmt, io, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncFolderRole {
    Source,
    Destination,
    Left,
    Right,
}

#[derive(Debug)]
pub(crate) enum SyncVerification {
    StagedCopy,
    HeldOriginal,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SyncDifference {
    Missing,
    Unexpected,
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncExclusionProblem {
    AbsolutePath,
    MissingName,
    EmptyComponent,
    DotComponent,
    NulCharacter,
}

impl fmt::Display for SyncExclusionProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AbsolutePath => "Use a relative name or path",
            Self::MissingName => "Expected a name after ./",
            Self::EmptyComponent => "Empty path component",
            Self::DotComponent => "Path contains . or ..",
            Self::NulCharacter => "Path contains a NUL character",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncStateKind {
    Directory,
    CommonState,
    RunRecord,
    PairLock,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SyncStateAction {
    Inspect,
    CreateDirectory,
    Open,
    Lock,
    Read,
    CreateTemporary,
    Write,
    SyncFile,
    Publish,
    SyncSave,
    Remove,
    SyncRemoval,
}

#[derive(Debug)]
pub(crate) enum SyncStateProblem {
    Io(SyncStateAction, io::Error),
    Json(SyncStateAction, serde_json::Error),
    InvalidEncoding(base64::DecodeError),
    InvalidType,
    InvalidContents,
    UnsupportedVersion(u32),
    SizeLimit(u64),
    WrongPair,
    PairChanged,
    Changed,
    ReplacedLock,
    Busy,
    InsideFolder(PathBuf),
}

impl fmt::Display for SyncStateProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(action, source) => write!(f, "{action:?}: {source}"),
            Self::Json(action, source) => write!(f, "{action:?} JSON: {source}"),
            Self::InvalidEncoding(source) => write!(f, "Invalid encoded value: {source}"),
            Self::InvalidType => f.write_str("Unexpected filesystem object type"),
            Self::InvalidContents => f.write_str("Invalid record contents"),
            Self::UnsupportedVersion(version) => write!(f, "Unsupported version {version}"),
            Self::SizeLimit(limit) => write!(f, "Record exceeds the {limit}-byte limit"),
            Self::WrongPair => f.write_str("Record belongs to a different folder pair"),
            Self::PairChanged => f.write_str("The folder pair changed"),
            Self::Changed => f.write_str("The record changed after planning"),
            Self::ReplacedLock => f.write_str("The lock file was replaced"),
            Self::Busy => f.write_str("Synchronization is already running for these folders"),
            Self::InsideFolder(folder) => {
                write!(
                    f,
                    "The storage is inside synchronized folder {}",
                    folder.display()
                )
            }
        }
    }
}

impl Error for SyncStateProblem {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(_, source) => Some(source),
            Self::Json(_, source) => Some(source),
            Self::InvalidEncoding(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum SyncError {
    Folder {
        role: SyncFolderRole,
        folder: Location,
        cause: Box<TransferError>,
    },
    Apply {
        completed: usize,
        cause: Box<TransferError>,
    },
    VerificationMismatch {
        check: SyncVerification,
        difference: SyncDifference,
        original: Location,
        inspected: Location,
    },
    EntryTypeChanged(Location),
    LinkTargetChanged(Location),
    PlanEntryAdded(Location),
    PlanEntryMissing(Location),
    NameCollision(Location),
    RetainedStage(Location),
    Overlap {
        source: Location,
        destination: Location,
    },
    PlanLimit,
    ExcludedPath {
        target: Location,
        excluded: PathBuf,
    },
    InvalidExclusion {
        line: usize,
        problem: SyncExclusionProblem,
    },
    ExclusionsTextLimit(usize),
    ExclusionsCountLimit {
        limit: usize,
        line: usize,
    },
    State {
        kind: SyncStateKind,
        path: PathBuf,
        problem: SyncStateProblem,
    },
    DataDirectoryUnavailable,
    RootIdentityUnavailable,
    ServerIdentityUnavailable,
    ConflictChanged(PathBuf),
    CommonStateRequiresMatchingFolders,
    UnresolvedConflicts,
    ResultMismatch(Location),
}

impl SyncError {
    pub(crate) fn applying(completed: usize, cause: TransferError) -> TransferError {
        if matches!(cause, TransferError::Cancelled) {
            return cause;
        }
        let unconfirmed = matches!(cause, TransferError::PublicationUnconfirmed(_));
        let error: TransferError = Self::Apply {
            completed,
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

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Apply { completed, cause } => {
                write!(
                    f,
                    "Synchronization stopped after {completed} completed changes: {cause}"
                )
            }
            Self::VerificationMismatch {
                check,
                difference,
                original,
                inspected,
            } => {
                write!(
                    f,
                    "{check:?} does not match {original}: {difference:?} item at {inspected}"
                )
            }
            Self::Folder {
                role,
                folder,
                cause,
            } => {
                write!(f, "{role:?} folder {folder}: {cause}")
            }
            Self::EntryTypeChanged(location) => {
                write!(f, "Item type changed while scanning {location}")
            }
            Self::LinkTargetChanged(location) => {
                write!(f, "Symbolic link target changed while scanning {location}")
            }
            Self::PlanEntryAdded(location) => write!(f, "Item added after planning at {location}"),
            Self::PlanEntryMissing(location) => {
                write!(f, "Item disappeared after planning at {location}")
            }
            Self::NameCollision(location) => write!(f, "Names collide at {location}"),
            Self::RetainedStage(location) => write!(f, "Found a sync staging folder at {location}"),
            Self::Overlap {
                source,
                destination,
            } => {
                write!(f, "Sync folders overlap: {source} and {destination}")
            }
            Self::PlanLimit => f.write_str("Synchronization plan exceeded its entry or path limit"),
            Self::ExcludedPath { target, excluded } => write!(
                f,
                "Replacing or deleting {target} would affect excluded path {}",
                excluded.display()
            ),
            Self::InvalidExclusion { line, problem } => {
                write!(f, "Invalid exclusion on line {line}: {problem}")
            }
            Self::ExclusionsTextLimit(limit) => write!(f, "Exclusions exceed {limit} bytes"),
            Self::ExclusionsCountLimit { limit, line } => {
                write!(
                    f,
                    "Found more than {limit} distinct exclusions at line {line}"
                )
            }
            Self::State {
                kind,
                path,
                problem,
            } => {
                write!(f, "{kind:?} at {}: {problem}", path.display())
            }
            Self::DataDirectoryUnavailable => {
                f.write_str("The application data folder is unavailable")
            }
            Self::RootIdentityUnavailable => {
                f.write_str("Could not verify the folder identity for the common state")
            }
            Self::ServerIdentityUnavailable => {
                f.write_str("Could not confirm the server for the common state")
            }
            Self::ConflictChanged(path) => write!(f, "The conflict changed at {}", path.display()),
            Self::CommonStateRequiresMatchingFolders => {
                f.write_str("Both folders must have matching contents to save the common state")
            }
            Self::UnresolvedConflicts => {
                f.write_str("The synchronization plan has unresolved conflicts")
            }
            Self::ResultMismatch(folder) => {
                write!(f, "Folder {folder} differs from the applied plan")
            }
        }
    }
}

impl Error for SyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Folder { cause, .. } | Self::Apply { cause, .. } => Some(cause.as_ref()),
            Self::State { problem, .. } => Some(problem),
            _ => None,
        }
    }
}
