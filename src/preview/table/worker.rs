use super::{
    records::{self, Records, SAMPLE_BYTES},
    Codec, Position, Row, Separator,
};
use crate::{
    file_error::{AccessError, Failure, PreviewError, TablePreviewError},
    operations::{Cancellation, TransferError},
    preview::source::{Source, Target, Version},
    vfs::Location,
};
use eframe::egui;
use std::{
    io::{BufRead, BufReader, Read},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

pub(super) const PAGE_ROWS: usize = 128;
const PAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(super) struct Settings {
    pub separator: Option<Separator>,
    pub header: bool,
    pub query: String,
}

pub(super) struct Layout {
    pub separator: Separator,
    pub headers: Vec<String>,
    version: Version,
}

pub(super) struct Bookmark {
    pub layout: Arc<Layout>,
    pub position: Position,
}

#[derive(Default)]
pub(super) struct Update {
    pub ready: Option<Bookmark>,
    pub rows: Vec<Row>,
    pub scanned: u64,
    pub finished: Option<Result<Option<Position>, TransferError>>,
}

pub(super) struct Scan {
    updates: Arc<Mutex<Update>>,
    cancel: Cancellation,
}

impl Scan {
    pub(super) fn take_update(&self) -> Update {
        std::mem::take(&mut *self.updates.lock().expect("table updates"))
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.cancel.request();
    }
}

pub(super) struct Worker(mpsc::Sender<Request>);

struct Request {
    settings: Settings,
    start: Option<Bookmark>,
    updates: Arc<Mutex<Update>>,
    cancel: Cancellation,
}

struct Reporter<'a> {
    request: &'a Request,
    ctx: &'a egui::Context,
    viewport: egui::ViewportId,
}

impl Reporter<'_> {
    fn send(&self, change: impl FnOnce(&mut Update)) {
        if !self.request.cancel.is_requested() {
            change(&mut self.request.updates.lock().expect("table updates"));
            self.ctx.request_repaint_of(self.viewport);
        }
    }
}

impl Worker {
    pub(super) fn start(
        target: Target,
        preferred: Separator,
        encoding: Codec,
        ctx: &egui::Context,
    ) -> Result<Self, TransferError> {
        let (sender, incoming) = mpsc::channel::<Request>();
        let ctx = ctx.clone();
        let viewport = ctx.viewport_id();
        thread::Builder::new()
            .name("pawcommander-table".into())
            .spawn(move || {
                while let Ok(mut request) = incoming.recv() {
                    while let Ok(newer) = incoming.try_recv() {
                        request = newer;
                    }
                    if request.cancel.is_requested() {
                        continue;
                    }
                    let reporter = Reporter {
                        request: &request,
                        ctx: &ctx,
                        viewport,
                    };
                    let result = read_page(&target, preferred, encoding, &reporter);
                    reporter.send(|update| update.finished = Some(result));
                }
            })?;
        Ok(Self(sender))
    }

    pub(super) fn load(
        &self,
        settings: Settings,
        start: Option<Bookmark>,
    ) -> Result<Scan, TransferError> {
        let scan = Scan {
            updates: Arc::new(Mutex::new(Update::default())),
            cancel: Cancellation::default(),
        };
        self.0
            .send(Request {
                settings,
                start,
                updates: scan.updates.clone(),
                cancel: scan.cancel.clone(),
            })
            .map_err(|_| PreviewError::WorkerUnavailable)?;
        Ok(scan)
    }
}

fn read_page(
    target: &Target,
    preferred: Separator,
    encoding: Codec,
    reporter: &Reporter<'_>,
) -> Result<Option<Position>, TransferError> {
    let request = reporter.request;
    request.cancel.check()?;
    let mut source = Source::open(target)?;
    let version = source.version()?;
    let read = scan_page(
        &target.location,
        preferred,
        encoding,
        &mut source,
        version,
        reporter,
    );
    request.cancel.check()?;
    let next = source.checked(read)?;
    if source.version()? != version {
        return Err(PreviewError::ChangedWhileReading(target.location.clone()).into());
    }
    // A full page whose last row ends at EOF has no continuation.
    Ok(next.filter(|at| version.size.is_none_or(|size| at.byte < size)))
}

