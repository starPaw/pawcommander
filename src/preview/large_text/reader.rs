use super::{Codec, Direction, Hit, Position, QUERY_CHARS};
use crate::{
    operations::{Cancellation, TransferError},
    preview::source::{Source, Window, READ_BLOCK as BLOCK},
};
use memchr::memmem;
use std::ops::Range;

const PAGE_BYTES: u64 = 32 * 1024;
const LINE_LOOKAHEAD: usize = 512;

pub(super) struct PageText {
    pub index: u64,
    pub start: Position,
    pub origin: u64,
    pub text: String,
    pub has_next: bool,
    encoded: Option<(Codec, Vec<u8>)>,
}

pub(super) struct Reader<'a> {
    source: Window<'a>,
    size: Option<u64>,
    encoding: Codec,
}

impl<'a> Reader<'a> {
    pub(super) fn new(
        source: &'a mut Source,
        size: Option<u64>,
        encoding: Codec,
        cancel: &'a Cancellation,
    ) -> Self {
        Self {
            source: Window::new(source, size, cancel),
            size,
            encoding,
        }
    }

    pub(super) fn page(
        &mut self,
        index: u64,
        near: Position,
        show_through: Option<u64>,
    ) -> Result<PageText, TransferError> {
        let first = self.boundary(index)?;
        let start = self.relocate(near, first)?;
        let nominal_end = (index + 1) * PAGE_BYTES - first;
        let mut data = vec![0; nominal_end as usize + LINE_LOOKAHEAD + QUERY_CHARS * 4 + 1];
        let count = self.read_at(first, &mut data)?;
        data.truncate(count);
        let end = split_at(&data, nominal_end as usize, self.encoding);
        let has_next = end < data.len();
        let through =
            show_through.map_or(end, |byte| (byte.saturating_sub(first) as usize).max(end));
        data.truncate(through);
        let origin = if first == 0 {
            let bom = self.encoding.bom(&data);
            data.drain(..bom);
            bom as u64
        } else {
            first
        };
        let (text, encoded) = if self.encoding == Codec::Utf8 {
            (
                String::from_utf8(data).map_err(|_| self.encoding.error())?,
                None,
            )
        } else {
            let text = self
                .encoding
                .decode(&data)
                .ok_or_else(|| self.encoding.error())?
                .into_owned();
            (text, Some((self.encoding, data)))
        };
        if !super::super::preview_text_chars(&text) {
            return Err(self.encoding.error().into());
        }
        Ok(PageText {
            index,
            start,
            origin,
            text,
            has_next,
            encoded,
        })
    }

    pub(super) fn page_at(
        &mut self,
        at: Position,
        through: Option<u64>,
    ) -> Result<PageText, TransferError> {
        let nominal = at.byte / PAGE_BYTES;
        let start = self.boundary(nominal)?;
        let index = if start > at.byte || (nominal > 0 && self.size == Some(start)) {
            nominal.saturating_sub(1)
        } else {
            nominal
        };
        let page = self.page(index, at, through)?;
        // Some SFTP servers omit size. A jump to the blank line after a final
        // newline still belongs on the last page if the nominal page is empty.
        if page.text.is_empty() && index != 0 {
            self.page(index - 1, at, through)
        } else {
            Ok(page)
        }
    }

    pub(super) fn find(
        &mut self,
        from: Position,
        query: &str,
        direction: Direction,
    ) -> Result<Option<Hit>, TransferError> {
        let mut prefix = [0; 3];
        let used = self.read_at(0, &mut prefix)?;
        let bom = self.encoding.bom(&prefix[..used]);
        let first = Position::START.after(&prefix[..bom], self.encoding);
        let from = if from.byte < first.byte { first } else { from };
        let Some(needle) = self
            .encoding
            .encode(query)
            .filter(|needle| !needle.is_empty())
        else {
            return Ok(None);
        };
        let overlap = needle.len() - self.encoding.width();
        match direction {
            Direction::Forward => {
                let finder = memmem::Finder::new(&needle);
                let hit = self.forward(from, self.size.unwrap_or(u64::MAX), &finder)?;
                if hit.is_some() || from.byte == first.byte {
                    return Ok(hit);
                }
                // Include a match that starts before the origin and ends
                // just after it. Only match starts are split by the origin.
                self.forward(first, from.byte.saturating_add(overlap as u64), &finder)
            }
            Direction::Backward => {
                let finder = memmem::FinderRev::new(&needle);
                if from.byte > first.byte {
                    let mut tail = vec![0; overlap];
                    let used = self.read_at(from.byte, &mut tail)?;
                    let aligned = used - used % self.encoding.width();
                    let hit = self.backward(
                        first.byte,
                        from.after(&tail[..aligned], self.encoding),
                        &finder,
                    )?;
                    if hit.is_some() {
                        return Ok(hit);
                    }
                }
                let end = self.end_position(from)?;
                if !end.byte.is_multiple_of(self.encoding.width() as u64) {
                    return Err(self.encoding.error().into());
                }
                self.backward(from.byte, end, &finder)
            }
        }
    }

