use super::Failure;
use crate::operations::TransferError;
use crate::vfs::{file_templates::TextEncoding, Location};
use std::{error::Error, fmt};

#[derive(Debug)]
pub(crate) enum FileTemplateError {
    UnrepresentableCharacters(TextEncoding),
    InvalidSavedPayload(base64::DecodeError),
    SourceMissing(Location),
    TooLarge(Option<Location>),
}

impl From<FileTemplateError> for TransferError {
    fn from(error: FileTemplateError) -> Self {
        Self::Failed(Failure::FileTemplate(Box::new(error)))
    }
}

impl fmt::Display for FileTemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnrepresentableCharacters(encoding) => {
                write!(f, "Some characters cannot be represented in {encoding:?}")
            }
            Self::InvalidSavedPayload(error) => write!(f, "Invalid saved template ({error})"),
            Self::SourceMissing(path) => write!(f, "The template source is missing at {path}"),
            Self::TooLarge(Some(path)) => write!(f, "The template source at {path} exceeds 4 MiB"),
            Self::TooLarge(None) => f.write_str("The file template exceeds 4 MiB"),
        }
    }
}

impl Error for FileTemplateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSavedPayload(error) => Some(error),
            _ => None,
        }
    }
}
