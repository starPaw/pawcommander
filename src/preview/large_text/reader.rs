use super::{Direction, Hit, Position, QUERY_CHARS};
use crate::{operations::Cancellation, preview::source::Source};
use memchr::{memchr, memchr_iter, memmem, memrchr_iter};
use std::{
    io::{self, Read},
    ops::Range,
};

const PAGE_BYTES: u64 = 32 * 1024;
const LINE_LOOKAHEAD: usize = 512;
const BLOCK: usize = 64 * 1024;
const BOM: &[u8] = b"\xef\xbb\xbf";

pub(super) struct PageText {
    pub index: u64,
    pub start: Position,
    pub origin: u64,
    pub text: String,
    pub has_next: bool,
}

pub(super) struct Reader<'a> {
    source: &'a mut Source,
    size: Option<u64>,
    offset: u64,
    cancel: &'a Cancellation,
}

impl<'a> Reader<'a> {
    pub(super) fn new(source: &'a mut Source, size: Option<u64>, cancel: &'a Cancellation) -> Self {
        Self {
            source,
            size,
            offset: 0,
            cancel,
        }
    }

    pub(super) fn page(
        &mut self,
        index: u64,
        near: Position,
        show_through: Option<u64>,
    ) -> Result<PageText, String> {
        let first = self.boundary(index)?;
        let start = self.relocate(near, first)?;
        let nominal_end = (index + 1) * PAGE_BYTES - first;
        let mut data = vec![0; nominal_end as usize + LINE_LOOKAHEAD + QUERY_CHARS * 4 + 1];
        let count = self.read_at(first, &mut data)?;
        data.truncate(count);
        let end = split_at(&data, nominal_end as usize);
        let has_next = end < data.len();
        let through =
            show_through.map_or(end, |byte| (byte.saturating_sub(first) as usize).max(end));
        data.truncate(through);
        let origin = if first == 0 && data.starts_with(BOM) {
            data.drain(..BOM.len());
            BOM.len() as u64
        } else {
            first
        };
        let text = String::from_utf8(data).map_err(|_| invalid_text())?;
        if !super::super::preview_text_chars(&text) {
            return Err(invalid_text());
        }
        Ok(PageText {
            index,
            start,
            origin,
            text,
            has_next,
        })
    }

    pub(super) fn page_at(
        &mut self,
        at: Position,
        through: Option<u64>,
    ) -> Result<PageText, String> {
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
    ) -> Result<Option<Hit>, String> {
        let mut prefix = [0; 3];
        let used = self.read_at(0, &mut prefix)?;
        let first = if &prefix[..used] == BOM {
            Position::START.after(BOM)
        } else {
            Position::START
        };
        let from = if from.byte < first.byte { first } else { from };
        let needle = query.as_bytes();
        if needle.is_empty() {
            return Ok(None);
        }
        let overlap = needle.len() - 1;
        match direction {
            Direction::Forward => {
                let finder = memmem::Finder::new(needle);
                let hit = self.forward(from, self.size.unwrap_or(u64::MAX), &finder)?;
                if hit.is_some() || from.byte == first.byte {
                    return Ok(hit);
                }
                // Include a match that starts before the origin and ends
                // just after it. Only match starts are split by the origin.
                self.forward(first, from.byte.saturating_add(overlap as u64), &finder)
            }
            Direction::Backward => {
                let finder = memmem::FinderRev::new(needle);
                if from.byte > first.byte {
                    let mut tail = vec![0; overlap];
                    let used = self.read_at(from.byte, &mut tail)?;
                    let hit = self.backward(first.byte, from.after(&tail[..used]), &finder)?;
                    if hit.is_some() {
                        return Ok(hit);
                    }
                }
                let end = self.end_position(from)?;
                self.backward(from.byte, end, &finder)
            }
        }
    }

