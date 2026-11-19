use super::{
    image_bytes, Answer, Command, Image, Problem, Reply, FILE_LIMIT, PAGE_LIMIT, PDF_WORKER_ARG,
};
use crate::{
    operations::Cancellation,
    preview::helper::{read_message, write_blob, write_message, Process},
};
use std::{
    io::{self, Read},
    time::Duration,
};

const PAGE_TIMEOUT: Duration = Duration::from_secs(8);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct Session {
    process: Process,
    pages: usize,
}

impl Session {
    pub(super) fn open(bytes: &[u8], cancel: &Cancellation) -> Result<Self, Problem> {
        if bytes.len() > FILE_LIMIT {
            return Err(Problem::TooLarge);
        }
        if bytes.is_empty() {
            return Err(Problem::Invalid);
        }
        let mut process = Process::launch(PDF_WORKER_ARG).map_err(problem)?;
        let (mut input, mut output) = process.pipes(PAGE_TIMEOUT, cancel);
        write_blob(&mut input, bytes).map_err(problem)?;
        match read_message(&mut output).map_err(problem)? {
            Reply::Opened { pages } if (1..=PAGE_LIMIT).contains(&pages) => {
                Ok(Self { process, pages })
            }
            Reply::Error(reason) => Err(reason),
            _ => Err(Problem::Service),
        }
    }

    pub(super) fn pages(&self) -> usize {
        self.pages
    }

    pub(super) fn run(
        &mut self,
        command: &Command,
        cancel: &Cancellation,
    ) -> Result<Answer, Problem> {
        let pages = self.pages;
        let timeout = match command {
            Command::Render { .. } => PAGE_TIMEOUT,
            Command::Search { .. } => SEARCH_TIMEOUT,
        };
        let (mut input, mut output) = self.process.pipes(timeout, cancel);
        write_message(&mut input, command).map_err(problem)?;
        match (command, read_message(&mut output).map_err(problem)?) {
            (
                Command::Render { edge, .. },
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
                    .ok_or(Problem::Service)?;
                let mut rgba = vec![0; len];
                output.read_exact(&mut rgba).map_err(problem)?;
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
            (_, Reply::Error(reason)) => Ok(Answer::Error(reason)),
            _ => Err(Problem::Service),
        }
    }
}

fn problem(error: io::Error) -> Problem {
    match error.kind() {
        io::ErrorKind::TimedOut => Problem::Timeout,
        // A closed pipe is common when the framework aborts opening a bad PDF.
        _ => Problem::Service,
    }
}
