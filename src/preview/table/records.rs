use super::{Codec, Position, Separator};
use crate::operations::Cancellation;
use csv_core::{ReadRecordResult, Reader, ReaderBuilder};
use encoding_rs::{Decoder, DecoderResult};
use std::io::{self, BufRead, Cursor};

pub(super) const SAMPLE_BYTES: usize = 32 * 1024;
const RECORD_BYTES: usize = 256 * 1024;
const COLUMNS: usize = 256;

pub(super) struct Records<R> {
    input: TextInput<R>,
    reader: Reader,
    output: Vec<u8>,
    ends: [usize; COLUMNS + 1],
    position: Position,
    complete_input: bool,
}

struct TextInput<R> {
    raw: R,
    encoding: Codec,
    decoder: Option<Decoder>,
    text: String,
    consumed: usize,
    eof: bool,
    malformed: bool,
}

impl<R: BufRead> TextInput<R> {
    fn new(raw: R, encoding: Codec) -> Self {
        Self {
            raw,
            encoding,
            decoder: (encoding != Codec::Utf8).then(|| encoding.decoder()),
            text: String::new(),
            consumed: 0,
            eof: false,
            malformed: false,
        }
    }

    fn fill(&mut self, cancel: &Cancellation) -> Result<&[u8], String> {
        cancel.check().map_err(read_error)?;
        let Some(decoder) = self.decoder.as_mut() else {
            return self.raw.fill_buf().map_err(read_error);
        };
        while self.consumed == self.text.len() && !self.eof && !self.malformed {
            cancel.check().map_err(read_error)?;
            let bytes = self.raw.fill_buf().map_err(read_error)?;
            let last = bytes.is_empty();
            self.consumed = 0;
            self.text.clear();
            self.text.reserve(
                decoder
                    .max_utf8_buffer_length_without_replacement(bytes.len())
                    .ok_or_else(|| self.encoding.error())?,
            );
            let (status, read) =
                decoder.decode_to_string_without_replacement(bytes, &mut self.text, last);
            self.raw.consume(read);
            match status {
                DecoderResult::Malformed(..) => self.malformed = true,
                DecoderResult::InputEmpty => self.eof = last,
                DecoderResult::OutputFull if self.text.is_empty() => {
                    return Err(self.encoding.error())
                }
                DecoderResult::OutputFull => {}
            }
        }
        if self.consumed == self.text.len() && self.malformed {
            return Err(self.encoding.error());
        }
        Ok(&self.text.as_bytes()[self.consumed..])
    }

    fn consume(&mut self, count: usize) -> usize {
        if self.decoder.is_none() {
            self.raw.consume(count);
            return count;
        }
        let start = self.consumed;
        self.consumed += count;
        // Record boundaries and full input buffers end at a UTF-8 boundary.
        // A parser output limit can stop mid-character and is a terminal error.
        self.text
            .get(start..self.consumed)
            .map_or(0, |text| self.encoding.raw_len(text))
    }
}

pub(super) struct Record<'a> {
    text: &'a str,
    ends: &'a [usize],
}

impl Record<'_> {
    pub(super) fn cells(&self) -> impl Iterator<Item = &str> {
        self.ends.iter().scan(0, |start, &end| {
            Some(&self.text[std::mem::replace(start, end)..end])
        })
    }

    pub(super) fn len(&self) -> usize {
        self.text.len()
    }
}

impl<R: BufRead> Records<R> {
    pub(super) fn new(
        input: R,
        separator: Separator,
        position: Position,
        complete_input: bool,
        encoding: Codec,
    ) -> Self {
        let mut reader = ReaderBuilder::new().delimiter(separator.byte()).build();
        if position.byte != 0 {
            // At a resumed record boundary the parser should treat a BOM as cell data.
            reader.read_record(b"\n", &mut [0], &mut [0]);
        }
        Self {
            input: TextInput::new(input, encoding),
            reader,
            output: vec![0; RECORD_BYTES + 1],
            ends: [0; COLUMNS + 1],
            position,
            complete_input,
        }
    }

    pub(super) fn position(&self) -> Position {
        self.position
    }

    pub(super) fn next(&mut self, cancel: &Cancellation) -> Result<Option<Record<'_>>, String> {
        let number = self.position.records + 1;
        let mut written = 0;
        let mut fields = 0;
        loop {
            let input = self.input.fill(cancel)?;
            if input.is_empty() && !self.complete_input {
                return Ok(None);
            }
            let (status, consumed, bytes, columns) = self.reader.read_record(
                input,
                &mut self.output[written..],
                &mut self.ends[fields..],
            );
            self.position.byte += self.input.consume(consumed) as u64;
            written += bytes;
            fields += columns;
            if written > RECORD_BYTES || status == ReadRecordResult::OutputFull {
                return Err(crate::tr_format!(
                    "Record {number} exceeds 256 KiB.",
                    number = number
                ));
            }
            if fields > COLUMNS || status == ReadRecordResult::OutputEndsFull {
                return Err(crate::tr_format!(
                    "Record {number} has more than 256 columns.",
                    number = number
                ));
            }
            match status {
                ReadRecordResult::Record => {
                    let ends = &self.ends[..fields];
                    let text = std::str::from_utf8(&self.output[..written])
                        .ok()
                        .filter(|text| {
                            ends.iter().all(|&end| text.is_char_boundary(end))
                                && !text.as_bytes().contains(&0)
                                && !text.contains('\u{fffe}')
                        })
                        .ok_or_else(|| {
                            crate::tr_format!(
                                "Record {number} contains invalid {} text.",
                                self.input.encoding.name(),
                                number = number
                            )
                        })?;
                    self.position.records = number;
                    return Ok(Some(Record { text, ends }));
                }
                ReadRecordResult::End => return Ok(None),
                _ => {}
            }
        }
    }
}

