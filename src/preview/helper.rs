use crate::operations::Cancellation;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
    process::{Child, ChildStdin, ChildStdout, Stdio},
    time::{Duration, Instant},
};

const MESSAGE_LIMIT: usize = 128 * 1024;

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

pub(super) struct Process {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

impl Process {
    pub(super) fn launch(mode: &str) -> io::Result<Self> {
        let executable = std::env::current_exe()?;
        Self::spawn(std::process::Command::new(executable).arg(mode))
    }

    pub(super) fn spawn(command: &mut std::process::Command) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child.stdin.take().expect("piped helper input");
        let output = child.stdout.take().expect("piped helper output");
        let session = Self {
            child,
            input,
            output,
        };
        nonblocking(&session.input)?;
        nonblocking(&session.output)?;
        Ok(session)
    }

    pub(super) fn pipes<'a>(
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

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bad_response() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid preview helper message")
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

pub(super) struct Timed<'a, T> {
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
            // Short waits allow the parent to stop a preview operation when the
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
    fn helper_pipe_observes_timeout_and_cancellation() {
        // The shell builtin waits on the pipe and creates no descendants.
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "IFS= read -r unused"]);
        let mut helper = Process::spawn(&mut command).unwrap();
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
