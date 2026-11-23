use super::{
    image_bytes, Answer, Command, Image, Problem, Reply, FILE_LIMIT, PAGE_LIMIT, PDF_WORKER_ARG,
};
use crate::{
    file_error::{PdfAction, PdfError},
    operations::Cancellation,
    preview::helper::{read_message, write_blob, write_message, Process},
};
use std::{io::Read, time::Duration};

const PAGE_TIMEOUT: Duration = Duration::from_secs(8);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct Session {
    process: Process,
    pages: usize,
}

impl Session {
    pub(super) fn open(bytes: &[u8], cancel: &Cancellation) -> Result<Self, PdfError> {
        let action = PdfAction::Open;
        let io = |source| PdfError::Io { action, source };
        let rejected = |problem| PdfError::Rejected { action, problem };
        cancel.check().map_err(io)?;
        if bytes.len() > FILE_LIMIT {
            return Err(rejected(Problem::TooLarge));
        }
        if bytes.is_empty() {
            return Err(rejected(Problem::Invalid));
        }
        let mut process = Process::launch(PDF_WORKER_ARG).map_err(|source| PdfError::Io {
            action: PdfAction::LaunchHelper,
            source,
        })?;
        let (mut input, mut output) = process.pipes(PAGE_TIMEOUT, cancel);
        write_blob(&mut input, bytes).map_err(io)?;
        let reply = read_message(&mut output).map_err(io)?;
        cancel.check().map_err(io)?;
        match reply {
            Reply::Opened { pages } if (1..=PAGE_LIMIT).contains(&pages) => {
                Ok(Self { process, pages })
            }
            Reply::Error(reason) => Err(rejected(reason)),
            _ => Err(PdfError::InvalidResponse(action)),
        }
    }

    pub(super) fn pages(&self) -> usize {
        self.pages
    }

    pub(super) fn run(
        &mut self,
        command: &Command,
        cancel: &Cancellation,
    ) -> Result<Answer, PdfError> {
        let pages = self.pages;
        let (action, timeout) = match command {
            Command::Render { page, .. } => (PdfAction::RenderPage(*page), PAGE_TIMEOUT),
            Command::Search { .. } => (PdfAction::Search, SEARCH_TIMEOUT),
        };
        let io = |source| PdfError::Io { action, source };
        let (mut input, mut output) = self.process.pipes(timeout, cancel);
        write_message(&mut input, command).map_err(io)?;
        match (command, read_message(&mut output).map_err(io)?) {
            (
                Command::Render { page, edge, .. },
                Reply::Image {
                    width,
                    height,
                    points,
                },
            ) => {
                let len = image_bytes(width, height)
                    .filter(|_| {
                        width.max(height) == *edge
                            && points.iter().all(|value| {
                                value.is_finite() && *value > 0.0 && *value <= 100_000.0
                            })
                    })
                    .ok_or(PdfError::InvalidResponse(action))?;
                let mut rgba = vec![0; len];
                output
                    .read_exact(&mut rgba)
                    .map_err(|source| PdfError::Io {
                        action: PdfAction::ReadRaster(*page),
                        source,
                    })?;
                Ok(Answer::Page(Image {
                    width,
                    height,
                    points,
                    rgba,
                }))
            }
            (Command::Search { .. }, Reply::Matches(matches))
                if matches.iter().all(|&page| page < pages)
                    && matches.windows(2).all(|pair| pair[0] < pair[1]) =>
            {
                Ok(Answer::Matches(matches))
            }
            (_, Reply::Error(problem)) => Ok(Answer::Error(PdfError::Rejected { action, problem })),
            _ => Err(PdfError::InvalidResponse(action)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{operations::TransferError, vfs::Location};

    #[test]
    fn pdf_open_preserves_cancellation_before_decoding() {
        let cancel = Cancellation::default();
        cancel.request();
        let Err(error) = Session::open(b"", &cancel) else {
            panic!("a cancelled PDF request must stop before opening a helper");
        };
        let location = Location::Local("/fixtures/selected.pdf".into());
        assert!(matches!(
            TransferError::from(error.at(&location)),
            TransferError::Cancelled
        ));
    }
}
