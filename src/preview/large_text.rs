mod reader;
mod view;
mod worker;

use super::encoding::Codec;
pub(super) use view::Document;
#[cfg(test)]
pub(crate) use worker::check_sftp_large_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    byte: u64,
    line: u64,
}

impl Position {
    const START: Self = Self { byte: 0, line: 1 };

    fn after(self, bytes: &[u8], encoding: Codec) -> Self {
        Self {
            byte: self.byte + bytes.len() as u64,
            line: self.line + encoding.line_breaks(bytes).count() as u64,
        }
    }

    fn before(self, bytes: &[u8], encoding: Codec) -> Self {
        Self {
            byte: self.byte - bytes.len() as u64,
            line: self
                .line
                .saturating_sub(encoding.line_breaks(bytes).count() as u64)
                .max(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hit {
    start: Position,
    end: Position,
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Backward,
}

const QUERY_CHARS: usize = 256;
