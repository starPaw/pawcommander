use super::PreviewError;
use crate::vfs::Location;
use std::{error::Error, fmt, str::Utf8Error};

#[derive(Debug)]
pub(crate) enum TextDecodeError {
    Utf8 {
        source: Utf8Error,
        byte: Option<u64>,
    },
    Malformed {
        byte: u64,
        length: u8,
        consumed_after: u8,
    },
    IncompleteUnit {
        byte: u64,
    },
    ForbiddenCharacter(char),
    InvalidCellBoundary,
    BufferLimit,
}

impl TextDecodeError {
    /// `origin` is present when these bytes are a contiguous source slice.
    pub(crate) fn utf8(source: Utf8Error, origin: Option<u64>) -> Self {
        Self::Utf8 {
            source,
            byte: origin.map(|origin| origin.saturating_add(source.valid_up_to() as u64)),
        }
    }

    /// `read_through` includes buffered bytes from previous decoder calls.
    pub(crate) fn malformed(read_through: u64, length: u8, consumed_after: u8) -> Self {
        Self::Malformed {
            byte: read_through.saturating_sub(u64::from(length) + u64::from(consumed_after)),
            length,
            consumed_after,
        }
    }

    pub(crate) fn at(
        self,
        location: &Location,
        encoding: &'static str,
        record: Option<u64>,
    ) -> PreviewError {
        PreviewError::Text {
            location: location.clone(),
            encoding,
            record,
            cause: self,
        }
    }
}

impl fmt::Display for TextDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Utf8 { source, byte: Some(byte) } => write!(f, "{source} (file byte {byte})"),
            Self::Utf8 { source, byte: None } => source.fmt(f),
            Self::Malformed { byte, length, consumed_after } => write!(
                f,
                "Malformed byte sequence at file byte {byte} (length {length}, consumed {consumed_after} subsequent bytes)"
            ),
            Self::IncompleteUnit { byte } => write!(f, "Incomplete encoding unit at file byte {byte}"),
            Self::ForbiddenCharacter(value) => write!(f, "Character U+{:04X} is invalid in this preview", u32::from(*value)),
            Self::InvalidCellBoundary => f.write_str("A column boundary splits a text character"),
            Self::BufferLimit => f.write_str("The text decoder exceeded its buffer limit"),
        }
    }
}

impl Error for TextDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Utf8 { source, .. } => Some(source),
            _ => None,
        }
    }
}
