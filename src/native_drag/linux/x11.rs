//! XDND source on a separate connection/worker. Only the initial ungrab touches
//! winit's Xlib display, on the UI thread; no event is consumed from its queue.
use eframe::egui;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{xproto::*, Event},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
    CURRENT_TIME, NONE,
};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        XdndAware, XdndProxy, XdndEnter, XdndLeave, XdndPosition, XdndStatus,
        XdndDrop, XdndFinished, XdndSelection, XdndActionCopy, XdndActionList,
        TARGETS, TIMESTAMP, INCR,
        uri_list: b"text/uri-list",
    }
}

pub struct Drag {
    stop: Arc<AtomicBool>,
    result: mpsc::Receiver<Result<bool, String>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drag {
    pub fn start(
        display: usize,
        origin: Window,
        uris: Vec<u8>,
        ctx: egui::Context,
    ) -> Result<Self, String> {
        let xlib = x11_dl::xlib::Xlib::open().map_err(|e| e.to_string())?;
        // SAFETY: this display is borrowed from the live eframe frame. Release
        // winit's implicit press grab before the source worker takes over.
        unsafe {
            (xlib.XUngrabPointer)(display as *mut _, CURRENT_TIME.into());
            (xlib.XFlush)(display as *mut _);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (send, result) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("native-xdnd".into())
            .spawn(move || {
                let result = run(origin, uris, &stopped).map_err(|e| e.to_string());
                let _ = send.send(result);
                ctx.request_repaint();
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            stop,
            result,
            worker: Some(worker),
        })
    }

    pub fn poll(&mut self) -> Option<Result<bool, String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("XDND worker stopped".into())),
        }
    }
}

impl Drop for Drag {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

type Error = Box<dyn std::error::Error + Send + Sync>;
#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    window: Window,
    proxy: Window,
    version: u32,
}
struct Incremental {
    data: Vec<u8>,
    offset: usize,
    target: Atom,
}

fn message(conn: &RustConnection, target: Target, atom: Atom, data: [u32; 5]) -> Result<(), Error> {
    conn.send_event(
        false,
        target.proxy,
        EventMask::NO_EVENT,
        ClientMessageEvent::new(32, target.window, atom, data),
    )?;
    Ok(())
}