    pub(super) fn line(
        &mut self,
        wanted: u64,
        mut at: Position,
    ) -> Result<Option<Position>, String> {
        if wanted == 1 {
            return Ok(Some(Position::START));
        }
        let mut buffer = vec![0u8; BLOCK];
        if wanted > at.line {
            loop {
                let count = self.read_at(at.byte, &mut buffer)?;
                let bytes = &buffer[..count];
                if let Some(found) =
                    memchr_iter(b'\n', bytes).nth((wanted - at.line - 1).min(BLOCK as u64) as usize)
                {
                    return Ok(Some(Position {
                        byte: at.byte + found as u64 + 1,
                        line: wanted,
                    }));
                }
                if count < BLOCK {
                    return Ok(None);
                }
                at = at.after(bytes);
            }
        }
        while at.byte > 0 {
            let count = at.byte.min(BLOCK as u64) as usize;
            let byte = at.byte - count as u64;
            let bytes = self.read_exact_at(byte, &mut buffer[..count])?;
            if let Some(found) =
                memrchr_iter(b'\n', bytes).nth((at.line - wanted).min(BLOCK as u64) as usize)
            {
                return Ok(Some(Position {
                    byte: byte + found as u64 + 1,
                    line: wanted,
                }));
            }
            at = at.before(bytes);
        }
        Ok(None)
    }

    fn forward(
        &mut self,
        mut at: Position,
        end: u64,
        finder: &memmem::Finder<'_>,
    ) -> Result<Option<Hit>, String> {
        let overlap = finder.needle().len() - 1;
        let mut buffer = vec![0; BLOCK + overlap];
        let mut carried = 0;
        while at.byte + (carried as u64) < end {
            let offset = at.byte + carried as u64;
            let wanted = BLOCK.min((end - offset).min(usize::MAX as u64) as usize);
            let fresh = self.read_at(offset, &mut buffer[carried..carried + wanted])?;
            let used = carried + fresh;
            if let Some(index) = finder.find(&buffer[..used]) {
                let start = at.after(&buffer[..index]);
                return Ok(Some(Hit {
                    start,
                    end: start.after(finder.needle()),
                }));
            }
            if fresh < wanted {
                break;
            }
            carried = overlap.min(used);
            let consumed = used - carried;
            at = at.after(&buffer[..consumed]);
            buffer.copy_within(consumed..used, 0);
        }
        Ok(None)
    }

    fn backward(
        &mut self,
        lower: u64,
        mut at: Position,
        finder: &memmem::FinderRev<'_>,
    ) -> Result<Option<Hit>, String> {
        let end = at.byte;
        let overlap = (finder.needle().len() - 1) as u64;
        let mut buffer = vec![0; BLOCK + overlap as usize];
        while at.byte > lower {
            let byte = at.byte.saturating_sub(BLOCK as u64).max(lower);
            let block = (at.byte - byte) as usize;
            let tail = overlap.min(end - at.byte) as usize;
            let bytes = self.read_exact_at(byte, &mut buffer[..block + tail])?;
            at = at.before(&bytes[..block]);
            if let Some(index) = finder.rfind(bytes) {
                let start = at.after(&bytes[..index]);
                return Ok(Some(Hit {
                    start,
                    end: start.after(finder.needle()),
                }));
            }
        }
        Ok(None)
    }

    fn boundary(&mut self, index: u64) -> Result<u64, String> {
        if index == 0 {
            return Ok(0);
        }
        let nominal = index * PAGE_BYTES;
        if let Some(size) = self.size
            && nominal >= size
        {
            return Ok(size);
        }
        let mut edge = [0u8; LINE_LOOKAHEAD + 1];
        let count = self.read_at(nominal - 1, &mut edge)?;
        Ok(nominal - 1 + split_at(&edge[..count], 1) as u64)
    }

