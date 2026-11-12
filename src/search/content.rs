use regex::{Regex, RegexBuilder};
use std::{
    fs::{File, Metadata},
    io::{self, Read},
};

pub(super) const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_REGEX_LINE_BYTES: usize = 64 * 1024;
const READ_CHUNK_BYTES: usize = 16 * 1024;
const EXCERPT_BYTES: usize = 240;
const BEFORE_MATCH_BYTES: usize = 60;

pub(super) struct LineMatch {
    pub line: usize,
    pub excerpt: String,
}

pub(super) enum Scan {
    Text {
        matches: Vec<LineMatch>,
        long_lines: usize,
    },
    TooLarge,
    Binary,
    Unreadable,
}

enum Pattern<'a> {
    Text(&'a str),
    Regex(Regex),
}

pub(super) struct Scanner<'a> {
    pattern: Pattern<'a>,
    bytes: Vec<u8>,
}

impl<'a> Scanner<'a> {
    pub fn new(query: &'a str, regex: bool) -> Result<Self, String> {
        let pattern = if regex {
            let regex = RegexBuilder::new(query)
                .size_limit(512 * 1024)
                .dfa_size_limit(512 * 1024)
                .build()
                .map_err(|error| {
                    crate::tr_format!(
                        "Некорректное или слишком сложное регулярное выражение: {error}",
                        "Invalid or too complex regular expression: {error}"
                    )
                })?;
            Pattern::Regex(regex)
        } else {
            Pattern::Text(query)
        };
        Ok(Self {
            pattern,
            bytes: Vec::new(),
        })
    }

    pub fn scan(
        &mut self,
        file: File,
        metadata: Metadata,
        match_limit: usize,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Scan, String> {
        if metadata.len() > MAX_FILE_BYTES {
            return Ok(Scan::TooLarge);
        }
        self.scan_reader(file, match_limit, cancelled)
    }

    fn scan_reader(
        &mut self,
        reader: impl Read,
        match_limit: usize,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Scan, String> {
        self.bytes.clear();
        let mut reader = reader.take(MAX_FILE_BYTES + 1);
        let mut chunk = [0; READ_CHUNK_BYTES];
        loop {
            super::check_cancelled(cancelled)?;
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Ok(Scan::Unreadable),
            };
            if (self.bytes.len() + read) as u64 > MAX_FILE_BYTES {
                return Ok(Scan::TooLarge);
            }
            if chunk[..read].contains(&0) {
                return Ok(Scan::Binary);
            }
            self.bytes.extend_from_slice(&chunk[..read]);
        }
        // Validate the complete file before publishing any match from it.
        let Ok(text) = std::str::from_utf8(&self.bytes) else {
            return Ok(Scan::Binary);
        };
        let mut matches = Vec::new();
        let mut long_lines = 0;
        for (index, line) in text.lines().enumerate() {
            if matches.len() >= match_limit {
                break;
            }
            super::check_cancelled(cancelled)?;
            let found = match &self.pattern {
                Pattern::Text(query) => line.find(query),
                // A regex call cannot be interrupted mid-line.
                Pattern::Regex(_) if line.len() > MAX_REGEX_LINE_BYTES => {
                    long_lines += 1;
                    continue;
                }
                Pattern::Regex(regex) => regex.find(line).map(|found| found.start()),
            };
            if let Some(found) = found {
                matches.push(LineMatch {
                    line: index + 1,
                    excerpt: excerpt(line, found),
                });
            }
        }
        Ok(Scan::Text {
            matches,
            long_lines,
        })
    }
}

fn excerpt(line: &str, match_start: usize) -> String {
    let mut start = match_start.saturating_sub(BEFORE_MATCH_BYTES);
    while !line.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (start + EXCERPT_BYTES).min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < line.len() { "…" } else { "" };
    format!("{prefix}{}{suffix}", &line[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn rejects_a_file_that_exceeds_the_read_limit_or_becomes_binary_after_a_match() {
        let mut scanner = Scanner::new("hit", false).unwrap();
        let mut file = vec![b'a'; MAX_FILE_BYTES as usize];
        file[..4].copy_from_slice(b"hit\n");
        assert!(matches!(
            scanner.scan_reader(&file[..], 10, &|| false).unwrap(),
            Scan::Text { matches, long_lines: 0 } if matches.len() == 1
        ));
        file.push(b'a');
        assert!(matches!(
            scanner.scan_reader(&file[..], 10, &|| false).unwrap(),
            Scan::TooLarge
        ));
        for last in [b'\0', 0xff] {
            file.truncate(MAX_FILE_BYTES as usize - 1);
            file.push(last);
            assert!(matches!(
                scanner.scan_reader(&file[..], 10, &|| false).unwrap(),
                Scan::Binary
            ));
        }
    }

    #[test]
    fn regex_skips_long_lines_but_literal_search_can_match_them() {
        let file = format!("{}hit\nhit\n", "é".repeat(MAX_REGEX_LINE_BYTES / 2));
        let mut scanner = Scanner::new("hit", true).unwrap();
        assert!(matches!(
            scanner.scan_reader(file.as_bytes(), 10, &|| false).unwrap(),
            Scan::Text { matches, long_lines: 1 } if matches.len() == 1 && matches[0].line == 2
        ));
        let mut scanner = Scanner::new("hit", false).unwrap();
        assert!(matches!(
            scanner.scan_reader(file.as_bytes(), 10, &|| false).unwrap(),
            Scan::Text { matches, long_lines: 0 } if matches.len() == 2
        ));
    }

    #[test]
    fn excerpt_keeps_the_match_start_and_respects_unicode_boundaries() {
        let line = format!("{}{}", "я🙂".repeat(200), "é🙂".repeat(100));
        let start = line.find('é').unwrap();
        let snippet = excerpt(&line, start);
        assert!(snippet.starts_with('…'));
        assert!(snippet.contains("é🙂"));
        assert!(snippet.ends_with('…'));
        assert!(snippet.len() <= EXCERPT_BYTES + 2 * '…'.len_utf8());
        assert!(excerpt(&line, line.len()).ends_with('🙂'));
    }

    #[test]
    fn cancellation_is_checked_between_reads_and_before_matching() {
        let mut scanner = Scanner::new("hit", false).unwrap();
        let mut file = vec![b'a'; READ_CHUNK_BYTES * 2];
        file[..4].copy_from_slice(b"hit\n");
        for cancel_at in [2, 4] {
            let checks = Cell::new(0);
            let cancelled = || {
                checks.set(checks.get() + 1);
                checks.get() == cancel_at
            };
            assert!(scanner.scan_reader(&file[..], 10, &cancelled).is_err());
        }
    }
}