fn scan_page(
    location: &Location,
    preferred: Separator,
    encoding: Codec,
    source: &mut Source,
    version: Version,
    reporter: &Reporter<'_>,
) -> Result<Option<Position>, TransferError> {
    let request = reporter.request;
    let mut position = request
        .start
        .as_ref()
        .map_or_else(Position::default, |start| start.position);
    let mut sample = Vec::new();
    if let Some(bookmark) = &request.start
        && bookmark.layout.version != version
    {
        return Err(PreviewError::ChangedWhileReading(location.clone()).into());
    }
    let known_separator = request
        .start
        .as_ref()
        .map(|start| start.layout.separator)
        .or(request.settings.separator);
    if position.byte == 0 {
        // Give the parser at least one byte after the BOM when possible. A
        // buffer containing only the BOM looks like EOF to the parser.
        let limit = known_separator.map_or(SAMPLE_BYTES + 1, |_| 4);
        let limit = version
            .size
            .map_or(limit, |size| size.min(limit as u64) as usize);
        sample = source.read_up_to(limit, &request.cancel)?;
    } else {
        source.seek_to(position.byte)?;
    }
    let separator = known_separator.unwrap_or_else(|| {
        records::detect(
            location,
            &sample[..sample.len().min(SAMPLE_BYTES)],
            sample.len() <= SAMPLE_BYTES,
            preferred,
            encoding,
            &request.cancel,
        )
    });

    let prefix_len = sample.len() as u64;
    let remaining = version.size.map_or(u64::MAX, |size| {
        size.saturating_sub(position.byte + prefix_len)
    });
    let bom = if position.byte == 0 && encoding.width() == 2 {
        encoding.bom(&sample)
    } else {
        0
    };
    position.byte += bom as u64;
    let input = (&sample[bom..]).chain(source.take(remaining));
    let buffer = BufReader::with_capacity(64 * 1024, input);
    let mut records = Records::new(buffer, separator, position, true, encoding);
    let layout = if let Some(bookmark) = &request.start {
        bookmark.layout.clone()
    } else {
        let mut headers = Vec::new();
        if request.settings.header
            && let Some(record) = records
                .next(location, &request.cancel)
                .map_err(|error| table_access_error(location, error))?
        {
            headers.extend(record.cells().map(str::to_owned));
        }
        Arc::new(Layout {
            separator,
            headers,
            version,
        })
    };
    let start = records.position();
    reporter.send(|update| {
        update.ready = Some(Bookmark {
            layout,
            position: start,
        });
        update.scanned = start.records;
    });

    let filter = if request.settings.query.is_empty() {
        None
    } else {
        Some(
            regex::RegexBuilder::new(&regex::escape(&request.settings.query))
                .case_insensitive(true)
                .build()
                .map_err(|source| TablePreviewError::Filter(source).at(location))?,
        )
    };
    let mut batch = Batch {
        rows: Vec::new(),
        scanned: start.records,
    };
    let result = collect_rows(
        location,
        &mut records,
        filter.as_ref(),
        &mut batch,
        reporter,
    )
    .map_err(|error| table_access_error(location, error));
    batch.publish(reporter);
    result
}

struct Batch {
    rows: Vec<Row>,
    scanned: u64,
}

impl Batch {
    fn publish(&mut self, reporter: &Reporter<'_>) {
        reporter.send(|update| {
            update.rows.append(&mut self.rows);
            update.scanned = self.scanned;
        });
    }
}

