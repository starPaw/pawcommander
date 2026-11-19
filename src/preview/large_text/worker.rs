use super::{
    reader::{changed, read_error, PageText, Reader},
    Direction, Hit, Position,
};
use crate::{
    operations::Cancellation,
    preview::{
        code::Code,
        source::{Source, Version},
    },
    vfs::Location,
};
use eframe::egui;
use std::{sync::mpsc, thread};

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
    pub jump: Option<u64>,
}

pub(super) enum ResultPage {
    Page(Page),
    NoMatch,
    NoLine,
}

type Reply = Result<(Version, ResultPage), String>;

struct Job {
    work: Work,
    version: Option<Version>,
    reply: mpsc::Sender<Reply>,
    cancel: Cancellation,
}

pub(super) struct Pending {
    replies: mpsc::Receiver<Reply>,
    cancel: Cancellation,
}

impl Pending {
    pub(super) fn poll(&self) -> Option<Reply> {
        match self.replies.try_recv() {
            Ok(reply) => Some(reply),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(unavailable())),
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancel.request();
    }
}

pub(super) struct Worker(mpsc::Sender<Job>);

impl Worker {
    pub(super) fn start(location: Location, ctx: &egui::Context) -> Result<Self, String> {
        let (requests, jobs) = mpsc::channel::<Job>();
        let ctx = ctx.clone();
        let viewport = ctx.viewport_id();
        thread::Builder::new()
            .name("pawcommander-text".into())
            .spawn(move || {
                while let Ok(mut job) = jobs.recv() {
                    while let Ok(newer) = jobs.try_recv() {
                        job = newer;
                    }
                    if job.cancel.is_requested() {
                        continue;
                    }
                    let reply = run(&location, &job.work, job.version, &job.cancel);
                    if !job.cancel.is_requested() && job.reply.send(reply).is_ok() {
                        ctx.request_repaint_of(viewport);
                    }
                }
            })
            .map_err(read_error)?;
        Ok(Self(requests))
    }

    pub(super) fn load(&self, work: Work, version: Option<Version>) -> Result<Pending, String> {
        let (reply, replies) = mpsc::channel();
        let cancel = Cancellation::default();
        self.0
            .send(Job {
                work,
                version,
                reply,
                cancel: cancel.clone(),
            })
            .map_err(|_| unavailable())?;
        Ok(Pending { replies, cancel })
    }
}

fn run(
    location: &Location,
    work: &Work,
    expected: Option<Version>,
    cancel: &Cancellation,
) -> Reply {
    cancel.check().map_err(read_error)?;
    let mut source = Source::open(location)?;
    let version = source.version().map_err(read_error)?;
    if expected.is_some_and(|expected| expected != version) {
        return Err(changed());
    }

    let mut reader = Reader::new(&mut source, version.size, cancel);
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
    cancel.check().map_err(read_error)?;
    if source.version().map_err(read_error)? != version {
        return Err(changed());
    }
    let result = if let Some(text) = text {
        let code = Code::numbered(location.path(), &text.text, text.start.line);
        ResultPage::Page(Page {
            text,
            code,
            hit,
            jump,
        })
    } else if matches!(work, Work::Match { .. }) {
        ResultPage::NoMatch
    } else {
        ResultPage::NoLine
    };
    Ok((version, result))
}

fn unavailable() -> String {
    crate::i18n::text("Служба предпросмотра недоступна").into()
}

#[cfg(test)]
pub(crate) use tests::check_sftp_large_text;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn check_sftp_large_text(location: &Location) {
        let cancel = Cancellation::default();
        let (version, first) = run(
            location,
            &Work::Page {
                index: 0,
                near: Position::START,
            },
            None,
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
            &cancel,
        )
        .unwrap();
        let ResultPage::Page(found) = found else {
            panic!("expected SFTP match");
        };
        assert_eq!(found.hit.unwrap().start.line, 4097);
        assert!(!found.text.has_next);
        assert!(found.text.text.contains("REMOTE PREVIEW MARKER"));
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
            &cancel,
        )
        .unwrap();
        let ResultPage::Page(first) = first else {
            panic!("expected first page");
        };
        assert!(first.text.has_next);
        std::fs::write(file.path(), b"replacement").unwrap();
        assert!(run(
            &location,
            &Work::Page {
                index: 1,
                near: first.text.start
            },
            Some(version),
            &cancel,
        )
        .is_err());
    }
}
