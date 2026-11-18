use super::{
    image_bytes, Answer, Command, Image, Problem, Reply, FILE_LIMIT, PAGE_LIMIT, PDF_WORKER_ARG,
};
use crate::operations::Cancellation;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
    process::{Child, ChildStdin, ChildStdout, Stdio},
    time::{Duration, Instant},
};

const MESSAGE_LIMIT: usize = 128 * 1024;
const PAGE_TIMEOUT: Duration = Duration::from_secs(8);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) fn read_size(input: &mut impl Read) -> io::Result<usize> {
    let mut buffer = [0; 4];
    input.read_exact(&mut buffer)?;
    Ok(u32::from_le_bytes(buffer) as usize)
}

pub(super) fn read_message<T: DeserializeOwned>(input: &mut impl Read) -> io::Result<T> {
    let size = read_size(input)?;
    if !(1..=MESSAGE_LIMIT).contains(&size) {
        return Err(bad_response());
    }
    let mut bytes = vec![0; size];
    input.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub(super) fn write_blob(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let size = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    output.write_all(&size.to_le_bytes())?;
    output.write_all(bytes)
}

pub(super) fn write_message<T: Serialize>(output: &mut impl Write, message: &T) -> io::Result<()> {
    write_blob(output, &serde_json::to_vec(message)?)
}

pub(super) struct Session {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
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
        let executable = std::env::current_exe().map_err(problem)?;
        let mut command = std::process::Command::new(executable);
        command.arg(PDF_WORKER_ARG);
        let mut session = Self::spawn(&mut command).map_err(problem)?;
        let (mut input, mut output) = session.pipes(PAGE_TIMEOUT, cancel);
        write_blob(&mut input, bytes).map_err(problem)?;
        match read_message(&mut output).map_err(problem)? {
            Reply::Opened { pages } if (1..=PAGE_LIMIT).contains(&pages) => {
                session.pages = pages;
                Ok(session)
            }
            Reply::Error(reason) => Err(reason),
            _ => Err(Problem::Service),
        }
    }

    fn spawn(command: &mut std::process::Command) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child.stdin.take().expect("piped PDF input");
        let output = child.stdout.take().expect("piped PDF output");
        let session = Self {
            child,
            input,
            output,
            pages: 0,
        };
        nonblocking(&session.input)?;
        nonblocking(&session.output)?;
        Ok(session)
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
        let (mut input, mut output) = self.pipes(timeout, cancel);
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

    fn pipes<'a>(
        &'a mut self,
        duration: Duration,
        cancel: &'a Cancellation,
    ) -> (Timed<'a, ChildStdin>, Timed<'a, ChildStdout>) {
        let deadline = Instant::now() + duration;
        (
            Timed {
                stream: &mut self.input,
                deadline,
                cancel,
            },
            Timed {
                stream: &mut self.output,
                deadline,
                cancel,
            },
        )
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bad_response() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid PDF helper message")
}

fn problem(error: io::Error) -> Problem {
    match error.kind() {
        io::ErrorKind::TimedOut => Problem::Timeout,
        // A closed pipe is common when the framework aborts opening a bad PDF.
        _ => Problem::Service,
    }
}

fn nonblocking(stream: &impl AsRawFd) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    // Only the parent's end of each pipe is modified.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct Timed<'a, T> {
    stream: &'a mut T,
    deadline: Instant,
    cancel: &'a Cancellation,
}

impl<T: AsRawFd> Timed<'_, T> {
    fn transfer(
        &mut self,
        events: libc::c_short,
        mut operation: impl FnMut(&mut T) -> io::Result<usize>,
    ) -> io::Result<usize> {
        loop {
            self.cancel.check()?;
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match operation(self.stream) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                result => return result,
            }
            let mut fd = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events,
                revents: 0,
            };
            // Short waits allow the parent to stop a PDF operation when the
            // selection changes, even if the helper is stuck.
            let millis = remaining.as_millis().clamp(1, 50) as libc::c_int;
            if unsafe { libc::poll(&mut fd, 1, millis) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

impl<T: Read + AsRawFd> Read for Timed<'_, T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.transfer(libc::POLLIN, |stream| stream.read(buffer))
    }
}

impl<T: Write + AsRawFd> Write for Timed<'_, T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.transfer(libc::POLLOUT, |stream| stream.write(buffer))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.cancel.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_helper_pipe_observes_timeout_and_cancellation() {
        // The shell builtin waits on the pipe and creates no descendants.
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "IFS= read -r unused"]);
        let mut helper = Session::spawn(&mut command).unwrap();
        let cancel = Cancellation::default();
        let error = helper
            .pipes(Duration::from_millis(30), &cancel)
            .1
            .read_exact(&mut [0])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        cancel.request();
        let error = helper
            .pipes(Duration::from_secs(1), &cancel)
            .1
            .read_exact(&mut [0])
            .unwrap_err();
        assert!(crate::operations::TransferError::is_cancelled_io(&error));
        let pid = helper.child.id();
        drop(helper);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
