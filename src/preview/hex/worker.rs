use super::{PAGE_BYTES, PATTERN_LIMIT};
use crate::{
    file_error::PreviewError,
    operations::{Cancellation, TransferError},
    preview::{
        read_worker,
        source::{Source, Version, Window, READ_BLOCK},
    },
    vfs::Location,
};
use eframe::egui;
use memchr::memmem;
use std::ops::Range;

#[derive(Clone, Copy)]
pub(super) enum Direction {
    Forward,
    Backward,
}

pub(super) enum Work {
    Page(u64),
    Search {
        pattern: Vec<u8>,
        from: u64,
        direction: Direction,
    },
}

pub(super) struct Page {
    pub start: u64,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub has_next: bool,
    pub hit: Option<Range<u64>>,
}

pub(super) enum ResultPage {
    Page(Page),
    NoMatch,
    OutsideFile,
}

type Output = (Version, ResultPage);
pub(super) type Worker = read_worker::Worker<(Work, Option<Version>), Output>;
pub(super) type Pending = read_worker::Pending<Output>;

pub(super) fn start(location: Location, ctx: &egui::Context) -> Result<Worker, TransferError> {
    Worker::start("pawcommander-hex", ctx, move |(work, version), cancel| {
        run(&location, work, *version, cancel)
    })
}

fn run(
    location: &Location,
    work: &Work,
    expected: Option<Version>,
    cancel: &Cancellation,
) -> Result<Output, TransferError> {
    cancel.check()?;
    let mut source = Source::open(location)?;
    let version = source.version()?;
    if expected.is_some_and(|expected| version != expected) {
        return Err(PreviewError::ChangedWhileReading(location.clone()).into());
    }
    let mut window = Window::new(&mut source, version.size, cancel);
    let result = match work {
        Work::Page(offset) => read_page(&mut window, *offset, None)?,
        Work::Search {
            pattern,
            from,
            direction,
        } => {
            if let Some(start) = find(&mut window, version.size, *from, pattern, *direction)? {
                let end = start + pattern.len() as u64;
                read_page(&mut window, start, Some(start..end))?
            } else {
                ResultPage::NoMatch
            }
        }
    };
    cancel.check()?;
    if source.version()? != version {
        return Err(PreviewError::ChangedWhileReading(location.clone()).into());
    }
    Ok((version, result))
}

fn read_page(
    window: &mut Window<'_>,
    offset: u64,
    hit: Option<Range<u64>>,
) -> Result<ResultPage, TransferError> {
    let start = offset / PAGE_BYTES * PAGE_BYTES;
    let through = hit
        .as_ref()
        .map_or(PAGE_BYTES, |hit| (hit.end - start).max(PAGE_BYTES));
    let mut bytes = vec![0; through as usize + 1];
    let count = window.read_at(start, &mut bytes)?;
    if offset != 0 && offset - start >= count as u64 {
        return Ok(ResultPage::OutsideFile);
    }
    bytes.truncate(count.min(through as usize));
    Ok(ResultPage::Page(Page {
        start,
        offset,
        bytes,
        has_next: count as u64 > PAGE_BYTES,
        hit,
    }))
}

fn find(
    window: &mut Window<'_>,
    size: Option<u64>,
    from: u64,
    pattern: &[u8],
    direction: Direction,
) -> Result<Option<u64>, TransferError> {
    if pattern.is_empty() || pattern.len() > PATTERN_LIMIT {
        return Ok(None);
    }
    let overlap = (pattern.len() - 1) as u64;
    let limit = size.unwrap_or(u64::MAX);
    match direction {
        Direction::Forward => {
            let finder = memmem::Finder::new(pattern);
            let match_after = window.find_forward(from..limit, &finder, 1, |_| {})?;
            if match_after.is_some() || from == 0 {
                return Ok(match_after);
            }
            // The wrap range admits sequences that begin before the search
            // origin and cross it. Their starts still precede the origin.
            let end = from.saturating_add(overlap).min(limit);
            window.find_forward(0..end, &finder, 1, |_| {})
        }
        Direction::Backward => {
            let finder = memmem::FinderRev::new(pattern);
            if from > 0 {
                let end = if let Some(size) = size {
                    from.saturating_add(overlap).min(size)
                } else {
                    let mut tail = vec![0; overlap as usize];
                    from + window.read_at(from, &mut tail)? as u64
                };
                let match_before = window.find_backward(0..end, &finder, 1, |_| {})?;
                if match_before.is_some() {
                    return Ok(match_before);
                }
            }
            let end = if let Some(size) = size {
                size
            } else {
                find_end(window, from)?
            };
            window.find_backward(from..end, &finder, 1, |_| {})
        }
    }
}

fn find_end(window: &mut Window<'_>, mut at: u64) -> Result<u64, TransferError> {
    let mut buffer = [0; READ_BLOCK];
    loop {
        let count = window.read_at(at, &mut buffer)?;
        at += count as u64;
        if count < buffer.len() {
            return Ok(at);
        }
    }
}

