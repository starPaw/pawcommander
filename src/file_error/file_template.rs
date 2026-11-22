use super::{AccessError, Failure};
use crate::operations::TransferError;
use crate::vfs::{file_templates::TextEncoding, Location};
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) enum FileTemplateError {
    Save {
        title: String,
        cause: Box<TransferError>,
    },
    EmptyTitle,
    TitleTooLong(usize),
    TitleHasControl,
    DuplicateTitle(String),
    LibraryCountLimit(usize),
    LibrarySizeLimit(usize),
    UnrepresentableCharacters(TextEncoding),
    InvalidSavedPayload {
        title: String,
        cause: base64::DecodeError,
    },
    SavedPayloadTooLarge(String),
    SourceMissing {
        source: Location,
        cause: Box<AccessError>,
    },
    TooLarge(Option<Location>),
}

impl FileTemplateError {
    pub(crate) fn saving(title: &str, cause: TransferError) -> TransferError {
        if matches!(cause, TransferError::Cancelled) {
            return cause;
        }
        Self::Save {
            title: title.to_owned(),
            cause: Box::new(cause),
        }
        .into()
    }
}

impl From<FileTemplateError> for TransferError {
    fn from(error: FileTemplateError) -> Self {
        Self::Failed(Failure::FileTemplate(Box::new(error)))
    }
}

impl fmt::Display for FileTemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Save { title, cause } => write!(f, "Saving template {title:?}: {cause}"),
            Self::EmptyTitle => f.write_str("A template name is required"),
            Self::TitleTooLong(limit) => {
                write!(f, "Template names support up to {limit} characters")
            }
            Self::TitleHasControl => f.write_str("The template name contains control characters"),
            Self::DuplicateTitle(title) => write!(f, "A template named {title:?} already exists"),
            Self::LibraryCountLimit(limit) => {
                write!(f, "The library supports up to {limit} templates")
            }
            Self::LibrarySizeLimit(limit) => {
                write!(f, "The template library would exceed {limit} bytes")
            }
            Self::UnrepresentableCharacters(encoding) => {
                write!(f, "Some characters cannot be represented in {encoding:?}")
            }
            Self::InvalidSavedPayload { title, cause } => {
                write!(f, "Invalid saved template {title:?} ({cause})")
            }
            Self::SavedPayloadTooLarge(title) => {
                write!(f, "Saved template {title:?} exceeds 4 MiB")
            }
            Self::SourceMissing { source, cause } => {
                write!(f, "The template source is missing at {source}: {cause}")
            }
            Self::TooLarge(Some(path)) => write!(f, "The template source at {path} exceeds 4 MiB"),
            Self::TooLarge(None) => f.write_str("The file template exceeds 4 MiB"),
        }
    }
}

impl Error for FileTemplateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Save { cause, .. } => Some(cause.as_ref()),
            Self::InvalidSavedPayload { cause, .. } => Some(cause),
            Self::SourceMissing { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }
}
