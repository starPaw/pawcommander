use super::PreviewError;
use crate::vfs::Location;
use std::{error::Error, fmt, io};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PdfAction {
    LaunchHelper,
    Open,
    StartReader,
    RenderPage(usize),
    ReadRaster(usize),
    Search,
}

impl fmt::Display for PdfAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LaunchHelper => f.write_str("starting the PDF helper"),
            Self::Open => f.write_str("opening the PDF"),
            Self::StartReader => f.write_str("starting the PDF preview reader"),
            Self::RenderPage(page) => write!(f, "rendering PDF page {}", page.saturating_add(1)),
            Self::ReadRaster(page) => write!(f, "reading PDF page {}", page.saturating_add(1)),
            Self::Search => f.write_str("searching the PDF"),
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum PdfProblem {
    Invalid,
    Locked,
    TooLarge,
    TooManyPages,
    Page,
    Search,
}

impl fmt::Display for PdfProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "Invalid PDF document",
            Self::Locked => "PDF document is locked",
            Self::TooLarge => "PDF document exceeds the preview size limit",
            Self::TooManyPages => "PDF document exceeds the preview page limit",
            Self::Page => "Could not render the PDF page",
            Self::Search => "Could not search the PDF document",
        })
    }
}

impl Error for PdfProblem {}

#[derive(Debug)]
pub(crate) enum PdfError {
    Rejected {
        action: PdfAction,
        problem: PdfProblem,
    },
    Io {
        action: PdfAction,
        source: io::Error,
    },
    InvalidResponse(PdfAction),
    WorkerUnavailable,
}

impl PdfError {
    pub(crate) fn at(self, location: &Location) -> PreviewError {
        PreviewError::Pdf {
            location: location.clone(),
            cause: self,
        }
    }
}

impl fmt::Display for PdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { action, problem } => write!(f, "{problem} while {action}"),
            Self::Io { action, source } => write!(f, "I/O failure while {action}: {source}"),
            Self::InvalidResponse(action) => write!(f, "Invalid helper response while {action}"),
            Self::WorkerUnavailable => f.write_str("PDF preview reader is unavailable"),
        }
    }
}

impl Error for PdfError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected { problem, .. } => Some(problem),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