#[cfg(test)]
pub(crate) use tests::check_sftp_hex;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn require_page(output: ResultPage) -> Page {
        let ResultPage::Page(page) = output else {
            panic!("expected hex page");
        };
        page
    }

    pub(crate) fn check_sftp_hex(location: &Location, at: u64, pattern: &[u8]) {
        let cancel = Cancellation::default();
        let (version, first) = run(location, &Work::Page(0), None, &cancel).unwrap();
        let page = require_page(first);
        assert_eq!(page.bytes.len() as u64, PAGE_BYTES);
        let (_, found) = run(
            location,
            &Work::Search {
                pattern: pattern.to_vec(),
                from: 0,
                direction: Direction::Forward,
            },
            Some(version),
            &cancel,
        )
        .unwrap();
        let found = require_page(found);
        let end = at + pattern.len() as u64;
        assert_eq!(found.hit, Some(at..end));
        assert_eq!(
            &found.bytes[(at - found.start) as usize..(end - found.start) as usize],
            pattern
        );
    }

    #[test]
    fn pages_read_only_a_window_and_search_keeps_cross_boundary_matches_visible() {
        const FIRST: usize = READ_BLOCK - 2;
        const SECOND: usize = READ_BLOCK * 3 + 10;
        let pattern = b"\xde\xad\xbe\xef";
        let mut bytes = vec![0u8; READ_BLOCK * 4 + 261];
        bytes[FIRST..FIRST + pattern.len()].copy_from_slice(pattern);
        bytes[SECOND..SECOND + pattern.len()].copy_from_slice(pattern);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();
        let mut source = Source::open(&Location::Local(file.path().into())).unwrap();
        let size = bytes.len() as u64;
        let cancel = Cancellation::default();
        let mut window = Window::new(&mut source, Some(size), &cancel);
        let first_page = require_page(read_page(&mut window, 0, None).unwrap());
        assert_eq!(first_page.bytes.len() as u64, PAGE_BYTES);
        assert_eq!(window.offset(), PAGE_BYTES + 1);
        assert!(first_page.has_next);
        assert_eq!(
            find(&mut window, Some(size), 0, pattern, Direction::Forward).unwrap(),
            Some(FIRST as u64)
        );
        assert_eq!(
            find(
                &mut window,
                Some(size),
                SECOND as u64,
                pattern,
                Direction::Backward
            )
            .unwrap(),
            Some(FIRST as u64)
        );
        assert_eq!(
            find(
                &mut window,
                Some(size),
                (FIRST + READ_BLOCK - 1) as u64,
                pattern,
                Direction::Backward
            )
            .unwrap(),
            Some(FIRST as u64)
        );
        assert_eq!(
            find(
                &mut window,
                Some(size),
                FIRST as u64,
                pattern,
                Direction::Backward
            )
            .unwrap(),
            Some(SECOND as u64)
        );
        assert_eq!(
            find(
                &mut window,
                Some(size),
                SECOND as u64 + 1,
                pattern,
                Direction::Forward
            )
            .unwrap(),
            Some(FIRST as u64)
        );
        let hit_page = require_page(
            read_page(
                &mut window,
                FIRST as u64,
                Some(FIRST as u64..FIRST as u64 + 4),
            )
            .unwrap(),
        );
        assert_eq!(hit_page.offset, FIRST as u64);
        assert_eq!(&hit_page.bytes[PAGE_BYTES as usize - 2..], pattern);
        assert!(hit_page.has_next);

        let mut source = Source::open(&Location::Local(file.path().into())).unwrap();
        let mut unknown_size = Window::new(&mut source, None, &cancel);
        assert_eq!(
            find(
                &mut unknown_size,
                None,
                FIRST as u64,
                pattern,
                Direction::Backward
            )
            .unwrap(),
            Some(SECOND as u64)
        );
        assert_eq!(
            find(
                &mut unknown_size,
                None,
                SECOND as u64,
                pattern,
                Direction::Backward
            )
            .unwrap(),
            Some(FIRST as u64)
        );
        assert!(matches!(
            read_page(&mut unknown_size, size, None).unwrap(),
            ResultPage::OutsideFile
        ));
        let last = require_page(read_page(&mut unknown_size, size - 1, None).unwrap());
        assert_eq!(last.bytes.len(), 261);
        assert!(!last.has_next);
        assert_eq!(std::fs::read(file.path()).unwrap(), bytes);
    }

    #[test]
    fn a_change_to_the_open_file_invalidates_the_saved_offsets() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let location = Location::Local(file.path().to_owned());
        let cancel = Cancellation::default();
        let (version, empty) = run(&location, &Work::Page(0), None, &cancel).unwrap();
        assert!(require_page(empty).bytes.is_empty());
        std::fs::write(file.path(), b"a different file size").unwrap();
        assert!(run(&location, &Work::Page(0), Some(version), &cancel).is_err());
    }
}