fn collect_rows<R: BufRead>(
    location: &Location,
    records: &mut Records<R>,
    filter: Option<&regex::Regex>,
    batch: &mut Batch,
    reporter: &Reporter<'_>,
) -> Result<Option<Position>, TransferError> {
    let mut count = 0;
    let mut bytes = 0;
    let mut last = Instant::now();
    while count < PAGE_ROWS {
        let before = records.position();
        let Some(record) = records.next(location, &reporter.request.cancel)? else {
            return Ok(None);
        };
        if filter.is_none_or(|query| record.cells().any(|cell| query.is_match(cell))) {
            let row_bytes = record.len() + record.cells().count() * std::mem::size_of::<String>();
            if bytes + row_bytes > PAGE_BYTES {
                return Ok(Some(before));
            }
            bytes += row_bytes;
            count += 1;
            batch.rows.push(Row {
                number: before.records + 1,
                cells: record.cells().map(str::to_owned).collect(),
            });
        }
        batch.scanned = before.records + 1;
        if (count == 1 && !batch.rows.is_empty())
            || batch.rows.len() >= 24
            || last.elapsed() >= Duration::from_millis(120)
        {
            batch.publish(reporter);
            last = Instant::now();
        }
    }
    Ok(Some(records.position()))
}

fn table_access_error(location: &Location, error: TransferError) -> TransferError {
    match error {
        TransferError::Failed(Failure::Io { source, .. }) => {
            AccessError::read(location, source).into()
        }
        error => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::TextDecodeError;
    use std::{fs::File, io::Write};

    fn load(
        location: &Location,
        query: &str,
        start: Option<Bookmark>,
        separator: Option<Separator>,
        encoding: Codec,
    ) -> (Update, Result<Option<Position>, TransferError>) {
        let request = Request {
            settings: Settings {
                separator,
                header: true,
                query: query.into(),
            },
            start,
            updates: Arc::new(Mutex::new(Update::default())),
            cancel: Cancellation::default(),
        };
        let ctx = egui::Context::default();
        let reporter = Reporter {
            request: &request,
            ctx: &ctx,
            viewport: ctx.viewport_id(),
        };
        let result = read_page(
            &location.clone().into(),
            Separator::Comma,
            encoding,
            &reporter,
        );
        let update = std::mem::take(&mut *request.updates.lock().unwrap());
        (update, result)
    }

    #[test]
    fn table_continues_from_full_pages_and_filters_past_the_initial_prefix() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixtures.CSV");
        let mut file = File::create(&path).unwrap();
        write!(
            file,
            "\u{feff}City;Notes;Count\r\n\"north;east\";\"said \"\"hi\"\"\r\nagain\";7\r\n"
        )
        .unwrap();
        let filler = "z".repeat(60);
        for i in 1..2500 {
            let name = if i == PAGE_ROWS {
                "\u{feff}Next page"
            } else {
                "City"
            };
            writeln!(file, "{name};{filler};{i}\r").unwrap();
        }
        write!(file, "End;ГЛУБЖЕ[1];2500").unwrap();
        drop(file);
        let location = Location::Local(path.clone());
        let original_size = path.metadata().unwrap().len();

        let (first, next) = load(&location, "", None, None, Codec::Utf8);
        let next = next.unwrap().unwrap();
        let first_bookmark = first.ready.unwrap();
        assert_eq!(first_bookmark.layout.separator, Separator::Semicolon);
        assert_eq!(first_bookmark.layout.headers, ["City", "Notes", "Count"]);
        assert_eq!(first.rows.len(), PAGE_ROWS);
        assert_eq!(
            first.rows[0].cells,
            ["north;east", "said \"hi\"\r\nagain", "7"]
        );
        assert!(next.byte < original_size / 4);

        let (manual, after_manual) =
            load(&location, "", None, Some(Separator::Semicolon), Codec::Utf8);
        assert_eq!(after_manual.unwrap(), Some(next));
        assert_eq!(
            manual.ready.unwrap().layout.headers,
            ["City", "Notes", "Count"]
        );
        assert_eq!(manual.rows[0].cells, first.rows[0].cells);

        let (second, _) = load(
            &location,
            "",
            Some(Bookmark {
                layout: first_bookmark.layout.clone(),
                position: next,
            }),
            None,
            Codec::Utf8,
        );
        assert_eq!(second.rows[0].number, PAGE_ROWS as u64 + 2);
        assert_eq!(second.rows[0].cells[0], "\u{feff}Next page");
        assert_eq!(second.rows[0].cells[2], PAGE_ROWS.to_string());
        let (previous, _) = load(
            &location,
            "",
            Some(Bookmark {
                layout: first_bookmark.layout.clone(),
                position: first_bookmark.position,
            }),
            None,
            Codec::Utf8,
        );
        assert_eq!(
            previous.rows.last().unwrap().number,
            first.rows.last().unwrap().number
        );

        let (filtered, next_filtered) = load(&location, "глубже[1]", None, None, Codec::Utf8);
        assert_eq!(next_filtered.unwrap(), None);
        assert_eq!(filtered.rows.len(), 1);
        assert_eq!(filtered.rows[0].number, 2502);
        assert_eq!(filtered.rows[0].cells[2], "2500");
        assert_eq!(path.metadata().unwrap().len(), original_size);

        std::fs::write(&path, "new;contents\n").unwrap();
        let (changed_page, result) = load(
            &location,
            "",
            Some(Bookmark {
                layout: first_bookmark.layout,
                position: next,
            }),
            None,
            Codec::Utf8,
        );
        assert!(result.is_err());
        assert!(changed_page.rows.is_empty());
    }

    #[test]
    fn encoded_tables_filter_and_resume_at_original_byte_offsets() {
        for (encoding, bom) in [
            (Codec::Utf16Be, b"\xfe\xff".as_slice()),
            (Codec::Windows1251, b""),
        ] {
            let file = tempfile::NamedTempFile::new().unwrap();
            let mut text =
                "Город;Описание\r\nПрага;\"Первая строка\r\nПродолжение\"\r\n".to_owned();
            for number in 2..=420 {
                text.push_str(&format!("{number};{}\r\n", "проверка".repeat(8)));
            }
            text.push_str("Конец;ПОЗДНИЙ[1]");
            let mut raw = bom.to_vec();
            raw.extend_from_slice(&encoding.encode(&text).unwrap());
            std::fs::write(file.path(), &raw).unwrap();
            let location = Location::Local(file.path().to_owned());

            let (first, next) = load(&location, "", None, None, encoding);
            assert_eq!(
                first.rows[0].cells,
                ["Прага", "Первая строка\r\nПродолжение"]
            );
            let start = first.ready.unwrap();
            assert_eq!(start.layout.headers, ["Город", "Описание"]);
            assert_eq!(start.layout.separator, Separator::Semicolon);
            let next = next.unwrap().unwrap();
            assert!(next.byte.is_multiple_of(encoding.width() as u64));
            let (second, _) = load(
                &location,
                "",
                Some(Bookmark {
                    layout: start.layout,
                    position: next,
                }),
                None,
                encoding,
            );
            assert_eq!(second.rows[0].number, 130);
            assert_eq!(second.rows[0].cells[0], "129");
            let (filtered, next) = load(&location, "поздний[1]", None, None, encoding);
            assert_eq!(next.unwrap(), None);
            assert_eq!(filtered.rows[0].cells, ["Конец", "ПОЗДНИЙ[1]"]);
            assert_eq!(std::fs::read(file.path()).unwrap(), raw);
        }
    }

    #[test]
    fn malformed_csv_utf8_keeps_the_prior_row_and_original_cause() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let bytes = b"City,Details\nPraha,ready\nBrno,\"broken\xff\"\n";
        std::fs::write(file.path(), bytes).unwrap();
        let location = Location::Local(file.path().to_owned());
        let (update, result) = load(&location, "", None, None, Codec::Utf8);
        assert_eq!(update.scanned, 2);
        assert_eq!(update.rows.len(), 1);
        assert_eq!(update.rows[0].number, 2);
        assert_eq!(update.rows[0].cells, ["Praha", "ready"]);
        let Err(TransferError::Failed(Failure::Preview(error))) = result else {
            panic!("expected an invalid UTF-8 error for row 3");
        };
        let PreviewError::Text {
            location: source,
            encoding: "UTF-8",
            record: Some(3),
            cause:
                TextDecodeError::Utf8 {
                    source: cause,
                    byte: None,
                },
        } = *error
        else {
            panic!("expected the original UTF-8 cause without a file offset");
        };
        assert_eq!(source, location);
        assert_eq!(cause.error_len(), Some(1));
        assert_eq!(std::fs::read(file.path()).unwrap(), bytes);
    }
}