    pub(super) fn line(
        &mut self,
        wanted: u64,
        mut at: Position,
    ) -> Result<Option<Position>, TransferError> {
        if wanted == 1 {
            return Ok(Some(Position::START));
        }
        let mut buffer = vec![0u8; BLOCK];
        if wanted > at.line {
            loop {
                let count = self.read_at(at.byte, &mut buffer)?;
                let bytes = &buffer[..count];
                if let Some(found) = self
                    .encoding
                    .line_breaks(bytes)
                    .nth((wanted - at.line - 1).min(BLOCK as u64) as usize)
                {
                    return Ok(Some(Position {
                        byte: at.byte + found as u64,
                        line: wanted,
                    }));
                }
                if count < BLOCK {
                    return Ok(None);
                }
                at = at.after(bytes, self.encoding);
            }
        }
        while at.byte > 0 {
            let count = at.byte.min(BLOCK as u64) as usize;
            let byte = at.byte - count as u64;
            let bytes = self.read_exact_at(byte, &mut buffer[..count])?;
            if let Some(found) = self
                .encoding
                .line_breaks(bytes)
                .rev()
                .nth((at.line - wanted).min(BLOCK as u64) as usize)
            {
                return Ok(Some(Position {
                    byte: byte + found as u64,
                    line: wanted,
                }));
            }
            at = at.before(bytes, self.encoding);
        }
        Ok(None)
    }

    fn forward(
        &mut self,
        mut at: Position,
        end: u64,
        finder: &memmem::Finder<'_>,
    ) -> Result<Option<Hit>, TransferError> {
        let encoding = self.encoding;
        let found = self
            .source
            .find_forward(at.byte..end, finder, encoding.width(), |bytes| {
                at = at.after(bytes, encoding);
            })?;
        Ok(found.map(|_| Hit {
            start: at,
            end: at.after(finder.needle(), encoding),
        }))
    }

    fn backward(
        &mut self,
        lower: u64,
        mut at: Position,
        finder: &memmem::FinderRev<'_>,
    ) -> Result<Option<Hit>, TransferError> {
        let encoding = self.encoding;
        let found =
            self.source
                .find_backward(lower..at.byte, finder, encoding.width(), |bytes| {
                    at = at.before(bytes, encoding);
                })?;
        Ok(found.map(|_| Hit {
            start: at,
            end: at.after(finder.needle(), encoding),
        }))
    }

    fn boundary(&mut self, index: u64) -> Result<u64, TransferError> {
        if index == 0 {
            return Ok(0);
        }
        let nominal = index * PAGE_BYTES;
        if let Some(size) = self.size
            && nominal >= size
        {
            return Ok(size);
        }
        let width = self.encoding.width();
        let base = nominal - width as u64;
        let mut edge = [0u8; LINE_LOOKAHEAD + 2];
        let count = self.read_at(base, &mut edge[..LINE_LOOKAHEAD + width])?;
        Ok(base + split_at(&edge[..count], width, self.encoding) as u64)
    }

    fn relocate(&mut self, from: Position, to: u64) -> Result<Position, TransferError> {
        let mut byte = from.byte.min(to);
        let end = from.byte.max(to);
        let mut lines = 0;
        let mut buffer = [0; BLOCK];
        while byte < end {
            let count = BLOCK.min((end - byte).min(usize::MAX as u64) as usize);
            let bytes = self.read_exact_at(byte, &mut buffer[..count])?;
            lines += self.encoding.line_breaks(bytes).count() as u64;
            byte += count as u64;
        }
        Ok(Position {
            byte: to,
            line: if to >= from.byte {
                from.line + lines
            } else {
                from.line.saturating_sub(lines).max(1)
            },
        })
    }

    fn end_position(&mut self, mut from: Position) -> Result<Position, TransferError> {
        if let Some(size) = self.size {
            return self.relocate(from, size);
        }
        let mut buffer = [0; BLOCK];
        loop {
            let count = self.read_at(from.byte, &mut buffer)?;
            from = from.after(&buffer[..count], self.encoding);
            if count < BLOCK {
                return Ok(from);
            }
        }
    }

    fn read_exact_at<'b>(
        &mut self,
        byte: u64,
        buffer: &'b mut [u8],
    ) -> Result<&'b [u8], TransferError> {
        self.source.read_exact_at(byte, buffer)
    }

    fn read_at(&mut self, byte: u64, buffer: &mut [u8]) -> Result<usize, TransferError> {
        self.source.read_at(byte, buffer)
    }
}

