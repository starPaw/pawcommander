use super::{
    reader::{PageText, Reader},
    Codec, Direction, Hit, Position,
};
use crate::{
    file_error::PreviewError,
    operations::{Cancellation, TransferError},
    preview::{
        code::Code,
        read_worker,
        source::{Source, Version},
    },
    vfs::Location,
};
use eframe::egui;
use std::ops::Range;

pub(super) enum Work {
    Page {
        index: u64,
        near: Position,
    },
    Match {
        query: String,
        from: Position,
        direction: Direction,
    },
    Line {
        number: u64,
        near: Position,
    },
}

pub(super) struct Page {
    pub text: PageText,
    pub code: Code,
    pub hit: Option<Hit>,
    pub hit_range: Option<Range<usize>>,
    pub jump: Option<usize>,
}

pub(super) enum ResultPage {
    Page(Box<Page>),
    NoMatch,
    NoLine,
}

type Output = (Version, ResultPage);
pub(super) type Worker = read_worker::Worker<(Work, Option<Version>), Output>;
pub(super) type Pending = read_worker::Pending<Output>;

pub(super) fn start(
    location: Location,
    encoding: Codec,
    ctx: &egui::Context,
) -> Result<Worker, TransferError> {
    Worker::start("pawcommander-text", ctx, move |(work, version), cancel| {
        run(&location, work, *version, encoding, cancel)
    })
}
fn run(
    location: &Location,
    work: &Work,
    expected: Option<Version>,
    encoding: Codec,
    cancel: &Cancellation,
) -> Result<Output, TransferError> {
    cancel.check()?;
    let mut source = Source::open(location)?;
    let version = source.version()?;
    if expected.is_some_and(|expected| expected != version) {
        return Err(PreviewError::ChangedWhileReading(location.clone()).into());
    }

    let mut reader = Reader::new(&mut source, version.size, encoding, cancel);
    let (text, hit, jump) = match work {
        Work::Page { index, near } => (Some(reader.page(*index, *near, None)?), None, None),
        Work::Match {
            query,
            from,
            direction,
        } => {
            if let Some(hit) = reader.find(*from, query, *direction)? {
                let text = reader.page_at(hit.start, Some(hit.end.byte))?;
                (Some(text), Some(hit), Some(hit.start.byte))
            } else {
                (None, None, None)
            }
        }
        Work::Line { number, near } => {
            if let Some(at) = reader.line(*number, *near)? {
                (Some(reader.page_at(at, None)?), None, Some(at.byte))
            } else {
                (None, None, None)
            }
        }
    };
    cancel.check()?;
    if source.version()? != version {
        return Err(PreviewError::ChangedWhileReading(location.clone()).into());
    }
    let result = if let Some(mut text) = text {
        let code = Code::numbered(location.path(), &text.text, text.start.line);
        let hit_range = hit.and_then(|hit| text.local_range(hit.start.byte..hit.end.byte));
        let jump = jump.and_then(|byte| {
            let byte = byte.max(text.origin);
            text.local_range(byte..byte).map(|range| range.start)
        });
        text.forget_encoded();
        ResultPage::Page(Box::new(Page {
            text,
            code,
            hit,
            hit_range,
            jump,
        }))
    } else if matches!(work, Work::Match { .. }) {
        ResultPage::NoMatch
    } else {
        ResultPage::NoLine
    };
    Ok((version, result))
}

#[cfg(test)]
pub(crate) use tests::check_sftp_large_text;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn check_sftp_large_text(location: &Location) {
        let cancel = Cancellation::default();
        let (_, prefix) = crate::vfs::preview_data(location, 3, &cancel).unwrap();
        let encoding = Codec::choose(None, &prefix);
        let (version, first) = run(
            location,
            &Work::Page {
                index: 0,
                near: Position::START,
            },
            None,
            encoding,
            &cancel,
        )
        .unwrap();
        let ResultPage::Page(first) = first else {
            panic!("expected first SFTP page");
        };
        assert!(first.text.has_next);
        let (_, found) = run(
            location,
            &Work::Match {
                query: "REMOTE PREVIEW MARKER".into(),
                from: first.text.start,
                direction: Direction::Forward,
            },
            Some(version),
            encoding,
            &cancel,
        )
        .unwrap();
        let ResultPage::Page(found) = found else {
            panic!("expected SFTP match");
        };
        assert_eq!(found.hit.unwrap().start.line, 4097);
        assert!(!found.text.has_next);
        assert_eq!(
            &found.text.text[found.hit_range.unwrap()],
            "REMOTE PREVIEW MARKER"
        );
    }

    #[test]
    fn continuation_rejects_a_file_changed_since_the_previous_page() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"normal line\n".repeat(5000).as_slice())
            .unwrap();
        let location = Location::Local(file.path().to_owned());
        let cancel = Cancellation::default();
        let (version, first) = run(
            &location,
            &Work::Page {
                index: 0,
                near: Position::START,
            },
            None,
            Codec::Utf8,
            &cancel,
        )
        .unwrap();
        let ResultPage::Page(first) = first else {
            panic!("expected first page");
        };
        assert!(first.text.has_next);
        std::fs::write(file.path(), b"replacement").unwrap();
        let changed = run(
            &location,
            &Work::Page {
                index: 1,
                near: first.text.start,
            },
            Some(version),
            Codec::Utf8,
            &cancel,
        )
        .err()
        .expect("changed source should refuse continuation");
        assert!(matches!(changed,
            TransferError::Failed(crate::file_error::Failure::Preview(cause))
            if matches!(cause.as_ref(), PreviewError::ChangedWhileReading(path) if path == &location)
        ));
    }
}
