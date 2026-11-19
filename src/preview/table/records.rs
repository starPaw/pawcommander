use super::{Position, Separator};
use crate::operations::Cancellation;
use csv_core::{ReadRecordResult, Reader, ReaderBuilder};
use std::io::{self, BufRead, Cursor};

pub(super) const SAMPLE_BYTES: usize = 32 * 1024;
const RECORD_BYTES: usize = 256 * 1024;
const COLUMNS: usize = 256;

pub(super) struct Records<R> {
    input: R,
    reader: Reader,
    output: Vec<u8>,
    ends: [usize; COLUMNS + 1],
    position: Position,
    complete_input: bool,
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
    ) -> Self {
        let mut reader = ReaderBuilder::new().delimiter(separator.byte()).build();
        if position.byte != 0 {
            // At a resumed record boundary the parser should treat a BOM as cell data.
            reader.read_record(b"\n", &mut [0], &mut [0]);
        }
        Self {
            input,
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
            cancel.check().map_err(read_error)?;
            let input = self.input.fill_buf().map_err(read_error)?;
            if input.is_empty() && !self.complete_input {
                return Ok(None);
            }
            let (status, consumed, bytes, columns) = self.reader.read_record(
                input,
                &mut self.output[written..],
                &mut self.ends[fields..],
            );
            self.input.consume(consumed);
            self.position.byte += consumed as u64;
            written += bytes;
            fields += columns;
            if written > RECORD_BYTES || status == ReadRecordResult::OutputFull {
                return Err(crate::tr_format!(
                    "Запись {number} превышает 256 КиБ.",
                    "Record {number} exceeds 256 KiB."
                ));
            }
            if fields > COLUMNS || status == ReadRecordResult::OutputEndsFull {
                return Err(crate::tr_format!(
                    "В записи {number} больше 256 столбцов.",
                    "Record {number} has more than 256 columns."
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
                        })
                        .ok_or_else(|| {
                            crate::tr_format!(
                                "Запись {number} содержит недопустимый текст UTF-8.",
                                "Record {number} contains invalid UTF-8 text."
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
    cancel: &Cancellation,
) -> Separator {
    let mut best = (0, 0);
    let mut separator = preferred;
    let options = std::iter::once(preferred).chain(
        Separator::ALL
            .into_iter()
            .filter(|&value| value != preferred),
    );
    for candidate in options {
        let mut records = Records::new(
            Cursor::new(sample),
            candidate,
            Position::default(),
            complete,
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
    crate::tr_format!(
        "Не удалось прочитать таблицу: {error}",
        "Could not read table: {error}"
    )
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
            detect(source.as_bytes(), true, Separator::Tab, &cancel),
            Separator::Tab
        );
        let input = BufReader::with_capacity(5, Cursor::new(source.as_bytes()));
        let mut records = Records::new(input, Separator::Tab, Position::default(), true);
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
            );
            assert!(records.next(&cancel).is_err());
        }
    }
}
