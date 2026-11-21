use std::{
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeRecordSection {
    Header,
    Event(usize),
    AccessTimes,
}

#[derive(Debug)]
pub(crate) enum ResumeRecordProblem {
    Read(io::Error),
    ReadDirectory(io::Error),
    Lock(io::Error),
    InUse,
    IdentityChanged,
    SizeLimit(u64),
    Json {
        section: ResumeRecordSection,
        source: serde_json::Error,
    },
    UnsupportedVersion(u32),
    VersionMismatch {
        stored: u32,
        expected: u32,
    },
    EventOrder(usize),
    InvalidAccessTimes,
    InvalidPaths,
    InvalidName,
    ScanLimit(usize),
}

#[derive(Debug)]
pub(crate) struct ResumeRecordError {
    pub(crate) path: PathBuf,
    pub(crate) problem: ResumeRecordProblem,
}

impl ResumeRecordError {
    pub(crate) fn new(path: &Path, problem: ResumeRecordProblem) -> Self {
        Self {
            path: path.to_owned(),
            problem,
        }
    }

    pub(crate) fn is_missing(&self) -> bool {
        matches!(&self.problem, ResumeRecordProblem::Read(error) if error.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for ResumeRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.problem {
            ResumeRecordProblem::Read(error) => {
                write!(f, "Could not read recovery record {path}: {error}")
            }
            ResumeRecordProblem::ReadDirectory(error) => {
                write!(f, "Could not read recovery folder {path}: {error}")
            }
            ResumeRecordProblem::Lock(error) => {
                write!(f, "Could not lock recovery record {path}: {error}")
            }
            ResumeRecordProblem::InUse => write!(f, "Recovery record {path} is in use"),
            ResumeRecordProblem::IdentityChanged => {
                write!(f, "Recovery record {path} changed identity")
            }
            ResumeRecordProblem::SizeLimit(limit) => {
                write!(f, "Recovery record {path} exceeds {limit} bytes")
            }
            ResumeRecordProblem::Json { section, source } => {
                write!(f, "Invalid {section:?} in recovery record {path}: {source}")
            }
            ResumeRecordProblem::UnsupportedVersion(version) => {
                write!(f, "Unsupported version {version} of recovery record {path}")
            }
            ResumeRecordProblem::VersionMismatch { stored, expected } => write!(
                f,
                "Recovery record {path} has version {stored}, expected {expected}"
            ),
            ResumeRecordProblem::EventOrder(event) => write!(
                f,
                "Invalid order at event {event} in recovery record {path}"
            ),
            ResumeRecordProblem::InvalidAccessTimes => {
                write!(f, "Invalid saved access times in recovery record {path}")
            }
            ResumeRecordProblem::InvalidPaths => {
                write!(f, "Invalid paths in recovery record {path}")
            }
            ResumeRecordProblem::InvalidName => write!(f, "Invalid recovery record name in {path}"),
            ResumeRecordProblem::ScanLimit(limit) => write!(
                f,
                "Stopped reading recovery folder {path} after {limit} entries"
            ),
        }
    }
}

impl Error for ResumeRecordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.problem {
            ResumeRecordProblem::Read(error)
            | ResumeRecordProblem::ReadDirectory(error)
            | ResumeRecordProblem::Lock(error) => Some(error),
            ResumeRecordProblem::Json { source, .. } => Some(source),
            _ => None,
        }
    }
}