pub(super) fn detect(
    sample: &[u8],
    complete: bool,
    preferred: Separator,
    encoding: Codec,
    cancel: &Cancellation,
) -> Separator {
    let mut best = (0, 0);
    let mut separator = preferred;
    let options = std::iter::once(preferred).chain(
        Separator::ALL
            .into_iter()
            .filter(|&value| value != preferred),
    );
    let bom = if encoding.width() == 2 {
        encoding.bom(sample)
    } else {
        0
    };
    for candidate in options {
        let mut records = Records::new(
            Cursor::new(&sample[bom..]),
            candidate,
            Position {
                byte: bom as u64,
                records: 0,
            },
            complete,
            encoding,
        );
        let mut widths = [0; 8];
        let mut count = 0;
        for width in &mut widths {
            let Ok(Some(record)) = records.next(cancel) else {
                break;
            };
            *width = record.ends.len();
            count += 1;
        }
        let widths = &widths[..count];
        let score = widths
            .iter()
            .copied()
            .filter(|&width| width > 1)
            .map(|width| {
                let consistent = widths.iter().filter(|&&other| other == width).count();
                (consistent, consistent as isize - count as isize)
            })
            .max()
            .unwrap_or((0, 0));
        if score > best {
            best = score;
            separator = candidate;
        }
    }
    separator
}

pub(super) fn read_error(error: io::Error) -> String {
    crate::tr_format!("Could not read table: {error}", error = error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn tsv_records_preserve_tab_quotes_multiline_and_trailing_empty_cells() {
        let source = "\u{feff}A\tB\tC\r\n\"one\ttwo\"\t\"say \"\"hi\"\"\r\nagain\"\t";
        let cancel = Cancellation::default();
        assert_eq!(
            detect(
                source.as_bytes(),
                true,
                Separator::Tab,
                Codec::Utf8,
                &cancel
            ),
            Separator::Tab
        );
        let input = BufReader::with_capacity(5, Cursor::new(source.as_bytes()));
        let mut records = Records::new(
            input,
            Separator::Tab,
            Position::default(),
            true,
            Codec::Utf8,
        );
        assert_eq!(
            records
                .next(&cancel)
                .unwrap()
                .unwrap()
                .cells()
                .collect::<Vec<_>>(),
            ["A", "B", "C"]
        );
        assert_eq!(
            records
                .next(&cancel)
                .unwrap()
                .unwrap()
                .cells()
                .collect::<Vec<_>>(),
            ["one\ttwo", "say \"hi\"\r\nagain", ""]
        );
        assert!(records.next(&cancel).unwrap().is_none());
        assert_eq!(records.position().byte, source.len() as u64);
    }

    #[test]
    fn table_record_limits_preserve_valid_edges_and_guard_utf8_field_boundaries() {
        let cancel = Cancellation::default();
        let at_limit = format!("{},{}\n", "x".repeat(RECORD_BYTES), ",".repeat(COLUMNS - 2));
        let mut records = Records::new(
            Cursor::new(at_limit.as_bytes()),
            Separator::Comma,
            Position::default(),
            true,
            Codec::Utf8,
        );
        let row = records.next(&cancel).unwrap().unwrap();
        assert_eq!(row.len(), RECORD_BYTES);
        assert_eq!(row.cells().count(), COLUMNS);

        for bad in [
            vec![b'x'; RECORD_BYTES + 1],
            vec![b','; COLUMNS],
            vec![0xc2, b',', 0xa9],
        ] {
            let mut records = Records::new(
                Cursor::new(bad),
                Separator::Comma,
                Position::default(),
                true,
                Codec::Utf8,
            );
            assert!(records.next(&cancel).is_err());
        }
    }

    #[test]
    fn utf16_table_decoder_keeps_good_rows_before_a_malformed_tail() {
        let source = "Город;\"строка 🙂\"\r\n";
        let encoding = Codec::Utf16Le;
        let mut bytes = encoding.encode(source).unwrap().into_owned();
        let good_length = bytes.len();
        bytes.extend_from_slice(&[0x3d, 0xd8]);
        let cancel = Cancellation::default();
        let mut records = Records::new(
            BufReader::with_capacity(3, Cursor::new(bytes)),
            Separator::Semicolon,
            Position::default(),
            true,
            encoding,
        );
        let first = records.next(&cancel).unwrap().unwrap();
        assert_eq!(first.cells().collect::<Vec<_>>(), ["Город", "строка 🙂"]);
        assert!(records.position().byte >= (good_length - 2) as u64);
        assert!(records.next(&cancel).is_err());
    }
}