fn split_at(bytes: &[u8], nominal: usize, encoding: Codec) -> usize {
    let width = encoding.width();
    if nominal < width
        || nominal >= bytes.len()
        || encoding
            .line_breaks(&bytes[nominal - width..nominal])
            .next()
            .is_some()
    {
        return nominal.min(bytes.len());
    }
    let tail = &bytes[nominal..bytes.len().min(nominal + LINE_LOOKAHEAD)];
    if let Some(end) = encoding.line_breaks(tail).next() {
        return nominal + end;
    }
    let increment = if encoding == Codec::Utf8 {
        tail.iter()
            .take(3)
            .take_while(|&&byte| byte & 0xc0 == 0x80)
            .count()
    } else if width == 2
        && tail.len() >= width
        && (0xd800..=0xdbff).contains(&encoding.unit(&bytes[nominal - width..nominal]))
        && (0xdc00..=0xdfff).contains(&encoding.unit(tail))
    {
        width
    } else {
        0
    };
    nominal + increment
}

impl PageText {
    pub(super) fn local_range(&self, range: Range<u64>) -> Option<Range<usize>> {
        let start = usize::try_from(range.start.checked_sub(self.origin)?).ok()?;
        let end = usize::try_from(range.end.checked_sub(self.origin)?).ok()?;
        let (start, end) = if let Some((encoding, bytes)) = &self.encoded {
            (
                encoding.decode(bytes.get(..start)?)?.len(),
                encoding.decode(bytes.get(..end)?)?.len(),
            )
        } else {
            (start, end)
        };
        (start <= end && self.text.is_char_boundary(start) && self.text.is_char_boundary(end))
            .then_some(start..end)
    }

    pub(super) fn forget_encoded(&mut self) {
        self.encoded = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(bytes: &[u8]) -> (tempfile::NamedTempFile, Source) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        let source = Source::open(&crate::vfs::Location::Local(file.path().into())).unwrap();
        (file, source)
    }

    #[test]
    fn pages_preserve_utf8_and_every_byte_of_a_long_line() {
        let mut bytes = b"\xef\xbb\xbfFirst\r\n".to_vec();
        bytes.resize(PAGE_BYTES as usize - 2, b'a');
        bytes.extend_from_slice("🙂".as_bytes());
        bytes.resize(PAGE_BYTES as usize * 3 + 1300, b'b');
        bytes.extend_from_slice(b"\nLast short line\n");
        let (_file, mut source) = fixture(&bytes);
        let cancel = Cancellation::default();
        let mut reader = Reader::new(&mut source, None, Codec::Utf8, &cancel);
        let mut reconstructed = Vec::new();
        let mut near = Position::START;
        for index in 0..4 {
            let page = reader.page(index, near, None).unwrap();
            near = page.start;
            assert_eq!(page.start.line, if index == 0 { 1 } else { 2 });
            assert_eq!(page.has_next, index != 3);
            assert!(page.text.len() <= PAGE_BYTES as usize + LINE_LOOKAHEAD);
            reconstructed.extend_from_slice(page.text.as_bytes());
        }
        assert_eq!(reconstructed, bytes[3..]);
        let previous = reader.page(2, near, None).unwrap();
        assert_eq!(previous.start.byte, PAGE_BYTES * 2);
        let eof_line = reader.line(4, previous.start).unwrap().unwrap();
        assert_eq!(eof_line.byte, bytes.len() as u64);
        assert_eq!(reader.page_at(eof_line, None).unwrap().index, 3);
    }

    #[test]
    fn file_search_handles_block_and_page_boundaries_and_tracks_lines() {
        let query = "target🙂";
        let first_at = BLOCK - 2;
        let second_at = BLOCK * 3 - 4;
        let mut bytes = b"Top\r\n".to_vec();
        bytes.resize(first_at, b'x');
        bytes.extend_from_slice(query.as_bytes());
        bytes.extend_from_slice(b"\nline three\r\n");
        let fourth_line = bytes.len() as u64;
        bytes.resize(second_at, b'y');
        bytes.extend_from_slice(query.as_bytes());
        bytes.resize(second_at + 3 + BLOCK + 128, b'z');
        let (_file, mut source) = fixture(&bytes);
        let cancel = Cancellation::default();
        let mut reader = Reader::new(&mut source, Some(bytes.len() as u64), Codec::Utf8, &cancel);

        let first = reader
            .find(Position::START, query, Direction::Forward)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.start,
            Position {
                byte: first_at as u64,
                line: 2
            }
        );
        let page = reader.page_at(first.start, Some(first.end.byte)).unwrap();
        assert_eq!(page.index, 1);
        assert!(page.has_next);
        let range = page.local_range(first.start.byte..first.end.byte).unwrap();
        assert_eq!(&page.text[range], query);
        let straddles_origin = Position {
            byte: first.start.byte + 2,
            line: 2,
        };
        for direction in [Direction::Forward, Direction::Backward] {
            let unique = reader
                .find(straddles_origin, "xtarget🙂", direction)
                .unwrap()
                .unwrap();
            assert_eq!(unique.start.byte, first.start.byte - 1);
        }