fn property(conn: &RustConnection, window: Window, atom: Atom, kind: AtomEnum) -> Option<u32> {
    conn.get_property(false, window, atom, kind, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()
}

fn cursors(conn: &RustConnection) -> Result<[Cursor; 2], Error> {
    let font = conn.generate_id()?;
    conn.open_font(font, b"cursor")?.check()?;
    let mut cursors = [0; 2];
    // Core X cursor glyphs: X_cursor (rejected), plus (copy). Each has a mask
    // in the following glyph; both resources belong to this private connection.
    for (cursor, glyph) in cursors.iter_mut().zip([0, 90]) {
        *cursor = conn.generate_id()?;
        conn.create_glyph_cursor(
            *cursor,
            font,
            font,
            glyph,
            glyph + 1,
            0,
            0,
            0,
            u16::MAX,
            u16::MAX,
            u16::MAX,
        )?;
    }
    conn.close_font(font)?;
    Ok(cursors)
}

fn target_at(
    conn: &RustConnection,
    root: Window,
    origin: Window,
    atoms: &Atoms,
) -> Result<Option<Target>, Error> {
    let mut window = root;
    let mut target = None;
    for _ in 0..64 {
        if window == origin {
            return Ok(None);
        }
        if let Some(version) = property(conn, window, atoms.XdndAware, AtomEnum::ATOM)
            && version >= 3
        {
            let proxy = property(conn, window, atoms.XdndProxy, AtomEnum::WINDOW)
                .filter(|proxy| {
                    property(conn, *proxy, atoms.XdndProxy, AtomEnum::WINDOW) == Some(*proxy)
                })
                .unwrap_or(window);
            target = Some(Target {
                window,
                proxy,
                version: version.min(5),
            });
        }
        let pointer = conn.query_pointer(window)?.reply()?;
        if pointer.child == NONE {
            break;
        }
        window = pointer.child;
    }
    Ok(target)
}

fn run(origin: Window, uris: Vec<u8>, stop: &AtomicBool) -> Result<bool, Error> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let atoms = Atoms::new(&conn)?.reply()?;
    let source = conn.generate_id()?;
    conn.create_window(
        0,
        source,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        source,
        atoms.XdndActionList,
        AtomEnum::ATOM,
        &[atoms.XdndActionCopy],
    )?;
    conn.set_selection_owner(source, atoms.XdndSelection, CURRENT_TIME)?;
    let cursors = cursors(&conn)?;
    let mut shown_cursor = cursors[0];
    let grabbed = conn
        .grab_pointer(
            false,
            root,
            EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            NONE,
            cursors[0],
            CURRENT_TIME,
        )?
        .reply()?;
    if grabbed.status != GrabStatus::SUCCESS {
        return Err("Could not acquire the drag pointer".into());
    }
    let keyboard = conn
        .grab_keyboard(false, root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
        .reply()?;
    if keyboard.status != GrabStatus::SUCCESS {
        return Err("Could not acquire Escape cancellation".into());
    }
    let keymap = conn
        .get_keyboard_mapping(
            conn.setup().min_keycode,
            conn.setup().max_keycode - conn.setup().min_keycode + 1,
        )?
        .reply()?;
    let escape: Vec<_> = keymap
        .keysyms
        .chunks(usize::from(keymap.keysyms_per_keycode))
        .enumerate()
        .filter(|(_, symbols)| symbols.contains(&0xff1b))
        .map(|(i, _)| conn.setup().min_keycode + i as u8)
        .collect();
    let mut target = None;
    let mut accepted = false;
    let mut pending = false;
    let mut sent_position = None;
    let mut released = None;
    let mut dropped = None;
    let mut supplied = false;
    let mut transfers: HashMap<(Window, Atom), Incremental> = HashMap::new();
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::KeyPress(key) if escape.contains(&key.detail) && dropped.is_none() => {
                    if let Some(t) = target {
                        message(&conn, t, atoms.XdndLeave, [source, 0, 0, 0, 0])?;
                    }
                    conn.flush()?;
                    return Ok(false);
                }
                Event::ClientMessage(event) if event.type_ == atoms.XdndStatus => {
                    let data = event.data.as_data32();
                    if target.is_some_and(|t: Target| t.window == data[0]) {
                        accepted = data[1] & 1 != 0 && data[4] == atoms.XdndActionCopy;
                        pending = false;
                    }
                }
                Event::ClientMessage(event)
                    if event.type_ == atoms.XdndFinished && dropped.is_some() =>
                {
                    let data = event.data.as_data32();
                    if let Some(t) = target
                        && t.window == data[0]
                    {
                        return Ok(supplied
                            && (t.version < 5
                                || (data[1] & 1 != 0 && data[2] == atoms.XdndActionCopy)));
                    }
                }
                Event::SelectionRequest(request) if request.selection == atoms.XdndSelection => {
                    let prop = if request.property == NONE {
                        request.target
                    } else {
                        request.property
                    };
                    let mut reply = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: request.time,
                        requestor: request.requestor,
                        selection: request.selection,
                        target: request.target,
                        property: NONE,
                    };
                    if request.target == atoms.TARGETS {
                        conn.change_property32(
                            PropMode::REPLACE,
                            request.requestor,
                            prop,
                            AtomEnum::ATOM,
                            &[atoms.TARGETS, atoms.TIMESTAMP, atoms.uri_list],
                        )?;
                        reply.property = prop;
                    } else if request.target == atoms.TIMESTAMP {
                        conn.change_property32(
                            PropMode::REPLACE,
                            request.requestor,
                            prop,
                            AtomEnum::INTEGER,
                            &[CURRENT_TIME],
                        )?;
                        reply.property = prop;
                    } else if request.target == atoms.uri_list {
                        if uris.len() <= 64 * 1024 {
                            conn.change_property8(
                                PropMode::REPLACE,
                                request.requestor,
                                prop,
                                atoms.uri_list,
                                &uris,
                            )?;
                        } else if transfers.len() < 8 {
                            conn.change_window_attributes(
                                request.requestor,
                                &ChangeWindowAttributesAux::new()
                                    .event_mask(EventMask::PROPERTY_CHANGE),
                            )?;
                            conn.change_property32(
                                PropMode::REPLACE,
                                request.requestor,
                                prop,
                                atoms.INCR,
                                &[uris.len() as u32],
                            )?;
                            transfers.insert(
                                (request.requestor, prop),
                                Incremental {
                                    data: uris.clone(),
                                    offset: 0,
                                    target: atoms.uri_list,
                                },
                            );
                        } else {
                            continue;
                        }
                        supplied = true;
                        reply.property = prop;
                    }
                    conn.send_event(false, request.requestor, EventMask::NO_EVENT, reply)?;
                }
                Event::PropertyNotify(event) if event.state == Property::DELETE => {
                    if let Some(transfer) = transfers.get_mut(&(event.window, event.atom)) {
                        let end = (transfer.offset + 64 * 1024).min(transfer.data.len());
                        conn.change_property8(
                            PropMode::REPLACE,
                            event.window,
                            event.atom,
                            transfer.target,
                            &transfer.data[transfer.offset..end],
                        )?;
                        if transfer.offset == end {
                            transfers.remove(&(event.window, event.atom));
                        } else {
                            transfer.offset = end;
                        }
                    }
                }
                Event::SelectionClear(_) => return Ok(false),
                _ => {}
            }
        }
        if let Some(at) = dropped {
            if Instant::now().duration_since(at) > Duration::from_secs(10) {
                return Err("The drop target did not finish the transfer".into());
            }
        } else {
            let pointer = conn.query_pointer(root)?.reply()?;
            if !pointer.mask.contains(KeyButMask::BUTTON1) {
                released.get_or_insert_with(Instant::now);
            }
            let hovered = target_at(&conn, root, origin, &atoms)?;
            if hovered != target {
                if let Some(t) = target {
                    message(&conn, t, atoms.XdndLeave, [source, 0, 0, 0, 0])?;
                }
                target = hovered;
                accepted = false;
                pending = false;
                sent_position = None;
                if let Some(t) = target {
                    message(
                        &conn,
                        t,
                        atoms.XdndEnter,
                        [source, t.version << 24, atoms.uri_list, 0, 0],
                    )?;
                }
            }
            if let Some(t) = target {
                let position =
                    ((pointer.root_x as u16 as u32) << 16) | pointer.root_y as u16 as u32;
                if !pending && sent_position != Some(position) {
                    message(
                        &conn,
                        t,
                        atoms.XdndPosition,
                        [source, 0, position, CURRENT_TIME, atoms.XdndActionCopy],
                    )?;
                    sent_position = Some(position);
                    pending = true;
                }
                if released.is_some_and(|at| !pending || at.elapsed() > Duration::from_millis(500))
                {
                    if accepted && !pending {
                        message(&conn, t, atoms.XdndDrop, [source, 0, CURRENT_TIME, 0, 0])?;
                        conn.ungrab_pointer(CURRENT_TIME)?;
                        conn.ungrab_keyboard(CURRENT_TIME)?;
                        dropped = Some(Instant::now());
                    } else {
                        break;
                    }
                }
            } else if released.is_some() {
                break;
            }
        }
        if dropped.is_none() {
            let cursor = cursors[usize::from(accepted)];
            if cursor != shown_cursor {
                conn.change_active_pointer_grab(
                    cursor,
                    CURRENT_TIME,
                    EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                )?;
                shown_cursor = cursor;
            }
        }
        conn.flush()?;
        thread::sleep(Duration::from_millis(12));
    }
    if let Some(t) = target {
        message(&conn, t, atoms.XdndLeave, [source, 0, 0, 0, 0])?;
    }
    conn.flush()?;
    // Closing this private connection releases selections, grabs and windows.
    Ok(false)
}
