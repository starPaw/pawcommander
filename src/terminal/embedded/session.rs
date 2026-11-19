use super::{palette, Location};
use crate::terminal::{macos::ssh_command, pty::Pty};
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Osc52};
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::Term;
use eframe::egui;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(super) const MAX_PENDING_INPUT: usize = 1024 * 1024;
const EXIT_POLL_MS: libc::c_int = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::terminal) struct Size {
    pub rows: u16,
    pub columns: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Default for Size {
    fn default() -> Self {
        Self {
            rows: 24,
            columns: 80,
            cell_width: 8,
            cell_height: 16,
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }

    fn columns(&self) -> usize {
        usize::from(self.columns)
    }
}

impl From<Size> for WindowSize {
    fn from(size: Size) -> Self {
        Self {
            num_lines: size.rows,
            num_cols: size.columns,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

pub(super) enum State {
    Starting,
    Running,
    Exited(ExitStatus),
    Stopped,
    Failed(String),
}

impl State {
    pub(super) fn finished(&self) -> bool {
        !matches!(self, Self::Starting | Self::Running)
    }
}

#[derive(Clone)]
pub(super) struct Events {
    control: Arc<Control>,
    ctx: egui::Context,
}

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(bytes) => {
                self.control.enqueue(bytes.as_bytes());
            }
            Event::ColorRequest(index, format) => {
                self.control
                    .enqueue(format(palette::base(index)).as_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let size = self.control.requests.lock().unwrap().size;
                self.control.enqueue(format(size.into()).as_bytes());
            }
            Event::Wakeup => self.ctx.request_repaint(),
            _ => {}
        }
    }
}

type Terminal = Arc<FairMutex<Term<Events>>>;

pub(super) struct Session {
    pub(super) terminal: Terminal,
    pub(super) state: State,
    worker: Option<JoinHandle<()>>,
    states: mpsc::Receiver<State>,
    control: Arc<Control>,
    requested_size: Size,
}

impl Session {
    pub(super) fn start(folder: Location, ctx: &egui::Context) -> io::Result<Self> {
        Self::start_with(ctx, move || command(&folder))
    }

    pub(super) fn start_with(
        ctx: &egui::Context,
        start: impl FnOnce() -> Result<Command, String> + Send + 'static,
    ) -> io::Result<Self> {
        let (wake, read_wake) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        read_wake.set_nonblocking(true)?;
        let control = Arc::new(Control {
            requests: Mutex::default(),
            stop: AtomicBool::default(),
            wake,
        });
        let requested_size = Size::default();
        let terminal = Arc::new(FairMutex::new(Term::new(
            Config {
                scrolling_history: 4_000,
                osc52: Osc52::Disabled,
                ..Config::default()
            },
            &requested_size,
            Events {
                control: control.clone(),
                ctx: ctx.clone(),
            },
        )));
        let (send, states) = mpsc::channel();
        let worker_terminal = terminal.clone();
        let worker_control = control.clone();
        let ctx = ctx.clone();
        let worker = std::thread::Builder::new()
            .name("pawcommander-pty".into())
            .spawn(move || {
                let state = (|| {
                    if worker_control.stopped() {
                        return Ok(State::Stopped);
                    }
                    let mut command = start()?;
                    command
                        .env("TERM", "xterm-256color")
                        .env("COLORTERM", "truecolor")
                        .env("TERM_PROGRAM", "PawCommander");
                    let mut pty =
                        Pty::spawn(command, requested_size).map_err(|error| error.to_string())?;
                    let _ = send.send(State::Running);
                    ctx.request_repaint();
                    run(&mut pty, read_wake, &worker_control, &worker_terminal, &ctx)
                        .map_err(|error| error.to_string())
                })()
                .unwrap_or_else(State::Failed);
                let _ = send.send(state);
                ctx.request_repaint();
            })?;
        Ok(Self {
            terminal,
            state: State::Starting,
            worker: Some(worker),
            states,
            control,
            requested_size,
        })
    }

    pub(super) fn poll(&mut self) {
        loop {
            match self.states.try_recv() {
                Ok(state) => self.state = state,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.state.finished() {
                        self.state =
                            State::Failed(crate::i18n::text("Сеанс терминала прервался.").into());
                    }
                    break;
                }
            }
        }
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            let _ = self.worker.take().unwrap().join();
        }
    }

    pub(super) fn input(&self, bytes: &[u8]) -> bool {
        self.control.enqueue(bytes)
    }

    pub(super) fn resize(&mut self, size: Size) {
        if size == self.requested_size {
            return;
        }
        self.requested_size = size;
        if self.state.finished() {
            self.terminal.lock().resize(size);
        } else {
            self.control.requests.lock().unwrap().resize = Some(size);
            self.control.wake();
        }
    }

    pub(super) fn stop(&self) {
        self.control.stop.store(true, Ordering::Release);
        self.control.wake();
    }

    pub(super) fn shutdown(&mut self) {
        self.stop();
        // Application exit waits briefly so the worker can close and reap the
        // PTY child before this process exits. UI actions never join the worker.
        let deadline = Instant::now() + Duration::from_millis(450);
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.poll();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

fn command(folder: &Location) -> Result<Command, String> {
    match folder {
        Location::Local(path) => {
            let shell = std::env::var_os("SHELL")
                .filter(|value| Path::new(value).is_absolute())
                .unwrap_or_else(|| "/bin/zsh".into());
            let mut shell = Command::new(shell);
            shell.arg("-i").current_dir(path);
            // A GUI launch can lack the locale Terminal.app normally sets up.
            if ["LC_ALL", "LC_CTYPE", "LANG"]
                .iter()
                .all(|name| std::env::var_os(name).is_none_or(|value| value.is_empty()))
            {
                shell.env("LC_CTYPE", "UTF-8");
            }
            Ok(shell)
        }
        Location::Sftp { endpoint, path } => ssh_command(endpoint, path),
        Location::Archive { .. } => Err(crate::archive::readonly().into()),
    }
}

#[derive(Default)]
struct Requests {
    input: VecDeque<u8>,
    resize: Option<Size>,
    size: Size,
}

struct Control {
    requests: Mutex<Requests>,
    stop: AtomicBool,
    wake: UnixStream,
}

impl Control {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn wake(&self) {
        let _ = (&self.wake).write(&[1]);
    }

    fn enqueue(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        let mut requests = self.requests.lock().unwrap();
        if self.stopped() || bytes.len() > MAX_PENDING_INPUT.saturating_sub(requests.input.len()) {
            return false;
        }
        requests.input.extend(bytes);
        drop(requests);
        self.wake();
        true
    }
}

fn run(
    pty: &mut Pty,
    mut wake: UnixStream,
    control: &Control,
    terminal: &Terminal,
    ctx: &egui::Context,
) -> io::Result<State> {
    let mut parser = Processor::new();
    let mut readable = true;
    while !control.stopped() {
        if let Some(status) = pty.try_wait()? {
            if readable {
                read_pty(pty, terminal, &mut parser, ctx)?;
            }
            parser.stop_sync(&mut *terminal.lock());
            return Ok(State::Exited(status));
        }
        let resize = control.requests.lock().unwrap().resize.take();
        if let Some(size) = resize {
            pty.resize(size)?;
            control.requests.lock().unwrap().size = size;
            terminal.lock().resize(size);
            ctx.request_repaint();
        }
        if parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            parser.stop_sync(&mut *terminal.lock());
            ctx.request_repaint();
        }
        let writable = !control.requests.lock().unwrap().input.is_empty();
        let mut descriptors = [
            libc::pollfd {
                fd: if readable {
                    pty.master().as_raw_fd()
                } else {
                    -1
                },
                events: libc::POLLIN | if writable { libc::POLLOUT } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both descriptors live through poll; the array length is exact.
        if unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as _,
                EXIT_POLL_MS,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if control.stopped() {
            break;
        }
        if descriptors[1].revents & libc::POLLIN != 0 {
            let _ = wake.read(&mut [0; 256]);
        }
        let events = descriptors[0].revents;
        if events & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            readable = read_pty(pty, terminal, &mut parser, ctx)?;
        }
        if readable && events & libc::POLLOUT != 0 {
            let mut requests = control.requests.lock().unwrap();
            match pty.master().write(requests.input.as_slices().0) {
                Ok(count) => {
                    requests.input.drain(..count);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) if error.raw_os_error() == Some(libc::EIO) => readable = false,
                Err(error) => return Err(error),
            }
        }
    }
    Ok(State::Stopped)
}

fn read_pty(
    pty: &mut Pty,
    terminal: &Terminal,
    parser: &mut Processor,
    ctx: &egui::Context,
) -> io::Result<bool> {
    let mut buffer = [0; 16 * 1024];
    for _ in 0..4 {
        match pty.master().read(&mut buffer) {
            Ok(0) => return Ok(false),
            Ok(count) => {
                parser.advance(&mut *terminal.lock(), &buffer[..count]);
                ctx.request_repaint();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}