    fn relocate(&mut self, from: Position, to: u64) -> Result<Position, String> {
        let mut byte = from.byte.min(to);
        let end = from.byte.max(to);
        let mut lines = 0;
        let mut buffer = [0; BLOCK];
        while byte < end {
            let count = BLOCK.min((end - byte).min(usize::MAX as u64) as usize);
            let bytes = self.read_exact_at(byte, &mut buffer[..count])?;
            lines += memchr_iter(b'\n', bytes).count() as u64;
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

    fn end_position(&mut self, mut from: Position) -> Result<Position, String> {
        if let Some(size) = self.size {
            return self.relocate(from, size);
        }
        let mut buffer = [0; BLOCK];
        loop {
            let count = self.read_at(from.byte, &mut buffer)?;
            from = from.after(&buffer[..count]);
            if count < BLOCK {
                return Ok(from);
            }
        }
    }

    fn read_exact_at<'b>(&mut self, byte: u64, buffer: &'b mut [u8]) -> Result<&'b [u8], String> {
        let count = self.read_at(byte, buffer)?;
        if count != buffer.len() {
            return Err(changed());
        }
        Ok(buffer)
    }

    fn read_at(&mut self, byte: u64, buffer: &mut [u8]) -> Result<usize, String> {
        self.cancel.check().map_err(read_error)?;
        let limit = self.size.map_or(buffer.len(), |size| {
            size.saturating_sub(byte).min(buffer.len() as u64) as usize
        });
        if limit == 0 {
            return Ok(0);
        }
        if self.offset != byte {
            self.source.seek_to(byte).map_err(read_error)?;
            self.offset = byte;
        }
        let mut used = 0;
        while used < limit {
            self.cancel.check().map_err(read_error)?;
            match self.source.read(&mut buffer[used..limit]) {
                Ok(0) => break,
                Ok(count) => {
                    used += count;
                    self.offset += count as u64;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(read_error(error)),
            }
        }
        Ok(used)
    }
}

fn split_at(bytes: &[u8], nominal: usize) -> usize {
    if nominal == 0 || nominal >= bytes.len() || bytes[nominal - 1] == b'\n' {
        return nominal.min(bytes.len());
    }
    let tail = &bytes[nominal..bytes.len().min(nominal + LINE_LOOKAHEAD)];
    if let Some(newline) = memchr(b'\n', tail) {
        return nominal + newline + 1;
    }
    nominal
        + tail
            .iter()
            .take(3)
            .take_while(|&&byte| byte & 0xc0 == 0x80)
            .count()
}

impl PageText {
    pub(super) fn local_range(&self, range: Range<u64>) -> Option<Range<usize>> {
        let start = usize::try_from(range.start.checked_sub(self.origin)?).ok()?;
        let end = usize::try_from(range.end.checked_sub(self.origin)?).ok()?;
        (start <= end && self.text.is_char_boundary(start) && self.text.is_char_boundary(end))
            .then_some(start..end)
    }
}

pub(super) fn changed() -> String {
    crate::i18n::text("Текстовый файл изменился. Откройте быстрый просмотр заново.").into()
}

fn invalid_text() -> String {
    crate::i18n::text("Не удалось показать этот фрагмент как текст UTF-8.").into()
}

pub(super) fn read_error(error: io::Error) -> String {
    crate::tr_format!(
        "Не удалось прочитать текст: {error}",
        "Could not read text: {error}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(bytes: &[u8]) -> (tempfile::NamedTempFile, Source) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        let source = Source::Local(file.reopen().unwrap());
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
        let mut reader = Reader::new(&mut source, None, &cancel);
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
        assert_eq!(reconstructed, bytes[BOM.len()..]);
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
        let mut reader = Reader::new(&mut source, Some(bytes.len() as u64), &cancel);

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
        let mut reader = Reader::new(&mut source, Some(data.len() as u64), &cancel);
        assert!(reader
            .find(Position::START, "absent", Direction::Forward)
            .is_err());
        assert!(cancel.is_requested());
        assert!(reader.offset < data.len() as u64);
    }
}
