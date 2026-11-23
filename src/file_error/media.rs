use super::PreviewError;
use crate::vfs::Location;
use std::{error::Error, fmt, io};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaAction {
    StartController,
    LaunchHelper,
    SendSource,
    ReadOpenReply,
    SendCommand,
    ReadState,
    ReadFrame,
}

impl fmt::Display for MediaAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StartController => "start the media preview reader",
            Self::LaunchHelper => "launch the media helper",
            Self::SendSource => "send the file to the media helper",
            Self::ReadOpenReply => "read the media helper's opening reply",
            Self::SendCommand => "send a playback command",
            Self::ReadState => "read the playback state",
            Self::ReadFrame => "read the video frame",
        })
    }
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct MediaNativeError {
    pub(crate) domain: String,
    pub(crate) code: i64,
}

impl fmt::Display for MediaNativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.domain, self.code)
    }
}

impl Error for MediaNativeError {}

#[derive(Debug)]
pub(crate) enum MediaError {
    UnsupportedFormat,
    ControllerUnavailable,
    LoadingTimedOut,
    Io {
        action: MediaAction,
        source: io::Error,
    },
    InvalidResponse(MediaAction),
    Playback(Vec<MediaNativeError>),
}

impl MediaError {
    pub(crate) fn at(self, location: &Location) -> PreviewError {
        PreviewError::Media {
            location: location.clone(),
            cause: self,
        }
    }
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat => f.write_str("Unsupported media preview format"),
            Self::ControllerUnavailable => f.write_str("Media preview reader is unavailable"),
            Self::LoadingTimedOut => f.write_str("The media player timed out while loading"),
            Self::Io { action, source } => write!(f, "Could not {action}: {source}"),
            Self::InvalidResponse(action) => {
                write!(f, "Invalid media helper response while trying to {action}")
            }
            Self::Playback(errors) => {
                f.write_str("Could not play the media file")?;
                for error in errors {
                    write!(f, " · {error}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for MediaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Playback(errors) => errors.first().map(|error| error as _),
            _ => None,
        }
    }
}