        let second = reader
            .find(first.end, query, Direction::Forward)
            .unwrap()
            .unwrap();
        assert_eq!(
            second.start,
            Position {
                byte: second_at as u64,
                line: 4
            }
        );
        let from = Position {
            byte: (second_at + 3 + BLOCK) as u64,
            line: 4,
        };
        assert_eq!(
            reader.find(from, query, Direction::Backward).unwrap(),
            Some(second)
        );
        assert_eq!(
            reader
                .find(second.start, query, Direction::Backward)
                .unwrap(),
            Some(first)
        );
        assert_eq!(
            reader
                .find(first.start, query, Direction::Backward)
                .unwrap(),
            Some(second)
        );
        assert_eq!(
            reader.find(second.end, query, Direction::Forward).unwrap(),
            Some(first)
        );
        assert_eq!(
            reader.line(4, first.start).unwrap().unwrap().byte,
            fourth_line
        );
        assert_eq!(reader.line(2, second.start).unwrap().unwrap().byte, 5);
        assert_eq!(reader.line(999, second.start).unwrap(), None);
        assert_eq!(
            reader
                .find(Position::START, "TARGET", Direction::Forward)
                .unwrap(),
            None
        );
    }

    #[test]
    fn file_search_observes_cancellation_between_chunks() {
        let data = vec![b'x'; BLOCK * 32];
        let (_file, mut source) = fixture(&data);
        let mut checks = 0;
        let cancel = Cancellation::at_checkpoint(move || {
            checks += 1;
            checks == 12
        });
        let mut reader = Reader::new(&mut source, Some(data.len() as u64), Codec::Utf8, &cancel);
        assert!(reader
            .find(Position::START, "absent", Direction::Forward)
            .is_err());
        assert!(cancel.is_requested());
        assert!(reader.source.offset() < data.len() as u64);
    }

    #[test]
    fn utf16_pages_and_search_use_aligned_offsets_in_both_byte_orders() {
        for (encoding, bom, unaligned) in [
            (Codec::Utf16Le, b"\xff\xfe", "\u{4100}\u{4200}\u{0400}"),
            (Codec::Utf16Be, b"\xfe\xff", "\u{ff00}\u{4100}\u{4201}"),
        ] {
            let mut bytes = bom.to_vec();
            bytes.extend_from_slice(
                &encoding
                    .encode(&format!("Первый\r\n{unaligned}\u{410a}\u{0a41}"))
                    .unwrap(),
            );
            let pad = encoding.encode("x").unwrap();
            let surrogate = PAGE_BYTES as usize - 2;
            bytes.extend_from_slice(&pad.repeat((surrogate - bytes.len()) / 2));
            bytes.extend_from_slice(&encoding.encode("🙂").unwrap());
            let first_at = BLOCK - 2;
            bytes.extend_from_slice(&pad.repeat((first_at - bytes.len()) / 2));
            bytes.extend_from_slice(&encoding.encode("AB").unwrap());
            bytes.extend_from_slice(&pad.repeat(1700));
            bytes.extend_from_slice(&encoding.encode("\r\n").unwrap());
            let third_line = bytes.len() as u64;
            bytes.extend_from_slice(&encoding.encode("Последний AB\r\n").unwrap());
            let (_file, mut source) = fixture(&bytes);
            let cancel = Cancellation::default();
            let mut reader = Reader::new(&mut source, Some(bytes.len() as u64), encoding, &cancel);

            let first_page = reader.page(0, Position::START, None).unwrap();
            assert!(first_page.text.ends_with("🙂"));
            let second_page = reader.page(1, first_page.start, None).unwrap();
            assert_eq!(
                second_page.start,
                Position {
                    byte: PAGE_BYTES + 2,
                    line: 2
                }
            );

            let hit = reader
                .find(Position::START, "AB", Direction::Forward)
                .unwrap()
                .unwrap();
            assert_eq!(
                hit.start,
                Position {
                    byte: first_at as u64,
                    line: 2
                }
            );
            let shown = reader.page_at(hit.start, Some(hit.end.byte)).unwrap();
            let range = shown.local_range(hit.start.byte..hit.end.byte).unwrap();
            assert_eq!(&shown.text[range], "AB");
            let last = reader
                .find(hit.start, "AB", Direction::Backward)
                .unwrap()
                .unwrap();
            assert_eq!(last.start.line, 3);
            assert_eq!(
                reader.find(last.start, "AB", Direction::Backward).unwrap(),
                Some(hit)
            );
            assert_eq!(reader.line(3, hit.start).unwrap().unwrap().byte, third_line);
        }
    }
}
