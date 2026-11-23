use super::PreviewError;
use crate::vfs::Location;
use image::ImageError;
use std::{error::Error, fmt, io};

#[derive(Debug)]
pub(crate) enum ImagePreviewError {
    FileLimit { limit: u64 },
    Detect(io::Error),
    Decode(ImageError),
}

impl ImagePreviewError {
    pub(crate) fn at(self, location: &Location) -> PreviewError {
        PreviewError::Image {
            location: location.clone(),
            cause: self,
        }
    }

    pub(super) fn io_cause(&self) -> Option<&io::Error> {
        match self {
            Self::Detect(source) | Self::Decode(ImageError::IoError(source)) => Some(source),
            _ => None,
        }
    }
}

impl fmt::Display for ImagePreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileLimit { limit } => write!(f, "Image preview exceeds {limit} bytes"),
            Self::Detect(source) => write!(f, "Could not detect the image format: {source}"),
            Self::Decode(source) => write!(f, "Could not decode the image: {source}"),
        }
    }
}

impl Error for ImagePreviewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Detect(source) => Some(source),
            Self::Decode(source) => Some(source),
            Self::FileLimit { .. } => None,
        }
    }
}
