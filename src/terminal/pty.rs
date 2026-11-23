use super::embedded::session::Size;
use std::ffi::{CStr, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, process::CommandExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

// Darwin <sys/ttycom.h>. libc does not export these constants on macOS.
const TIOCPTYGNAME: libc::c_ulong = 0x4080_7453;
const TIOCSCTTY: libc::c_ulong = 0x2000_7461;
const TIOCSWINSZ: libc::c_ulong = 0x8008_7467;
const HANGUP_GRACE: Duration = Duration::from_millis(180);

#[derive(Debug)]
pub(crate) enum PtySpawnError {
    Setup(io::Error),
    Process { program: PathBuf, source: io::Error },
}

impl From<io::Error> for PtySpawnError {
    fn from(error: io::Error) -> Self {
        Self::Setup(error)
    }
}

pub(super) struct Pty {
    master: Option<File>,
    child: Child,
}

impl Pty {
    pub(super) fn spawn(mut command: Command, size: Size) -> Result<Self, PtySpawnError> {
        // OpenOptions marks both descriptors close-on-exec at open, including
        // while other PawCommander workers concurrently launch helpers.
        let master = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open("/dev/ptmx")?;
        let fd = master.as_raw_fd();
        let mut name = [0_u8; 128];
        // SAFETY: fd belongs to the live PTY controller, and the name buffer
        // is the 128-byte writable buffer required by TIOCPTYGNAME.
        unsafe {
            checked(libc::grantpt(fd))?;
            checked(libc::unlockpt(fd))?;
            checked(libc::ioctl(fd, TIOCPTYGNAME, name.as_mut_ptr()))?;
        }
        let name = CStr::from_bytes_until_nul(&name).map_err(io::Error::other)?;
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(OsStr::from_bytes(name.to_bytes()))?;
        resize(&master, size)?;
        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe system calls are used after fork.
        // Command has already duplicated the slave onto standard input.
        unsafe {
            command.pre_exec(|| {
                checked(libc::setsid())?;
                checked(libc::ioctl(libc::STDIN_FILENO, TIOCSCTTY, 0))?;
                for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
                    libc::signal(signal, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|source| PtySpawnError::Process {
            program: command.get_program().into(),
            source,
        })?;
        Ok(Self {
            master: Some(master),
            child,
        })
    }

    pub(super) fn master(&mut self) -> &mut File {
        self.master
            .as_mut()
            .expect("PTY open while the worker owns it")
    }

    pub(super) fn resize(&mut self, size: Size) -> io::Result<()> {
        resize(self.master(), size)
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let session = self.child.id() as libc::pid_t;
        // SAFETY: the live controller supplies the current foreground group.
        let foreground = self
            .master
            .as_ref()
            .map(|fd| unsafe { libc::tcgetpgrp(fd.as_raw_fd()) });
        if let Some(group) =
            foreground.filter(|&group| group != session && owns_group(group, session))
        {
            signal_group(group, libc::SIGHUP);
        }
        signal_group(session, libc::SIGHUP);
        drop(self.master.take());

        let deadline = Instant::now() + HANGUP_GRACE;
        while Instant::now() < deadline {
            // Keep the session leader unreaped while a separate foreground
            // group still needs cleanup, so its session ID stays pinned.
            let foreground_alive =
                foreground.is_some_and(|group| group != session && owns_group(group, session));
            if !foreground_alive && matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(group) =
            foreground.filter(|&group| group != session && owns_group(group, session))
        {
            signal_group(group, libc::SIGKILL);
        }
        // The unreaped session leader pins this PID until wait().
        signal_group(session, libc::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn owns_group(group: libc::pid_t, session: libc::pid_t) -> bool {
    // SAFETY: getsid inspects the process group leader without mutating it.
    group > 1 && unsafe { libc::getsid(group) } == session
}

fn signal_group(group: libc::pid_t, signal: libc::c_int) {
    if group > 1 {
        // SAFETY: the caller selected a group in this PTY's child session.
        unsafe {
            libc::kill(-group, signal);
        }
    }
}

fn resize(master: &File, size: Size) -> io::Result<()> {
    let window = libc::winsize {
        ws_row: size.rows,
        ws_col: size.columns,
        ws_xpixel: size.columns.saturating_mul(size.cell_width),
        ws_ypixel: size.rows.saturating_mul(size.cell_height),
    };
    // SAFETY: window lives for the ioctl, and the descriptor is a live PTY.
    checked(unsafe { libc::ioctl(master.as_raw_fd(), TIOCSWINSZ, &window) })
}

fn checked(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
