use std::{
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryRecordSection {
    Header,
    Event(usize),
    #[cfg(target_os = "macos")]
    AccessTimes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(target_os = "macos")]
pub(crate) enum ResumeRecordEvent {
    Publication,
    HoldingSource,
    SourceRemoved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryRecordAction {
    CreateDirectory,
    CreateFile,
    WriteInitial,
    SyncInitial,
    Publish,
    SyncPublication,
    Activate,
    CheckIdentity,
    #[cfg(target_os = "macos")]
    SeekEnd,
    #[cfg(target_os = "macos")]
    Append(ResumeRecordEvent),
    #[cfg(target_os = "macos")]
    SyncEvent(ResumeRecordEvent),
    Remove,
    SyncRemoval,
}

#[derive(Debug)]
pub(crate) enum RecoveryRecordProblem {
    Read(io::Error),
    ReadDirectory(io::Error),
    ScanStart(io::Error),
    ScanDisconnected,
    Lock(io::Error),
    Write {
        action: RecoveryRecordAction,
        source: io::Error,
    },
    Encode {
        #[cfg(target_os = "macos")]
        event: Option<ResumeRecordEvent>,
        source: serde_json::Error,
    },
    #[cfg(target_os = "macos")]
    AppendRollback {
        event: ResumeRecordEvent,
        source: io::Error,
        restore: io::Error,
    },
    InUse,
    AlreadyActive,
    InvalidType,
    IdentityChanged,
    MissingParent,
    SizeLimit(u64),
    Json {
        section: RecoveryRecordSection,
        source: serde_json::Error,
    },
    UnsupportedVersion(u32),
    IncompleteEvent(usize),
    OmittedInvalidEvents(usize),
    #[cfg(target_os = "macos")]
    VersionMismatch {
        stored: u32,
        expected: u32,
    },
    #[cfg(target_os = "macos")]
    EventOrder(usize),
    #[cfg(target_os = "macos")]
    InvalidAccessTimes,
    #[cfg(target_os = "macos")]
    InvalidPaths,
    #[cfg(target_os = "macos")]
    InvalidProof,
    #[cfg(target_os = "macos")]
    InvalidName,
    ScanLimit(usize),
    ReportLimit(usize),
}

#[derive(Debug)]
pub(crate) struct RecoveryRecordError {
    pub(crate) path: PathBuf,
    pub(crate) problem: RecoveryRecordProblem,
}

#[derive(Debug)]
pub(crate) enum RecoveryStartError {
    Unavailable,
    Record {
        cause: RecoveryRecordError,
        cleanup: Option<RecoveryRecordError>,
    },
}

impl fmt::Display for RecoveryStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => {
                f.write_str("Operation did not start: the recovery directory is unavailable")
            }
            Self::Record { cause, cleanup } => {
                write!(f, "Operation did not start. {cause}")?;
                if let Some(cleanup) = cleanup {
                    write!(f, "\n{cleanup}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for RecoveryStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable => None,
            Self::Record { cause, .. } => Some(cause),
        }
    }
}

impl RecoveryRecordError {
    pub(crate) fn new(path: &Path, problem: RecoveryRecordProblem) -> Self {
        Self {
            path: path.to_owned(),
            problem,
        }
    }

    pub(crate) fn io(path: &Path, action: RecoveryRecordAction, source: io::Error) -> Self {
        Self::new(path, RecoveryRecordProblem::Write { action, source })
    }

    pub(crate) fn encoding(path: &Path, source: serde_json::Error) -> Self {
        Self::new(
            path,
            RecoveryRecordProblem::Encode {
                #[cfg(target_os = "macos")]
                event: None,
                source,
            },
        )
    }

    pub(crate) fn is_missing(&self) -> bool {
        matches!(&self.problem, RecoveryRecordProblem::Read(error) if error.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for RecoveryRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.problem {
            RecoveryRecordProblem::Read(error) => {
                write!(f, "Could not read recovery record {path}: {error}")
            }
            RecoveryRecordProblem::ReadDirectory(error) => {
                write!(f, "Could not read recovery folder {path}: {error}")
            }
            RecoveryRecordProblem::ScanStart(error) => {
                write!(f, "Could not start recovery lookup in {path}: {error}")
            }
            RecoveryRecordProblem::ScanDisconnected => {
                write!(f, "Lost contact with the recovery worker for {path}")
            }
            RecoveryRecordProblem::Lock(error) => {
                write!(f, "Could not lock recovery record {path}: {error}")
            }
            RecoveryRecordProblem::Write { action, source } => {
                write!(f, "Recovery record {action:?} at {path}: {source}")
            }
            RecoveryRecordProblem::Encode { source, .. } => {
                write!(f, "Could not encode recovery record data at {path}: {source}")
            }
            #[cfg(target_os = "macos")]
            RecoveryRecordProblem::AppendRollback { event, source, restore } => write!(
                f,
                "Could not append {event:?} to recovery record {path}: {source}. Could not restore the previous length: {restore}"
            ),
            RecoveryRecordProblem::InUse => write!(f, "Recovery record {path} is in use"),
            RecoveryRecordProblem::AlreadyActive => write!(f, "The worker already has an active recovery record. Could not activate {path}"),
            RecoveryRecordProblem::InvalidType => write!(f, "Expected a regular file for recovery record {path}"),
            RecoveryRecordProblem::IdentityChanged => {
                write!(f, "Recovery record {path} changed identity")
            }
            RecoveryRecordProblem::MissingParent => write!(f, "Recovery record {path} has no parent directory"),
            RecoveryRecordProblem::SizeLimit(limit) => {
                write!(f, "Recovery record {path} exceeds {limit} bytes")
            }
            RecoveryRecordProblem::Json { section, source } => {
                write!(f, "Invalid {section:?} in recovery record {path}: {source}")
            }
            RecoveryRecordProblem::UnsupportedVersion(version) => {
                write!(f, "Unsupported version {version} of recovery record {path}")
            }
            RecoveryRecordProblem::IncompleteEvent(event) => {
                write!(f, "Event {event} in recovery record {path} is incomplete")
            }
            RecoveryRecordProblem::OmittedInvalidEvents(count) => {
                write!(f, "Recovery record {path} has {count} additional invalid events")
            }
            #[cfg(target_os = "macos")]
            RecoveryRecordProblem::VersionMismatch { stored, expected } => write!(
                f,
                "Recovery record {path} has version {stored}, expected {expected}"
            ),
            #[cfg(target_os = "macos")]
            RecoveryRecordProblem::EventOrder(event) => write!(
                f,
                "Invalid order at event {event} in recovery record {path}"
            ),
            #[cfg(target_os = "macos")]
            RecoveryRecordProblem::InvalidAccessTimes => {
                write!(f, "Invalid saved access times in recovery record {path}")
            }
            #[cfg(target_os = "macos")]
            RecoveryRecordProblem::InvalidPaths => {
                write!(f, "Invalid paths in recovery record {path}")
            }
            #[cfg(target_os = "macos")]
            RecoveryRecordProblem::InvalidProof => {
                write!(f, "Invalid transfer proof in recovery record {path}")
            }
            #[cfg(target_os = "macos")]
            RecoveryRecordProblem::InvalidName => write!(f, "Invalid recovery record name in {path}"),
            RecoveryRecordProblem::ScanLimit(limit) => write!(
                f,
                "Stopped reading recovery folder {path} after {limit} entries"
            ),
            RecoveryRecordProblem::ReportLimit(limit) => write!(f, "Recovery report for {path} reached the {limit}-byte limit"),
        }
    }
}

impl Error for RecoveryRecordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.problem {
            RecoveryRecordProblem::Lock(error)
            | RecoveryRecordProblem::Read(error)
            | RecoveryRecordProblem::ReadDirectory(error)
            | RecoveryRecordProblem::ScanStart(error)
            | RecoveryRecordProblem::Write { source: error, .. } => Some(error),
            #[cfg(target_os = "macos")]
            RecoveryRecordProblem::AppendRollback { source: error, .. } => Some(error),
            RecoveryRecordProblem::Encode { source, .. } => Some(source),
            RecoveryRecordProblem::Json { source, .. } => Some(source),
            _ => None,
        }
    }
}
