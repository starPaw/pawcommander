use super::Failure;
use crate::operations::TransferError;
use std::{error::Error, fmt, io, path::PathBuf};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExportKind {
    Settings,
    DiagnosticReport,
}

#[derive(Clone, Debug)]
pub(crate) struct ExportTarget {
    pub(crate) kind: ExportKind,
    pub(crate) path: PathBuf,
}

impl ExportTarget {
    pub(crate) fn failure(&self, problem: ExportProblem) -> TransferError {
        TransferError::Failed(Failure::Export(Box::new(ExportError {
            target: self.clone(),
            problem,
        })))
    }

    pub(crate) fn operation_error(&self, cause: TransferError) -> TransferError {
        if matches!(cause, TransferError::Cancelled) {
            return cause;
        }
        let unconfirmed = matches!(cause, TransferError::PublicationUnconfirmed(_));
        let error = self.failure(ExportProblem::Operation(Box::new(cause)));
        if unconfirmed {
            error.with_unconfirmed_publication()
        } else {
            error
        }
    }

    pub(crate) fn worker_stopped(&self) -> TransferError {
        self.failure(ExportProblem::WorkerStopped)
            .with_unconfirmed_publication()
    }
}

#[derive(Debug)]
pub(crate) enum InvalidPreferences {
    Layout,
    Columns,
    Shortcuts,
}

#[derive(Debug)]
pub(crate) enum ExportIoAction {
    CreateTemporary,
    Write,
    Sync,
    Publish,
}

#[derive(Debug)]
pub(crate) enum ExportProblem {
    InvalidSettings(InvalidPreferences),
    EncodeSettings(serde_json::Error),
    Io {
        action: ExportIoAction,
        source: io::Error,
    },
    Operation(Box<TransferError>),
    WorkerStopped,
}

#[derive(Debug)]
pub(crate) struct ExportError {
    pub(crate) target: ExportTarget,
    pub(crate) problem: ExportProblem,
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.target.kind {
            ExportKind::Settings => "Settings",
            ExportKind::DiagnosticReport => "Diagnostic report",
        };
        write!(f, "{name} export at {}: ", self.target.path.display())?;
        match &self.problem {
            ExportProblem::InvalidSettings(problem) => write!(f, "Invalid settings ({problem:?})"),
            ExportProblem::EncodeSettings(error) => write!(f, "Could not encode settings: {error}"),
            ExportProblem::Io { action, source } => write!(f, "{action:?}: {source}"),
            ExportProblem::Operation(error) => fmt::Display::fmt(error, f),
            ExportProblem::WorkerStopped => f.write_str("The worker ended without a result"),
        }
    }
}

impl Error for ExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.problem {
            ExportProblem::EncodeSettings(error) => Some(error),
            ExportProblem::Io { source, .. } => Some(source),
            ExportProblem::Operation(error) => Some(error.as_ref()),
            ExportProblem::InvalidSettings(_) | ExportProblem::WorkerStopped => None,
        }
    }
}
