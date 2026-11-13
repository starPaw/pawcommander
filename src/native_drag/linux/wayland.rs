//! A private event queue on winit's existing libwayland display. A second
//! pointer observes the genuine press serial; a new display connection would
//! not own the surface/implicit grab and cannot legally initiate this drag.
use eframe::egui;
use std::{
    collections::HashMap,
    ffi::c_void,
    io::{self, Write},
    os::fd::{AsFd, AsRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    backend::{Backend, ObjectId},
    protocol::{
        wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_pointer,
        wl_registry, wl_seat, wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};

pub struct Client {
    commands: mpsc::Sender<Vec<u8>>,
    results: mpsc::Receiver<Result<bool, String>>,
    active: bool,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Client {
    /// The borrowed display/surface must outlive this client (including Drop).
    pub unsafe fn new(
        display: *mut c_void,
        surface: *mut c_void,
        ctx: egui::Context,
    ) -> Result<Self, String> {
        let (commands, receive) = mpsc::channel();
        let (send, results) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let display = display as usize;
        let surface = surface as usize;
        let worker = thread::Builder::new()
            .name("native-wayland-dnd".into())
            .spawn(move || {
                // SAFETY: Client::drop joins this worker before eframe destroys the
                // borrowed handles. Libwayland supports independent event queues.
                let result =
                    unsafe { run(display, surface, receive, send.clone(), &stopped, &ctx) };
                if let Err(error) = result {
                    let _ = send.send(Err(error));
                    ctx.request_repaint();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            commands,
            results,
            active: false,
            stop,
            worker: Some(worker),
        })
    }

    pub fn start(&mut self, uris: Vec<u8>) -> Result<(), String> {
        if let Ok(Err(error)) = self.results.try_recv() {
            return Err(error);
        }
        self.commands
            .send(uris)
            .map_err(|_| "Wayland drag worker is unavailable".to_owned())?;
        self.active = true;
        Ok(())
    }

    pub fn poll(&mut self) -> Option<Result<bool, String>> {
        if !self.active {
            return None;
        }
        let result = match self.results.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err("Wayland drag worker stopped".into()),
        };
        self.active = false;
        Some(result)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Seat {
    seat: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
    focused: bool,
    pressed: Option<u32>,
}

struct Active {
    source: wl_data_source::WlDataSource,
    device: wl_data_device::WlDataDevice,
    bytes: Arc<Vec<u8>>,
    writes: Arc<Writes>,
    action: wl_data_device_manager::DndAction,
    dropped: Option<Instant>,
    own_target: bool,
    self_drop: bool,
    finished: bool,
}

#[derive(Default)]
struct Writes {
    pending: AtomicUsize,
    requested: AtomicBool,
    failed: AtomicBool,
}

struct State {
    origin: wl_surface::WlSurface,
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    seats: HashMap<u32, Seat>,
    active: Option<Active>,
    offers: Vec<wl_data_offer::WlDataOffer>,
    results: mpsc::Sender<Result<bool, String>>,
    ctx: egui::Context,
}

impl State {
    fn start(&mut self, bytes: Vec<u8>, qh: &QueueHandle<Self>) -> Result<(), String> {
        let manager = self
            .manager
            .as_ref()
            .filter(|m| m.version() >= 3)
            .ok_or("The compositor does not expose wl_data_device_manager v3")?;
        let seat = self
            .seats
            .values()
            .find(|seat| seat.focused && seat.pressed.is_some())
            .ok_or("The drag has no active pointer press on this Wayland surface")?;
        let source = manager.create_data_source(qh, ());
        source.offer("text/uri-list".into());
        source.set_actions(wl_data_device_manager::DndAction::Copy);
        // Do not keep a second receiving data device while idle: incoming drops
        // remain exclusively owned by winit's normal integration.
        let device = manager.get_data_device(&seat.seat, qh, ());
        device.start_drag(Some(&source), &self.origin, None, seat.pressed.unwrap());
        self.active = Some(Active {
            source,
            device,
            bytes: Arc::new(bytes),
            writes: Arc::new(Writes::default()),
            action: wl_data_device_manager::DndAction::empty(),
            dropped: None,
            own_target: false,
            self_drop: false,
            finished: false,
        });
        Ok(())
    }

    fn complete(&mut self, result: Result<bool, String>) {
        if let Some(active) = self.active.take() {
            active.source.destroy();
            active.device.release();
        }
        for offer in self.offers.drain(..) {
            if offer.is_alive() {
                offer.destroy();
            }
        }
        for seat in self.seats.values_mut() {
            seat.pressed = None;
        }
        let _ = self.results.send(result);
        self.ctx.request_repaint();
    }

    fn finish_writes(&mut self) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if !active.finished || active.writes.pending.load(Ordering::Acquire) != 0 {
            return;
        }
        let result = if active.writes.failed.load(Ordering::Acquire)
            || !active.writes.requested.load(Ordering::Acquire)
        {
            Err("Could not send the dragged file list".into())
        } else {
            Ok(active.dropped.is_some()
                && active.action == wl_data_device_manager::DndAction::Copy
                && !active.self_drop)
        };
        self.complete(result);
    }

    fn cleanup(&mut self) {
        if self.active.is_some() {
            self.complete(Ok(false));
        }
        for (_, seat) in self.seats.drain() {
            if let Some(pointer) = seat.pointer {
                pointer.release();
            }
            seat.seat.release();
        }
    }
}

unsafe fn run(
    display: usize,
    surface: usize,
    commands: mpsc::Receiver<Vec<u8>>,
    results: mpsc::Sender<Result<bool, String>>,
    stop: &AtomicBool,
    ctx: &egui::Context,
) -> Result<(), String> {
    // SAFETY: the enclosing Client retains/join-guards these borrowed handles.
    let backend = unsafe { Backend::from_foreign_display(display as *mut _) };
    let conn = Connection::from_backend(backend);
    // This is a borrowed foreign proxy: never send wl_surface.destroy for it.
    let id = unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface as *mut _) }
        .map_err(|e| e.to_string())?;
    let origin = wl_surface::WlSurface::from_id(&conn, id).map_err(|e| e.to_string())?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let mut state = State {
        origin,
        manager: None,
        seats: HashMap::new(),
        active: None,
        offers: Vec::new(),
        results,
        ctx: ctx.clone(),
    };
    let result = (|| -> Result<(), String> {
        while !stop.load(Ordering::Acquire) {
            queue
                .dispatch_pending(&mut state)
                .map_err(|e| e.to_string())?;
            state.finish_writes();
            while let Ok(bytes) = commands.try_recv() {
                if state.active.is_some() {
                    continue;
                }
                if let Err(error) = state.start(bytes, &qh) {
                    state.complete(Err(error));
                }
            }
            if state
                .active
                .as_ref()
                .and_then(|a| a.dropped)
                .is_some_and(|at| at.elapsed() > Duration::from_secs(10))
            {
                state.complete(Err(
                    "The drop target did not finish the Wayland transfer".into()
                ));
            }
            conn.flush().map_err(|e| e.to_string())?;
            if let Some(read) = queue.prepare_read() {
                let mut fd = libc::pollfd {
                    fd: conn.as_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: the borrowed connection fd remains live; the short
                // poll bounds shutdown and leaves winit's queue untouched.
                let ready = unsafe { libc::poll(&mut fd, 1, 12) };
                if ready > 0 {
                    if let Err(error) = read.read()
                        && !matches!(&error, wayland_client::backend::WaylandError::Io(e) if e.kind() == io::ErrorKind::WouldBlock)
                    {
                        return Err(error.to_string());
                    }
                } else if ready < 0
                    && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                {
                    return Err(io::Error::last_os_error().to_string());
                }
            }
        }
        Ok(())
    })();
    state.cleanup();
    // These globals have no protocol destructor, so release only our proxies.
    if let Some(manager) = state.manager.take() {
        let _ = conn.backend().destroy_object(&manager.id());
    }
    let _ = conn.backend().destroy_object(&registry.id());
    let _ = conn.flush();
    result
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_data_device_manager" => {
                state.manager = Some(registry.bind(name, version.min(3), qh, ()));
            }
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_seat" && version >= 5 => {
                state.seats.insert(
                    name,
                    Seat {
                        seat: registry.bind(name, version.min(7), qh, name),
                        pointer: None,
                        focused: false,
                        pressed: None,
                    },
                );
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(seat) = state.seats.remove(&name) {
                    if let Some(pointer) = seat.pointer {
                        pointer.release();
                    }
                    seat.seat.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
            && let Some(seat) = state.seats.get_mut(name)
        {
            if capabilities.contains(wl_seat::Capability::Pointer) && seat.pointer.is_none() {
                seat.pointer = Some(proxy.get_pointer(qh, *name));
            } else if !capabilities.contains(wl_seat::Capability::Pointer) {
                if let Some(pointer) = seat.pointer.take() {
                    pointer.release();
                }
                seat.pressed = None;
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(seat) = state.seats.get_mut(name) else {
            return;
        };
        match event {
            wl_pointer::Event::Enter { surface, .. } => {
                seat.focused = surface.id() == state.origin.id();
            }
            wl_pointer::Event::Leave { .. } => {
                seat.focused = false;
            }
            wl_pointer::Event::Button {
                serial,
                button: 0x110,
                state: WEnum::Value(button),
                ..
            } => {
                seat.pressed = if seat.focused && button == wl_pointer::ButtonState::Pressed {
                    Some(serial)
                } else {
                    None
                };
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(active) = state.active.as_mut().filter(|a| &a.source == source) else {
            return;
        };
        match event {
            wl_data_source::Event::Send { mime_type, fd } if mime_type == "text/uri-list" => {
                let bytes = active.bytes.clone();
                let writes = active.writes.clone();
                writes.requested.store(true, Ordering::Release);
                writes.pending.fetch_add(1, Ordering::AcqRel);
                let result =
                    thread::Builder::new()
                        .name("native-dnd-data".into())
                        .spawn(move || {
                            if write_pipe(fd, &bytes).is_err() {
                                writes.failed.store(true, Ordering::Release);
                            }
                            writes.pending.fetch_sub(1, Ordering::AcqRel);
                        });
                if result.is_err() {
                    active.writes.failed.store(true, Ordering::Release);
                    active.writes.pending.fetch_sub(1, Ordering::AcqRel);
                }
            }
            wl_data_source::Event::Action {
                dnd_action: WEnum::Value(action),
            } => {
                active.action = action;
            }
            wl_data_source::Event::DndDropPerformed => {
                active.dropped = Some(Instant::now());
            }
            wl_data_source::Event::DndFinished => {
                // The receiver can finish just before the writer reports EOF.
                // Do not acknowledge success until every requested pipe is done.
                active.finished = true;
            }
            wl_data_source::Event::Cancelled => state.complete(Ok(false)),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => state.offers.push(id),
            wl_data_device::Event::Enter { surface, .. } => {
                if let Some(active) = state.active.as_mut() {
                    active.own_target = surface.id() == state.origin.id();
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(active) = state.active.as_mut() {
                    active.own_target = false;
                }
            }
            wl_data_device::Event::Drop => {
                if let Some(active) = state.active.as_mut() {
                    active.self_drop = active.own_target;
                }
            }
            _ => {}
        }
    }
    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        _: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);

fn write_pipe(fd: OwnedFd, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::File::from(fd);
    // SAFETY: this is our owned pipe; changing its flags does not affect winit.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut offset = 0;
    while offset < bytes.len() {
        match file.write(&bytes[offset..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => offset += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                let mut poll = libc::pollfd {
                    fd: file.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: live owned pipe; bounded wait on a worker thread.
                unsafe {
                    libc::poll(&mut poll, 1, remaining.as_millis().min(100) as i32);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::unix::net::UnixStream};

    #[test]
    fn file_list_pipe_handles_backpressure_and_a_disconnected_receiver() {
        let bytes: Vec<u8> = (0..1024 * 1024).map(|n| (n % 251) as u8).collect();
        let (sender, mut receiver) = UnixStream::pair().unwrap();
        let reader = thread::spawn(move || {
            let mut received = Vec::new();
            receiver.read_to_end(&mut received).unwrap();
            received
        });
        write_pipe(sender.into(), &bytes).unwrap();
        assert_eq!(reader.join().unwrap(), bytes);

        let (sender, receiver) = UnixStream::pair().unwrap();
        drop(receiver);
        assert!(write_pipe(sender.into(), b"file:///tmp/disposable\r\n").is_err());
    }
}
